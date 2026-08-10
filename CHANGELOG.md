# Changelog

All notable changes to the public Trellis releases are documented here. This
project adheres to [Semantic Versioning](https://semver.org/).

## v0.2.5 — 2026-08-10

- Grunts now run on the codex CLI, defaulting to `gpt-5.6-luna`; the
  sidecar's own HTTP chat/completions arm is retired. There is no separate
  API key — each grunt authenticates from a private `CODEX_HOME`, re-seeded
  from the operator's `~/.codex/auth.json` at every attempt launch, so a
  grunt credential failure can never unauthenticate the formalization loop.
  The grunt agent runs untrusted in a bwrap sandbox under a dedicated
  `grunt` role, and the harness owns the verdict end to end: the agent's
  own success claim is discarded, the candidate body is harvested from the
  node file, scanned (banned tokens, no new top-level declarations, the
  declared-name gate), and compiled by the harness — several demonstrated
  bypasses of that gate, including an arbitrary-code-execution and an
  axiom-forgery vector, were closed in the process. A grunt that fails to
  compile retries within its wall (two rounds, carrying the compile error);
  the gate a grunt faces is local closure, matching primary work.
- The grunt queue gains a kernel lane: a standing ranked list of every
  sidecar-eligible node, refilled at each cycle boundary, that the pool
  falls back to when the reviewer's lane is empty — a free grunt idles only
  when both lanes are exhausted, and the kernel tops the lane up when the
  pool sits idle. Ranking prefers fewest prior attempts, then non-sketch,
  then shortest proof; auto-dispatch stops refilling a node once the pool
  has failed it 5 times (`max_attempts`, counted per node content, so a
  repaired node regains its allowance). The reviewer's lane is untouched:
  queueing a node means "work this first", not "work this at all".
- Pause and resume: `scripts/trellis_pause.sh` (arm / disarm / status /
  resume) turns "stopped" into a durable, reason-carrying state in
  `<runtime>/pause_request.json` that survives the supervisor being down —
  the stop sentinel remains the fire-once trigger, but the state no longer
  lives in a killable process. Resume replays `<runtime>/launch_env.json`,
  the exact environment captured at every launch, instead of guessing one,
  and refuses if the source tree the run was launched from has moved. The
  viewer drives the same state machine, renders a paused run on its own
  attention tier, and arms a pause automatically in two cases: a human gate
  left open past `gate_park_after_minutes` (default 120), and a weekly
  budget "pause run at" floor (default 5% remaining) enforced server-side.
- Build-performance guidance: `BUILD_PERFORMANCE.md`, a canonical
  build/elaboration diagnosis reference, is rendered to agents; worker and
  reviewer prompts now weigh a different approach or decomposition against
  raising heartbeat budgets when a build runs long (a raised budget is
  named as a durable cost paid on every future rebuild), and the
  incremental checker's giant-node fallback lines advise the fix keyed by
  the actual signal.
- The mandatory-LastClean reviewer mandate ships disabled. The
  `cycles_since_clean` trigger counts every checkpoint carrying any open
  blocker, so one stubborn side-node blocker could mark 40 cycles of real
  progress as a failed repair narrative and force their discard. Operators
  who want the mandate set a positive `TRELLIS_CSC_LAST_CLEAN_THRESHOLD`;
  the reviewer sees the mandate fragment exactly when the kernel will
  enforce it.
- Viewer: the Usage page states what its numbers actually are, and the
  experimental modeled rollups and the grunts failed-attempts table are
  dropped from it.
- Viewer: an update-available banner. The server checks the public repo's
  changelog every six hours and, when a newer release exists, shows a calm
  dismissible notice (per-version dismissal; deliberately outside the
  attention ladder — nothing about the run needs a human). The check never
  raises a banner on a guess: transient fetch failures keep the last good
  answer, and `TRELLIS_VIEWER_NO_UPDATE_CHECK=1` disables the outbound
  request entirely. Served at `/api/update-check.json`.
- Docs: the viewer is documented as the primary operator interface — the
  place approval gates and `NeedInput` escalations surface — in README §4
  and INSTALLATION §2e, rather than as an optional extra; README §5 gains
  the mid-run upgrade recipe (pause → drain sidecar → rebuild in place →
  restart checker → resume).
- Docs: `INSTALLATION.md` gains a system-requirements section — 32 GB RAM
  minimum (48 GB recommended), SSD strongly recommended, what needs root and
  how to proceed without it, and macOS-as-VM-host guidance — including which
  distros run the sandbox with no root at any point. **Ubuntu 26.04 LTS is
  the new recommended platform**: Trellis is validated end-to-end on it with
  zero root setup, via the shipped `bwrap-userns-restrict` profile
  (packaged `bwrap` only). Also root-free: Ubuntu 25.04+, Ubuntu 22.04,
  Mint 22 / Pop!_OS 24.04, WSL2, and the Debian/Fedora/RHEL/openSUSE/Arch
  families. Ubuntu 23.10 through 24.10 is the blocked island and keeps
  needing the one-time root sysctl.

## v0.2.4 — 2026-07-29

- A verifier revisit prompt now carries only the findings for the node it is
  verifying. The Sound and per-node Paper requests copied the whole stored
  previous-findings map into every request, and the next target is chosen
  independently of it, so a request routinely arrived framed as a revisit
  carrying the *preceding* node's finding — telling the verifier it had found
  an unsupported step in a proof it had never read. Both lanes now intersect
  that map with the request's own verify set, as Correspondence already did,
  and the revisit fragment is selected from the filtered map. Stored verdicts
  were never affected: a lane whose payload node disagrees with the request is
  rejected before it can be written.
- Reviewers can return a substantiveness Fail to the verifier during proof
  formalization. Substantiveness clause 2 reads a node against the content of
  every node importing it, while the fingerprint that re-opens the lane is
  node-local, so the repair the reviewer is directed to make — an edit to the
  importer — left the failing node's verdict stuck. Naming the blocker in
  `reset_blocker_ids` returns the node to the verifier frontier; the blocker
  itself is still retired only by a verifier pass. A Fail that a reset cannot
  move, such as one derived from a rejected deviation claim, is no longer
  offered in any phase.
- `memory_challenges` is advertised on the worker and review contracts when the
  run has active process memory. The channel was described to both roles and
  implemented end to end, but never named in the contract, so a role that found
  its evidence contradicting an entry had no field in which to say so.

## v0.2.3 — 2026-07-27

- Closure sidecar ("grunts"): a reviewer-queued pool of Lean-specialized models
  works beside the main loop, each trying to close one open proof node. A grunt
  writes the proof body only (below `-- BODY`), and a closure lands only through
  the full kernel apply sequence, complete and checker-passing; `closure_provenance`
  attributes each node to `worker` or `sidecar`. Inert unless a `sidecar` block
  in `trellis.config.json` enables it. Operator docs: README §10 and
  `SIDECAR_OPERATIONS.md`; the viewer gains a **Grunts** tab for the pool, queue,
  and attempt history.
- Sidecar liveness: one rule for the whole system (`trellis/sidecar/health.py`) —
  the daemon's own pid lock, corroborated by an exact `-m trellis.sidecar` match
  on `/proc/<pid>/cmdline` so an attempt child is never mistaken for the manager
  — surfaced as `scripts/trellis_sidecar.sh status <root>` with operator exit
  codes. `status.json` gains pid, phase, poll interval, transport suspension and
  last-pass verdict, and the reviewer's pool claims are gated on it, so a dead,
  suspended or still-bootstrapping daemon no longer reads as spare capacity.
- Drain and adopt: `<runtime>/sidecar/drain` stops assignment and exits at once,
  leaving in-flight attempts running; `slots.json` journals every assignment and
  the next daemon adopts those children, reaping whatever finished during the
  gap before its first assignment decision. Adoptions and kills are gated on
  `/proc/<pid>/cmdline` carrying the attempt id, so a zombie reads dead and a
  recycled pid is never signalled. The daemon clears both sentinels before
  bootstrap, so a forgotten `stop` file cannot kill the attempts of the next run.
- Queue entries retire on their own: a grunt gets one attempt per entry
  generation, and the spent generations are now reported to the kernel at the
  inter-cycle boundary, which removes the matching entries. The reviewer's
  capacity view no longer counts dead work.
- Sidecar retrieval: the driver's tablet and mathlib searches route through
  ripgrep with a fixed automaton engine and a subprocess timeout. The previous
  in-process regex path checked its deadline only between files, so a
  model-supplied pattern could run unbounded in an unsandboxed process; symlink
  containment now sits at the file open, and a malformed query returns a tool
  error instead of ending the attempt.
- Viewer: every "stopped, waiting for a human" state renders on one severity
  ladder. A halt marker, the fail-loud `NeedInput` gate (shown with its
  escalation reason and protocol-state source) and a routine advance or
  re-approval gate are now distinguishable without reading the text, and at a
  `NeedInput` gate sending input is the primary action while the empty approve
  is demoted. A runtime that reports no gate kind falls back to the previous
  reviewer-decision heuristic.

## v0.2.2 — 2026-07-24

- System feedback no longer halts the run by default. A burst that returns a
  non-empty `system_feedback` string now appends a record to
  `<runtime>/system_feedback_log.jsonl` and the run continues. Halting on
  system feedback is opt-in: set `system_feedback_halt: true` in
  `trellis.config.json`, or the environment variable
  `TRELLIS_SYSTEM_FEEDBACK_HALT` (which takes precedence). A halt marker left
  from a prior run is still honored regardless of the setting. The viewer
  exposes the recent feedback log at `/api/system-feedback.json`.
- Audit planning: the process-rules reference now states that Sound
  verification (including reviewer-requested re-verification) waits until every
  statement lane is clear, so audit plans sequence statement-lane repairs
  before the Sound certifications that depend on them.

## v0.2.1 — 2026-07-22

- Initial planner: every fresh run's first cycle dispatches a planning burst
  (math and both PV goal modes) that reads the manuscript and configured
  targets and writes an advisory construction plan (report + worker tasks);
  the reviewer works and dismisses the plan, workers keep decomposition
  authority.
- Coverage re-planning: while any configured paper target lacks a covering
  node, the planner re-runs on a fixed cycle cadence, assessing the live
  plan and superseding it (dismissal trail preserved). Dead once all targets
  are covered.
- Process rules: `PROCESS_RULES.md` — a capability-and-mechanism reference
  (per-role legality envelopes plus the full mechanism inventory) installed
  at the tablet-repo root by both setup scripts and consulted by every
  audit-lane scenario via a pointer fragment.
- Add-targets mode: `add_paper_targets` revives a Complete run into
  RevisionStating to state additional targets from the same paper; existing
  approvals stay byte-identical. Operator doc: `ADD_TARGETS.md`.
- Reference papers: an operator-registered registry of auxiliary papers
  (`paper/refs/<id>.tex`) that act as grounding authority for cited external
  results; workers claim them per node and the substantiveness lane verifies
  against the claimed text. Operator doc: `REFERENCE_PAPERS.md`.
- Orphan construction window: while any configured paper target has empty
  coverage, same-burst orphan rejection is waived so the statement DAG can
  be built up in layers; all-covered behavior unchanged.
- Soundness dispatch: no longer deferred until all paper targets are
  covered — a node's prose proof is verified once its own and its cited
  statements pass correspondence and substantiveness. A routed worker task
  always wins the cycle-start slot; soundness rides after the worker, one
  auto-dispatch per cycle, and kernel-scheduled soundness results are marked
  for the reviewer.
- Revision mode: statement editability is computed dynamically
  (present minus frozen), so nodes created mid-revision are repairable.
- Viewer: tablet-snapshot downloads include `paper/refs/`; README generator
  reports per-goal Decide polarity and the full assumption list.
- Setup: `normalize_paper_envs.py --map alias=canonical` handles papers
  whose `\newtheorem` declarations carry no usable title.

## v0.2.0 — 2026-07-04

- Process memory: a git-tracked, run-authored knowledge store
  (`process-memory/` in the tablet repo). Audits record refuted routes,
  constraints, and interface decisions via structured `memory_operations`
  (add / supersede / retire, tombstoned, never deleted); workers and
  reviewers file challenges that the next audit must adjudicate; entries
  render into every role's prompt. LastClean rewinds carry memory forward by
  default (`preserve_process_memory`), operator git rewinds keep plain-git
  semantics, and entries survive kernel worktree restores in rejection
  cycles. One-shot migration script for existing runs.
- Verifier findings: an `UNSOUND`/`STRUCTURAL` soundness rejection now
  enumerates every independent blocking gap, numbered, so one repair burst
  can address them all.
- Worker prompts: the active node's claimed deviations are listed with their
  `reference/` files and a read-before-working directive; the pre-`valid`
  self-audit gains a citation-surface table (each outside fact mapped to the
  cited node's statement clause).
- Retry context: an auto-retry no longer inherits the reviewer's
  fresh-context decision, so the failed burst's scratch handoff survives
  into the retry.
- Olean freshness: content-hash olean staleness detection end-to-end
  (mtime never consulted), with the olean hash folded into the kernel
  result-cache keys.
- Cleanup phase: active-node relegalization after deletions, closure
  revalidation after worker deltas, final-target deletion support, scoped
  final validation, structural-hash protection, and same-burst orphan
  deletions.
- Soundness lane: uncomputable empty passes reopen correctly; fingerprints
  backfilled for empty TeX model refs; completed proof formalization
  auto-advances.
- Program-verification (under-model) workflow: assumption slices with
  auditor adjudication, misroute guard, checkpoint workflow fixes, and a
  reproducible `examples/pv_dec2flt` seed + runbook.
- Worker handoff: the `last_invalid` WIP snapshot is also captured when a
  `valid` response is rejected by a kernel rule at apply time, so the retry
  prompt's promised snapshot always exists; orphan-cleanup attribution and
  the orphan gate now name only newly-created orphans.
- Assumptions framework (under-model): claim classes, conditioning check,
  mid-phase domain gate, assumptions-lane verdicts mirrored into reviewer
  evidence, and failed authoring bursts handed back to the enact loop.
- Operations: `trellis.sh` runs a prebuilt kernel binary when
  `TRELLIS_TRELLIS_KERNEL_CMD` is set (release-binary step loop); worker
  model A/B switch; viewer fixes (stale-cycle tag mixing, progress.json
  truncation on large repos, slimmer wire payload).

## v0.1.1 — 2026-06-12

- Challenge targets: benchmark mode for prescribed-statement problems
  (lean-eval). Deterministic importer from a problem download, kernel-enforced
  byte-exact coverage beside paper targets, and a submission exporter that
  replays the benchmark's comparator. Contract v38.
- Clearer kernel diagnostics: every review-legality rejection branch is named;
  acceptance skip notes, signature-drift, import-cycle, and empty
  next-active messages state their cause and remedy.
- FILESPEC: node auxiliaries are node-private (factor out to share a fact);
  heartbeat option placement specified by purpose.
- Audit roles: the audit's charge is to find the work or repair that puts a
  formalization on a closing route, not to weaken the verification regime.
- Per-burst tmux sessions are torn down at burst completion (previously only
  the burst window was killed, leaking an idle session per burst).

## v0.1.0

Initial public release.

First source-available release of Trellis, an agent-driven pipeline for
formalizing mathematics in Lean 4. This release establishes the public
baseline; subsequent entries will record notable changes against it.
