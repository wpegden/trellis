# Changelog

All notable changes to the public Trellis releases are documented here. This
project adheres to [Semantic Versioning](https://semver.org/).

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
