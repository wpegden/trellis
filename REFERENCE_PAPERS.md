# Reference Papers

Operator-provided additional reference papers act as grounding authority for
results the PRIMARY paper cites. The primary paper remains the sole target
authority: reference papers never produce targets. A worker claims a
registered document per node (`node_reference_grounds`, full-replacement set);
the claim feeds the node's substantiveness fingerprint
(`claimed_reference_shas`) and reopens the lane, and the substantiveness
verifier reads the claimed file as the authority for the cited result.
Reviewers may cite line ranges into a registered document via
`paper_focus_ranges[].doc` (absent `doc` = primary paper).

## Operator flow

New run:

    ./scripts/setup_repo.sh ... --reference smith2020=path/to/smith.tex:"Smith, JCTB 2020"

Existing run (repeatable per document):

1. `./scripts/add_reference_paper.sh <repo> <tex> --id smith2020 --source-id "Smith, JCTB 2020"`
   — transcodes to UTF-8 when needed, writes `paper/refs/smith2020.tex`,
   appends the `workflow.reference_papers` config entry, commits both.
2. During a stop window (sentinel-stop; state quiescent):

       trellis_runtime_cli <<< '{"action":"add_reference_paper","root":"<runtime-root>","config_path":"<repo>/trellis.config.json"}'

   Preconditions: no in-flight request, no pending worker task, no active
   human gate — any phase. Duplicate ids (spec drift) and missing/empty files
   are rejected; an identical re-add is an idempotent no-op. The action
   asserts state-registry ⊆ config-registry and mutates ONLY
   `configured_reference_papers`.
3. Restart the supervisor. Ids are immutable for the life of the run
   (`remove_reference_paper` is reject-only in v1).

## Composition with add-targets

`add_reference_paper` and `add_paper_targets` compose in either order inside
one stop window: add-targets requires a Complete state and revives it into
RevisionStating; add-reference-paper accepts any quiescent state, including
that revived one.

## Deploy note

Forward deploy is byte-safe: a claim-free substantiveness fingerprint
serializes byte-identically to the pre-feature shape, so no baseline reopens.
Rolling BACK to a pre-feature binary after claims exist mass-reopens
substantiveness (the old binary observes claim-free fingerprints that no
longer match the claim-bearing approved pins). Rewind claims first or accept
the reopen wave. During a mixed-binary window, a burst prepared by the old
binary and accepted by the new one has any reference claims rejected
fail-closed with a clear message (the prepared gate carries an empty
registry) — a one-burst transient in the safe direction.

Two config hazards (the ADD_TARGETS.md pair):

* A/B config-swap runs (`TRELLIS_AB_TEMPLATES_DIR` set): the checkpoint hook
  overwrites `trellis.config.json` from the templates every cycle, so the
  `workflow.reference_papers` entry must ALSO be added to every template
  file, or the on-disk config diverges at the next swap. The kernel action
  prints this warning when the env var is set.
* Checkpoint `git reset --hard` reverts uncommitted config edits — the git
  commit `add_reference_paper.sh` performs is mandatory, not optional. When
  the script reports the repo is not a git worktree, commit the config and
  `paper/refs/` file manually before relaunching.

## Namespaces and encoding

`paper/refs/` is the reference-paper home; `reference/` belongs to the
deviations feature — the two are unrelated. Reference files are stored UTF-8
(`add_reference_paper.sh` transcodes via iconv when needed); the
`normalize_paper_envs.py` pass is best-effort convenience only — plain TeX
passes through unchanged.
