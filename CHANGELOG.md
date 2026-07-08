# Changelog

All notable changes to the public Trellis releases are documented here. This
project adheres to [Semantic Versioning](https://semver.org/).

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
- Worker handoff: the `last_invalid` WIP snapshot is also captured when a
  `valid` response is rejected by a kernel rule at apply time, so the retry
  prompt's promised snapshot always exists; orphan-cleanup attribution and
  the orphan gate now name only newly-created orphans.
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
