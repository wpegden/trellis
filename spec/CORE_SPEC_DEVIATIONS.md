# Core Spec Deviations

`spec/SupervisorCore.tla` models the *intended* protocol semantics — the design
contract the Trellis kernel is meant to implement.  Where the kernel deviates
from that intent, the deviation is recorded here.

Each deviation has three parts:

- **Design** — what `SupervisorCore.tla` says.
- **Kernel reality** — what the kernel actually does (`kernel/src/`).
- **Disposition** — design preferred, or kernel deviation noted.

This file is the authoritative deviation tracker; refinement proofs
(`BigSpec ⇒ SupervisorCore!Spec`) treat each entry as a documented exception
the bigger spec must witness, not as a property to discharge.

Routine "kernel has more fields than design" gaps (fingerprint mirrors, lane
ids, schema versions, etc.) are implicit and not enumerated here.  Only
surprising or non-obvious deviations are recorded.

---

## 1. SoundAssessmentStatus richness

**Design.** The Soundness lane has the standard 3-valued status (`unknown`,
`pass`, `fail`) shared with Correspondence, Faithfulness, Substantiveness, and
Deviation.  Verdict-pinning, drift detection, and reviewer interactions are
modeled uniformly across the five lanes.

**Kernel reality.** The kernel maintains an 11-status `SoundAssessmentStatus`
taxonomy (`kernel/src/model.rs`) that distinguishes:

- verifier verdicts (`VerifierPass`, `VerifierFail`, `VerifierStructural`),
- reviewer pins (`ReviewerPinnedFail`),
- structural drift sentinels (`SelfEditUnknown`,
  `DepEditOnlyStaleFail`, `DepEditOnlyStalePassDeferred`),
- split-vote ambiguity (`SplitUnknown`),
- explicit sketch fail (`SketchAutoFail`).

The legacy `SoundStatus` (4-valued: Unknown/Pass/Fail/Structural) is the
"engagement view" the kernel exposes to the reviewer; the rich taxonomy is the
underlying store, used to decide whether a stale-Pass is "self-edit" or
"dep-edit" and whether a re-Sound verifier dispatch is even needed.

**Disposition.** Kernel optimization, not a design deviation.  The big spec
faithfully models the 11-status taxonomy; the small spec elides it because:

1. The reviewer's interface is the 3-valued view; the rich taxonomy is purely
   an internal kernel optimization for dispatch decisions.
2. The substantive design contract is "Pass means the verifier last said
   Pass and no relevant content has drifted since"; both views satisfy it.

Refinement obligation: the BigSpec's 11-status map must project onto the
SoundnessStatus 3-value map via the obvious collapse (anything starting with
`Verifier...` or `ReviewerAccepted` → Pass; anything ending in `Fail`,
`SplitUnknown`, or `*StalePassDeferred` → Fail/Unknown).

---

## 2. Deviation lane: trust-store machinery elided

**Design.** The Deviation lane is a first-class protocol-level lane: lane
status per node, blocker carrier on non-Pass, reviewer adjudication via
`task_blockers` / `reset_blockers`.  Worker delta may flip statuses;
verifier panel pins.

**Kernel reality.** The Deviation lane has additional machinery the design
does not include:

- **`deviationFiles` registry** — a separate per-deviation-id map tracking
  which deviation reference files exist on disk
  (`kernel/src/runtime_cli_observations.rs`).
- **Unauthorized-claim suppression** — a node that claims a deviation id not
  present in the registry triggers a Substantiveness-Pass suppression
  (the kernel actively prevents a node from passing Substantiveness while
  claiming a non-registered deviation).
- **Sticky Fail discipline** — once a deviation file's content drifts after a
  Fail verdict, the kernel pins it as a "frozen Fail" until the content is
  restored (kernel `DeviationStickyFailDiscipline` invariant).
- **`nodeDeviationClaims` map** — per-node subset of `Deviations` the node
  declares it relies on, used to determine which deviations contribute to
  which nodes' substantiveness checks.

**Disposition.** Design preferred at the protocol level.  The trust-store
mechanics are an implementation strategy: the design contract is just
"deviation lane verdicts are pinned by a verifier and adjudicated by the
reviewer; non-Pass produces a blocker."

The kernel's claim-suppression and sticky-Fail are correct refinements of
the design — they fix real soundness gaps that came up in production — but
they are not protocol-level concerns from the perspective of the contract
the kernel implements.

---

## 3. PendingTask field elision

**Design.** A pending task is a tuple of `<workerMode, taskCarriers,
authorizedNodes, activeNode>`.

**Kernel reality.** `kernel/src/model.rs::PendingTask` carries additional
fields:

- `orphan_cleanup_nodes`: BTreeSet<NodeId> — non-empty when the task is an
  orphan-cleanup burst (a worker dispatch that removes nodes not reachable
  from any configured-target's coverage closure).
- `paper_focus_ranges`: reviewer-supplied hint forwarded to the worker.
- `next_worker_context_mode`: `fresh` or `resume` — whether the next
  worker re-reads its context from scratch or carries it from the prior
  burst's scratchpad.
- `work_style_hint`: reviewer's advisory work-style string.
- `allow_new_obligations`, `must_close_active`: proof-formalization closure
  gates.
- `consumed_global_repair_grant`: Step C flag.

**Disposition.** Kernel implementation detail.  These fields are either
prompt-rendering hints (`paper_focus_ranges`, `work_style_hint`,
`next_worker_context_mode`) or kernel bookkeeping that doesn't change the
acceptance contract (`consumed_global_repair_grant` is the Step C flag —
the design's `globalRepairStep` transitions handle the lifecycle).

The closure gates `allow_new_obligations` and `must_close_active` are the
most semantically load-bearing: they affect what worker outputs are
accepted.  The small spec abstracts them into `workerMode` (CoarseRestructure
implies broader edit envelope) and the openNodes / closure-status maps.

---

## 4. CleanupAudit task-list lifecycle

**Design.** `cleanupAuditActive: BOOLEAN` (just a flag).  The lane
contributes to `globalBlockers` via the set of carriers it surfaces.  The
audit produces decisions that route to Reviewer (via `AcceptCleanupAudit`),
not a structured task list.

**Kernel reality.** The kernel maintains a structured per-task lifecycle:

- `cleanupAuditTasks: Vec<CleanupAuditTask>` with per-task `status`
  (`Pending`, `Dismissed`, `Failed`, `Completed`).
- `cleanupAuditScratchpad: String` — the audit role's persistent notes
  across multi-burst rounds.
- `cleanupAuditBurstCount: u32` — per-round burst counter (max 5 per round).
- `cleanupAuditRound: u32` — round counter (max 2 rounds per Cleanup entry).
- `cleanupConsecutiveInvalidWorkers: u32` — wedged-task escape valve.
- `cleanupActiveTask: Option<TaskId>` — the currently-dispatched task.
- `cleanupForceDone: bool` — force-Done latch on repeated failure.

The kernel enforces strict invariants over this structure: Pending →
terminal transitions are monotone, terminal status is immutable, etc.
(see `CleanupTasksShrinkMonotonic`, `CleanupTaskStatusTransitions`,
`CleanupAuditTargetsPresent` in `spec/SupervisorProtocol.tla`).

**Disposition.** Kernel implementation detail.  The structural task list is
how the kernel surfaces audit-produced work to the reviewer; the design
contract is just "the audit lane produces blocker carriers (or none) and
hands off to the reviewer or to a cleanup worker burst."

---

## 5. StuckMathAudit `audit_plan` lifecycle

**Design.** `stuckMathAuditActive: BOOLEAN`.  An audit response either
returns to the Reviewer (carrying carriers into the next reviewer cycle)
or routes to a cone-clean reset via `forceReviewAfterConeClean`.

**Kernel reality.** The kernel maintains an `audit_plan` lane:

- `audit_plan: Option<AuditPlan>` — current plan with `tasks`, `probe_paths`,
  optional `cone_clean_node`, and a `report` string.
- `superseded_audit_plan: Option<AuditPlan>` — the prior plan retained for
  audit-trail purposes when the current plan is dismissed.
- `audit_burst_retry_count: u32` — retries on Malformed (bounded by
  `STUCK_MATH_AUDIT_BURST_RETRY_LIMIT = 1`).
- `last_stuck_math_audit_dispatched_cycle: Option<u32>` — used by the
  dispatch-cooldown gate (cf.
  `stuck_math_audit_dispatch_cooldown_cycles`).
- Multiple activation triggers (`cycles_since_clean ≥ k1` plus open Soundness
  blocker, or `cycles_since_shallow_coarse_closed_count_increase ≥ k2`).

**Disposition.** Kernel detail.  The `audit_plan` retention is for the
viewer / human auditing; the design contract is the latch flag and the
cone-clean reset effect.

---

## 6. NeedInputAuditor activation taxonomy

**Design.** `needInputAuditorActive: BOOLEAN`.  Set TRUE on a reviewer
`NEED_INPUT` decision; flipped FALSE on `AcceptNeedInputAuditor`.

**Kernel reality.** The kernel folds the NeedInputAuditor lane into the
StuckMathAudit machinery via a `NeedInputAuditContext` field embedded in
`StuckMathAuditState`:

- `need_input_audit: Option<NeedInputAuditContext>` — carries the originating
  review request id, cycle, phase, active/held nodes, mode, and the
  reviewer's reason / comments.
- Activation routes through `route_need_input_to_auditor`
  (`kernel/src/engine.rs`); the auditor's response either confirms
  (`confirm_need_input = true` → HumanGate) or declines (writes an
  `audit_plan` with recovery tasks → back to Reviewer).

So the kernel reuses Stage::StuckMathAudit for both kinds of audit (the
NeedInputAuditor context distinguishes them at response-handling time);
the small spec splits them into two separate stages
(`StuckMathAudit` vs `NeedInputAuditor`) per the brief's partition
decision #3.

**Disposition.** Kernel implementation detail.  The brief decided to keep
the two lanes distinct at the protocol level; the kernel's lane-fusion is
not a design deviation, just a reuse of the audit-burst infrastructure.

The kernel-side fusion does have one consequence: a NeedInputAuditor burst
counts toward the same `audit_burst_retry_count` as an ordinary
StuckMathAudit burst.  The design doesn't model retry counters at all (see
deviation 9), so this consequence falls out by abstraction.

**GR / NeedInput mutex (subsequent note).**  Because the small spec
splits the audit lanes into two distinct stages (`StuckMathAudit` and
`NeedInputAuditor`), the mutex enforced kernel-side between
`pending_global_repair_request` and `stuck_math_audit.need_input_audit`
holds by construction here: the GR lane writes
`globalRepairStep = "request_pending"` while at `stage = StuckMathAudit`,
the NeedInputAuditor lane fires only at `stage = NeedInputAuditor`, and
the lane stages can never be co-active.  The big spec
(`SupervisorProtocol.tla`) mirrors the kernel's lane fusion and so must
carry the mutex explicitly (TypeOK clause + clears in
`ReviewNeedInputProof` and
`AcceptStuckMathAuditRetryExhaustedBackToReviewer`); no Core-side
deviation is required.

---

## 7. Live versus committed mirrors

**Design.** Single copy of structural state: `presentNodes`, `openNodes`,
`coverage`, `configuredTargets`.  Reviewer reset is modeled as
non-deterministic re-seeding (the reset is observable as a status-map flip,
not a structural rollback).

**Kernel reality.** The kernel maintains two mirrors of structural state:
`live.*` (the worktree's current state, modified by worker bursts as they
land) and `committed.*` (the last-committed checkpoint).  The reviewer's
`ResetChoice::LastCommit` and `ResetChoice::LastClean` actions roll back
live to committed.  `LastClean` additionally clears status maps and
fingerprint mirrors.

The big spec preserves the live/committed split as `presentNodes` vs
`committedPresentNodes`, `openNodes` vs `committedOpenNodes`, etc.

**Disposition.** Design intentional.  The small spec doesn't model the
live/committed split because:

1. The relevant property — "every blocker is on the *latest* observed state
   so it can be addressed" — is captured by the single tier;
2. The fingerprint-drift mechanism (kernel: approved fingerprint vs current
   fingerprint) becomes "any worker delta may flip the status back to
   unknown" non-deterministically (cf. `AcceptWorker`'s drift sub-disjuncts).

The refinement check `BigSpec ⇒ SupervisorCore!Spec` projects
`presentNodes ← live.presentNodes` and similar.  At quiescent rest points
the big spec's `QuiescentLiveEqualsCommitted` invariant guarantees the
two mirrors agree, so the projection is well-defined there; at non-quiescent
points the live mirror is what the design wants.

---

## 8. Approved vs current fingerprint distinction

**Design.** Status `pass` means the lane verdict is currently Pass.  Worker
deltas may non-deterministically drift statuses back to `unknown`.

**Kernel reality.** Each lane carries two fingerprint maps:
`*_current_fingerprints` (last observed content hash) and
`*_approved_fingerprints` (hash pinned at the verdict).  The derived
predicate `current_*_pass(n)` returns TRUE **iff** status = Pass AND
current == approved.  When a worker edits a passed node, current drifts,
`current_*_pass` flips false, blocker reappears.

**Disposition.** Design intentional.  The fingerprint mechanism is the
kernel's deterministic implementation of "any content change invalidates
the prior verdict"; the design just says "verdicts are subject to drift,
modeled as non-deterministic re-seeding."

This collapse is the largest single source of state-space contraction in
the small spec (fingerprint maps are 4 × |Nodes| variables in the big
spec).

---

## 9. Retry counters and transport-failure ladder

**Design.** Worker outcomes are non-deterministic.  `Invalid` and `Stuck`
both route to "Worker or Reviewer", abstracting the kernel's threshold
ladder.

**Kernel reality.** The kernel has at least three retry counters:

- `attempt` (work-quality retry) bounded by `proof_invalid_review_threshold`
  (default 2 for theorem-stating, 2 for proof-formalization, none for
  cleanup).
- `transport_attempt` (Bug X principled fix) bounded by
  `transport_invalid_review_threshold`.
- `consecutive_transport_failure_count` (circuit-breaker) bounded by
  `consecutive_transport_failure_halt_threshold = 5`.

The reviewer sees `retry_outcome_kind` and uses it to vary prompt
contexts and routing decisions.

**Disposition.** Design intentional.  Retry counters are policy, not
contract.  The design says "the worker may stutter or escalate"; that's
what the spec models.

This collapse is the second-largest source of state-space contraction
(no `attempt` variable, no `transport_attempt`, no
`consecutive_transport_failure_*`).

---

## 10. Reviewer override authority retirement

**Design.** Reviewer blocker partition is two-bucket: `task_blockers` and
`reset_blockers`.

**Kernel reality.** Pre-2026-06-04 the kernel admitted a three-bucket
partition with `override_blockers` (reviewer-pinned Pass).  Option C
retirement (`REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md`) retired the
authority entirely; the field is no longer accepted on reviewer responses,
and the invariant `ReviewerOverrideEmptyUnderDefault` is now true-by-
definition.

**Disposition.** Aligned.  The kernel matches the design as of
2026-06-04; the design's two-bucket partition reflects post-Option-C
reality.

The small spec doesn't model `reset_blockers` separately either, because
its only effect is to flip a status back to `unknown` (which a non-
deterministic worker drift can also do).  A future refinement extension
would model the legality scope (`reset_blockers` is theorem-stating-only)
as a structural constraint.

---

## 11. Verifier ordering invariant

**Design.** After AcceptWorker (Valid with delta), the spec transitions
non-deterministically to any verifier stage.

**Kernel reality.** The kernel enforces a strict verifier ordering:
Paper → Substantiveness → Correspondence → Soundness → Reviewer.  The
paper drain loop ensures Faithfulness and Substantiveness both clear
before moving to Correspondence/Soundness.

**Disposition.** Kernel detail.  The verifier ordering is a kernel-side
scheduling decision that the design doesn't constrain — at the protocol
level, the contract is just "all lanes have voted (or are pinned non-
Pass / Unknown) before the reviewer is engaged."

The kernel's ordering is an optimization: paper-fail precedence
prevents reviewer tasking from pinning approved fingerprints before
verifier evidence.  The small spec's non-determinism admits every
ordering, so any kernel ordering refines into it.

---

## 12. Worker mode enum vs proof_edit_mode + worker_validation_kind

**Design.** Worker mode is a 4-valued enum: `local`, `restructure`,
`coarse_restructure`, `cleanup`.

**Kernel reality.** The kernel splits the concept across two enums:

- `proof_edit_mode: ProofEditMode` (3 values: Local, Restructure,
  CoarseRestructure) — valid only in `Phase::ProofFormalization`.
- `worker_validation_kind: WorkerValidationKind` (9 values:
  None, TheoremGlobal, TheoremTargeted, ProofEasy, ProofLocal,
  ProofRestructure, ProofCoarseRestructure, Cleanup, FinalCleanup) —
  the per-burst validation contract.

The mapping from design's worker mode to kernel's per-phase enum is:

- `local` →
  TheoremStating: WorkerValidationKind::TheoremGlobal (no per-target restriction).
  ProofFormalization: WorkerValidationKind::ProofLocal (ProofEditMode::Local).
- `restructure` →
  ProofFormalization: WorkerValidationKind::ProofRestructure (ProofEditMode::Restructure).
- `coarse_restructure` →
  ProofFormalization: WorkerValidationKind::ProofCoarseRestructure (ProofEditMode::CoarseRestructure).
- `cleanup` →
  Cleanup: WorkerValidationKind::Cleanup or FinalCleanup.

**Disposition.** Design intentional collapse.  The kernel's per-phase
enum distinction is an implementation detail.  The protocol contract is
the four-valued mode; the kernel's enum split is just a refinement of the
authorization semantics by phase.

The TheoremGlobal vs TheoremTargeted distinction is not relevant at the
protocol level — both are "edit any node within the theorem-stating
scope".  The kernel's split exists for prompt rendering.

---

## 13. Coarse anchor lifecycle: cone-clean + starvation guard

**Design.** Active coarse anchor (`activeCoarseNode`) is set on
ProofFormalization entry, locked while blocker repair is in progress,
moved by Reviewer Continue when "change is allowed".

**Kernel reality.** The starvation guard is more elaborate:

- `cycles_in_coarse_repair_mode: u32` counter, incremented every cycle
  the anchor remains stuck in repair mode.
- `stuck_coarse_repair_threshold` (default 8, overridable via env) —
  when reached, anchor change is unlocked even without strict shallow
  closure.
- `coarse_repair_mode()` predicate — TRUE iff any task-blocker carrier
  lies outside the anchor's down-cone.
- `coarse_legal_active_set()` — base is the anchor's down-cone; widens
  to include each task-blocker carrier's down-cone when in repair mode.
- `active_coarse_change_allowed()` — TRUE under four conditions
  including clean-unlock (anchor shallow-closed + no blockers) and
  starvation-escape.

**Disposition.** Kernel detail.  The starvation guard is an operational
escape valve; the design contract is "anchor change is allowed sometimes,
and the reviewer can pick".  The small spec admits all anchor changes
non-deterministically when `ActiveCoarseChangeAllowed` holds, which
weakens the kernel's contract — refinement would project the kernel's
4-condition predicate onto the spec's 3-condition disjunction.

---

## 14. Audit lane fusion: NeedInputAuditor reuses StuckMathAudit infrastructure

(See deviation 6.)

The kernel uses `Stage::StuckMathAudit` for both `RequestKind::StuckMathAudit`
and the NeedInputAuditor variant.  The small spec keeps the two stages
distinct (`StuckMathAudit` vs `NeedInputAuditor`); the kernel fuses them at
the stage level and distinguishes by a context field.

This is a real protocol-level deviation per the brief's partition decision
#3 (which says to keep three distinct audit lanes).  The big spec's fusion
must either be re-split at the protocol level, or refinement requires
projecting both `Stage::StuckMathAudit` arms onto the right small-spec
stage by reading the `need_input_audit` context.

**Disposition.** **Kernel deviation noted.**  Per the brief, the design
prefers the small spec's three-lane partition.  Future kernel work could
either: (a) split the stage at the kernel level; or (b) accept the fusion
as a kernel-side implementation choice and document the projection via
context-field reading.  No immediate action — the kernel's fusion is
backward-compatible with the design contract because the dispatch-time
distinction (which lane "owns" the burst) is observable.

---

## 15. Global repair lifecycle: Step C consume-grant mechanism

**Design.** Three-step lifecycle: `none → request_pending → grant_available →
none`, tracked by a single `globalRepairStep` variable.

**Kernel reality.** The kernel uses two `Option<Record>` fields:

- `pending_global_repair_request: Option<GlobalRepairRequest>` — Step A
  (reviewer proposes extension nodes).
- `pending_global_repair_grant: Option<GlobalRepairGrant>` — Step B
  (auditor-approved extension nodes).

Plus several bookkeeping fields:

- `latest_global_repair_audit_decline_reason: String`
- `latest_global_repair_audit_decline_cycle: Option<u32>`
- `last_reviewer_global_repair_request_cycle: Option<u32>` — S10
  cooldown.
- `ever_shallow_coarse_closed: BTreeSet<NodeId>` — monotone history.
- `global_repair_mode_enabled: bool` — kill-switch.

**Disposition.** Kernel detail.  The cooldown / decline-reason / kill-
switch are operational concerns; the design's three-step lifecycle
captures the protocol semantics.

The `ever_shallow_coarse_closed` history is the load-bearing field for
the `AnchorChangeForbiddenDuringGlobalRepair` invariant (kernel:
`ever_shallow_coarse_closed_regressed()` non-empty ⇒ anchor change
forbidden).  The small spec captures the consequence (anchor change
forbidden during global repair) without modeling the history set.

---

## 16. ProtectedReapproval as gate variant

**Design.** Three gate kinds: `advance`, `need_input`, `protected_reapproval`.
`MaybeIssueProtectedReapprovalGate` is its own action.

**Kernel reality.** ProtectedReapproval is one of three `GateKind`s
(`kernel/src/model.rs`), reached when a proof-phase worker delta reopens
an approved protected-closure node.  The kernel tracks both
`pending_protected_reapproval_nodes` (the closure set requiring re-
approval) and `pending_protected_semantic_scope_confirmation` (a
distinct confirmation flow for explicitly-scoped semantic-change
re-issues by the reviewer).

**Disposition.** Design intentional collapse.  The semantic-scope
confirmation flow is a sub-mechanism of the reapproval gate; the small
spec abstracts both into "gateKind = protected_reapproval, pending set
non-empty".

---

## 17. Orphan cleanup

**Design.** Not explicitly modeled.

**Kernel reality.** When a worker delta introduces orphans (nodes not
reachable from any configured target's coverage closure), the kernel
schedules an orphan-cleanup worker burst before any verifier drain.
This is its own dispatch path in `start_cycle` and across several
worker-acceptance paths.

**Disposition.** Design intentional.  Orphan cleanup is a structural
consistency mechanism — the design contract is "presentNodes is
reachable from configured-target coverage closure" (a sub-invariant of
the structural type predicate).  The small spec doesn't enforce this
sub-invariant because the action set doesn't produce orphans
(`AcceptWorker`'s structural-delta sub-disjunct only removes from
`openNodes`, never adds disconnected nodes).

A future refinement extension would either: (a) add an orphan-cleanup
action and the orphan-free invariant; or (b) constrain `AcceptWorker`'s
structural delta to preserve reachability.

---

## 18. Phase advance gating: ProofFormalization → Cleanup

**Design.** TheoremStating phase advance is reviewer-mediated via
`ReviewAdvancePhase`. ProofFormalization advances automatically to
Cleanup when formalization becomes complete (no blockers, all proofs
closed, all closure records fresh).

**Kernel reality.** The kernel has direct phase-flip sites
(`apply_proof_paper_accept`, `apply_proof_corr_accept`,
`apply_proof_sound_response`, `apply_proof_review_response`, and
`route_after_progress`) that call `enter_cleanup_phase` when
`formalization_complete()` returns TRUE. Runtime local-closure refresh
can make that predicate true before a reviewer acts.

**Disposition.** No longer a deviation for the small spec:
`SupervisorProtocol.tla` includes `ProofFormalizationAutoAdvance` and
disables `IssueReviewRequest` in completed ProofFormalization states.

---

## 19. WorkerOutcome::Stuck/NeedsRestructure snapshot rollback

**Design.** Worker outcomes are independent of structural delta; the
spec doesn't model "snapshot delta" as a separate observable.

**Kernel reality.** When the worker returns Stuck or NeedsRestructure
with a snapshot delta (exploratory edits), the kernel runs
`state.restore_committed()` to revert in-memory state and emits
`RestoreWorktreeToActiveWorkerBase` to revert disk.  The runtime
captures the rolled-back snapshot as `last_invalid` Tablet WIP for the
next worker.

**Disposition.** Design intentional.  The rollback is a kernel-side
safety mechanism; the protocol contract is just "Stuck/NeedsRestructure
returns control to the reviewer without applying the worker's
exploratory deltas".  The single-mirror structure of the small spec
makes the rollback vacuous: any partial Stuck-time delta the worker
applied was never committed to the spec's structural state.

---

## 20. Paper-Fail precedence with verifier drain exception

**Design.** Verifier acceptance routes back to Reviewer unconditionally
when a Fail blocker survives.

**Kernel reality.** When a paper verifier response leaves a current
PaperFaithfulness or Substantiveness Fail blocker live, the paper-accept
handler routes to Reviewer — EXCEPT if some non-adjudicable Unknown
blocker has a live verifier frontier, in which case that verifier runs
first to prevent reviewer tasking from pinning a freshly changed
fingerprint.

**Disposition.** Kernel detail.  The exception is a fingerprint-pinning
race-condition fix at the kernel level; the design contract doesn't
distinguish "adjudicable" vs "non-adjudicable" Unknowns because both
are handled uniformly by the reviewer.

---

## 21. Local-closure tier: record schema vs status enum

**Design.** The intent spec abstracts the closure tier as a single
total function `localClosureStatus : Nodes -> {verified, unverified}`
(SupervisorCore.tla §"Local-closure status"). Coverage is structural
(total function); status mutex is structural (2-element codomain).

**Kernel reality.** The kernel stores three separate maps and one
flag (`kernel/src/model.rs`):

- `local_closure_records: BTreeMap<NodeId, LocalClosureRecord>` — the
  passed-probe records, with toolchain / lake_manifest / preamble /
  approved-axioms / per-decl hashes, dep-relationship hashes
  (boundary_theorems / strict_theorem_deps / strict_definition_deps),
  per-dep `kernel_semantic_hashes`, and (since audit H-4) an
  `axcheck_status: AxcheckStatus` field.
- `local_closure_unverified_nodes: BTreeSet<NodeId>` — pending re-probe.
- `local_closure_failures: BTreeMap<NodeId, ErrorSummary>` — failure
  diagnostics keyed by unverified entries.
- `last_clean_local_closure_*` mirrors with a paired
  `last_clean_local_closure_mirror_ready: bool` readiness flag.

The intent spec's "verified" corresponds to `node ∈ records.keys()` AND
the canonical predicate
(`LocalClosureRecord::is_consistent_with_state`) is OK; "unverified"
corresponds to `node ∈ unverified_nodes`. The kernel adds:

- Hash-based staleness detection (drift detection across env policy
  changes — Audit H-2 added a per-step rescission hook).
- Sentinel-value transient state (`TODO_PATCH_C_D_HASH`) between
  engine accept and runtime backfill.
- Dep-relationship maps used for reverse-index acceleration of
  invalidation walks (boundary_statement_consumers /
  strict_dep_consumers).
- Per-record axcheck telemetry (Audit H-4) so re-enabling axcheck
  invalidates records taken under `--no-axcheck`.

**Disposition.** Kernel optimization, not a design deviation. The
intent spec's verified ⇔ unverified status maps to the kernel's
records / unverified split via the obvious collapse. The kernel's
richer machinery serves the same intent — every node's status is
monotone in the records ∪ unverified ∪ failures partition.

Refinement obligation: every transition that flips
`localClosureStatus[n]` in the intent spec must correspond to a
records/unverified mutation in the kernel; the kernel's hash-drift
hooks (Audits H-2 / H-4) realize the intent spec's
`RescindApprovedAxiom` action by demoting records whose policy hash
no longer matches current `APPROVED_AXIOMS.json`.

Aligned items: validate()'s closure-tier asserts (Audits H-1 / M-1)
mirror SupervisorCore's `ClosureCoverageTotal` and
`ClosureStatusMutex` invariants — every present sorry-free
proof_node sits in records ∪ unverified, and the two are mutually
exclusive. The kernel's `ensure_local_closure_coverage()` helper
(Audit C-3) is the constructive guarantor.

---

## 22. RevisionStating phase: kernel refinement of theorem_stating

**Design.** The spec's `Phases` / `PhaseValues` set is
`{theorem_stating, proof_formalization, cleanup, complete}`. Every
statement-editing transition (worker validation, reviewer decisions, the
paper / substantiveness / correspondence / soundness verifier gates, the
HumanGate `AdvancePhase` → `proof_formalization` edge) is keyed on
`phase = "theorem_stating"`.

**Kernel reality.** The kernel adds a fifth phase, `Phase::RevisionStating`
(`kernel/src/model.rs`), for revision-mode runs that start from an
already-populated `Tablet/` and update it against a newer paper version
(`revision_plan.md`). `RevisionStating` is behaviorally a clone of
`TheoremStating`: it shares worker/reviewer validation, the target-edit
modes, all four verifier-lane gates, the StuckMathAudit machinery, and the
HumanGate-before-`ProofFormalization` transition. The kernel routes both
phases through the same `apply_theorem_*` handlers, the same
`theorem_start_request_kind` / `select_theorem_held_target`, and the same
`current_substantiveness_state` / `substantiveness_verify_nodes` gate
predicates; the single shared helper `Phase::is_theorem_stating_like()`
witnesses the alias at every `phase == TheoremStating` predicate.

`RevisionStating` differs from `TheoremStating` only at *startup*, not in
its transition relation: it is created by an out-of-band import action
(`import_revision_project`) rather than a fresh-run seed, it begins at
`Stage::StuckMathAudit` (a revision-planning audit) instead of
`Stage::Worker`, and it carries inherited verifier approvals plus a
frozen-node set. The revision statement-edit envelope is dynamic —
`present − frozen_nodes` at plan-build time (`RevisionContext::editable_now`),
so nodes born during `RevisionStating` are editable; this is the same
present-minus-frozen shape as the spec's `TheoremRestructureEnvelope`
(the stored `editable_nodes` snapshot is display-only, not an enforcement
set). Import, approval inheritance, the substantiveness
re-baseline, and revision planning are setup-time state construction, not
new protocol edges — the same out-of-spec status the legacy/exported-tablet
import already has (the spec models only fresh runs). The revision planner is
selected by a `revision_planning: Option<RevisionPlanningContext>` scenario
field on `StuckMathAuditState`, one more arm of the same `Stage::StuckMathAudit`
context-dispatch the spec already abstracts non-deterministically (#14): it
takes a `Stage::StuckMathAudit` step like any other audit lane and adds no new
spec stage or transition. The kernel's audit-role mutex (`ProtocolState::
validate`) keeps it exclusive with the other lanes; that is an internal
consistency check, not a spec edge.

**Disposition.** Design intentional collapse. The spec's `theorem_stating`
phase stands for both `TheoremStating` and `RevisionStating`; no new spec
phase value is introduced, so no spec transition is left unhandled and
`Phase \in Phases` / `phase \in PhaseValues` TypeOK is preserved unchanged.
Refinement projects `RevisionStating` onto the spec's `theorem_stating`
phase: every kernel transition taken in `RevisionStating` corresponds to the
identically-shaped `theorem_stating` transition in
`SupervisorCore.tla` / `SupervisorProtocol.tla`. The revision-only setup
machinery (import, inheritance, re-baseline, planning) is out of spec scope,
consistent with import not being modeled.

The two HumanGate-before-`ProofFormalization` edges are both already covered
by this projection, so no `.tla` change accompanies the kernel's step-12
transition:

- **Approval.** The `HumanApproveAdvance` action's
  `theorem_stating → proof_formalization` arm
  (`SupervisorCore.tla` / `SupervisorProtocol.tla`) is the projection of the
  kernel's `RevisionStating → ProofFormalization` advance (`coarse_dag_nodes`
  set from present nodes, `RevisionContext` preserved as metadata).
- **Rejection.** The kernel returns the rejected gate to its *pre-gate*
  phase. `SupervisorCore.tla`'s `HumanFeedback` already models this as
  `UNCHANGED phase`, and `SupervisorProtocol.tla`'s `HumanFeedbackAfterAdvance`
  as `phase' = "theorem_stating"`; both are the same projected self-loop
  since `RevisionStating` collapses onto `theorem_stating`. The kernel's
  prior hard-coded `phase = TheoremStating` on rejection was a kernel-only
  bug (`apply_human_gate_response`, fixed in step 12) that was invisible at
  the spec level — a genuine `TheoremStating` gate's rejection is a no-op
  there, but a `RevisionStating` gate's was not — so the fix brings the
  kernel into agreement with the spec rather than the reverse.

---

## 23. Backend axis: `nodeTarget` / `FormalImports` / CrossTargetFormalIsolation

**Design.** Phase IV introduces a per-node proof-assistant backend
(`BackendId`). The kernel carries a tablet-wide default
`ProtocolState.tablet_target` plus a SPARSE per-node override map
`node_target` (a node absent from the map inherits `tablet_target`, via
`effective_node_target`). A new structural invariant —
`CrossTargetFormalIsolation` — forbids a formal-import edge `a -> b`
(`b ∈ deps[a]`) from crossing backend targets, because both declarations are
elaborated together; NL `\noderef` citations (`dep_statement_hashes`,
governed by Soundness) are tracked separately and may cross.

**Spec modeling.** `SupervisorCore.tla` models the backend axis with three
CONSTANTS — `Backends` (the closed `BackendId` set), `TabletTarget` (the
tablet default), and `FormalImports ⊆ Nodes × Nodes` (the formal-import
relation, i.e. kernel `deps`) — and one VARIABLE, `nodeTarget`, a TOTAL
function `[Nodes -> Backends]` (the spec collapses the kernel's
sparse-override + default into one total map: a node with no override takes
`TabletTarget`, matching `effective_node_target`). The invariant
`CrossTargetFormalIsolation` mirrors the kernel check byte-for-byte in intent.

Two deliberate collapses:

1. **`deps` modeled as a CONSTANT relation, not a protocol edge.** Formal
   imports are structural (a node's source physically imports another's
   declaration), not protocol transitions, so `FormalImports` is a constant —
   no Init/Next surgery. This is the same disposition import already has
   (import is not modeled as a spec edge, #4 / Summary bucket 1).
2. **`nodeTarget` IMMUTABLE in step 12.** No action assigns it; it is held
   `UNCHANGED` everywhere via the `BackendVars` cluster (TLC machine-checks
   that every action holds it — a missed action raises "variable not
   assigned"). Per-node reassignment dynamics are deferred to a later phase.

**Disposition.** Design intentional collapse + deferred dynamics. All-Lean is
`Backends = {"lean"}` (`tablet_target = node_target = Lean`), so `nodeTarget`
is the constant `"lean"` map and the invariant is VACUOUS — byte-identical to
today. The Protocol-spec threading of the backend axis is deliberately
deferred; `SupervisorCore.tla` owns the structural node invariants, and
extending `SupervisorProtocol.tla` is a later-phase item.

---

## 24. LastClean rewind-target selection is commit-pinned (runtime/git concern)

**Design.** `ResetChoice::LastClean` is a logical rollback: live → committed
plus a restore of the `lastClean*` status / fingerprint mirrors (cf. #6). The
spec models *what state* the rewind lands on, abstracting away *which git
commit* materializes that state on disk.

**Kernel reality.** The on-disk rewind is `git reset --hard <target>` in
`runtime.rs:restore_repo_worktree_to_last_clean`. Two invariants were added
(Bug 2, designs incident 2026-06-26) that live entirely below the spec's
abstraction but must hold for the logical rollback to be faithful:

1. **Commit-pinned target.** The target is selected among two candidates —
   the durable commit pointer (`ProtocolState.last_clean_commit`, the HEAD SHA
   recorded after a clean checkpoint's sink commits) and the
   `supervisor2/clean-*` tag that is an *ancestor of HEAD* and *nearest to
   HEAD* — taking whichever is MORE RECENT (nearer HEAD; ties go to the commit
   pointer, the exact mirror match). Round-2 refinement: the pointer no longer
   wins unconditionally, so a stale pointer (e.g. a later `rev-parse HEAD`
   failed and the pointer lagged a newer clean tag) cannot cause a needlessly
   long rollback. It is NEVER the lexically-highest `clean-{event_count}` tag —
   `event_count` is non-monotonic across event-log segmentation / prior
   rewinds, so the lexical max can be an ancient wrong checkpoint. With no
   HEAD-ancestor target the runtime fails loud rather than rewinding to a
   stale / non-ancestor tag.

2. **State-integrity faults route to LastCommit, never LastClean (round-2).**
   The operational/state-inconsistency recovery — the startup
   "kernel state diverges from disk" fingerprint check in
   `load_runtime_with_fingerprint_validation` — now AUTO-REWINDS to the last
   checkpoint (`ResetChoice::LastCommit` semantics: `restore_worktree_to_head`,
   `git reset --hard HEAD`, the most recent `supervisor2/checkpoint-*` commit,
   ~1 cycle back), then re-checks; it fails loud only if the divergence
   PERSISTS after the rewind (HEAD itself diverges = genuine corruption). This
   is minimal and automatic: a stray worktree edit is discarded back to the
   committed checkpoint instead of taking the run down or — as an older
   deployed binary did — auto-selecting `LastClean` (whose nearest tag was 226
   cycles back for the designs run). "clean" is a VERIFIER-LANE judgement, not
   a process/disk-state property. `LastClean` remains reserved strictly for an
   explicit reviewer-driven `ResetChoice::LastClean`: `ReviewResetChoices` is
   the sole producer of `"lastClean"`, no fault path reaches
   `restore_repo_worktree_to_last_clean`, and `LastCommit` arms emit
   `RestoreWorktreeToHead`.

**Disposition.** Design intentional; runtime-only refinement. The spec's
`ResetChoice::LastClean` continues to denote the logical mirror-restore (the
semantic path is UNCHANGED); the commit-pin + ancestor-filter guarantee the
materialized commit equals the state the `lastClean*` mirrors describe, so the
projection `live ← lastCleanLive` remains well-defined. The state-fault →
`LastCommit` auto-rewind is an operational recovery below the spec abstraction
(the spec does not model disk/worktree divergence or its repair). No new spec
state variable is warranted (the commit SHA is a disk artifact, not protocol
state).

---

## 25. Fresh-run initial planner: unconditional cycle-1 StuckMathAudit dispatch

**Design.** A fresh run's first cycle dispatches a `Stage::Worker` burst: the
worker owns first-request DAG decomposition, and the spec models the start of
formalization as a non-deterministic worker/audit dispatch at
`Stage::Start → Stage::{Worker | StuckMathAudit}`. StuckMathAudit dispatch is
abstracted non-deterministically (#14): the spec does not distinguish audit
*roles* (need-input, gap-research, revision-planning, …), only that a
`Stage::StuckMathAudit` step may be taken.

**Kernel reality.** Every fresh run now dispatches a StuckMathAudit *initial
planner* burst at cycle 1, before any worker — for math/paper runs and both PV
modes, unconditionally (no config knob). The planner reads the run's source of
truth (manuscript / prose goal file / pinned challenge statements) plus the
configured targets and emits an initial plan (foundational definitions and
lowest-layer lemmas first, DAG shape, decomposition strategy, target order) as
an ordinary audit output — `report` + `tasks` + `probe_paths`, no
`revision_actions` and no new wire fields. The plan is ADVISORY (Option W): the
worker keeps its existing first-request DAG-decomposition authority and reads
the plan as guidance.

The planner is selected by an `initial_planning: Option<InitialPlanningContext>`
scenario field on `StuckMathAuditState`, one more arm of the same
`Stage::StuckMathAudit` context-dispatch the spec already abstracts
non-deterministically (#14, §22's revision planner is its sibling). The
`start_cycle` guard fires on `phase == TheoremStating && initial_planning
.is_some()` (exact-phase — `RevisionStating` has its own planner), takes a
`Stage::StuckMathAudit` step like any other audit lane, and adds no new spec
stage, transition, or event kind (the `tla_replay` fixtures are unchanged). The
audit-role mutex (`ProtocolState::validate`) keeps the carrier exclusive with
the other lanes; that is an internal consistency check, not a spec edge.

Retirement is exclusively the reviewer's existing `dismiss_audit_plan` (§5
audit-plan lifecycle; legality already spans `Phase::TheoremStating`). There is
NO forced plan retirement anywhere — no retire-on-first-accepted-worker-response
and no advance-time clearing — so the accepted plan follows the same
write → visible-on-Review/Worker → reviewer-dismiss lifecycle as every other
`audit_plan`, and re-dispatch is suppressed while it is live (mirroring the
revision plan). The seed lives in `seed_state_from_config` behind a freshness
guard (cycle 0, `TheoremStating`, no in-flight request, no live audit lane) and
a `RuntimeMetadata.initial_planning_seeded` replay gate, so a pre-feature event
log replays byte-identically.

**Disposition.** Design intentional collapse. The initial-planner burst
projects onto the spec's abstract non-deterministic `Stage::StuckMathAudit`
audit dispatch, and its retirement onto the existing `dismiss_audit_plan`
transition (§5); no new spec stage, transition, event kind, or state variable
is introduced. The seed is setup-time state construction, out of spec scope,
consistent with import not being modeled (§22). The net effect is that a fresh
run's cycle-1 non-determinism is resolved toward the audit branch first, which
the spec already permits.

**Amendment — periodic coverage re-planning.** The planner lane additionally
re-fires on a fixed cadence while construction is incomplete: whenever the
coverage feature is armed (`ProtocolState.coverage_replanning_source`, seeded
only on post-feature fresh runs under a
`RuntimeMetadata.coverage_replanning_seeded` replay gate — the exact
`initial_planning_seeded` pattern, its own flag so initial-planner-era logs
replay byte-identically), some configured paper target has empty coverage
(`orphan_construction_window_open` over `live`), the phase is exactly
`TheoremStating`, and `COVERAGE_REPLAN_INTERVAL_CYCLES` cycles have elapsed
since `last_stuck_math_audit_dispatched_cycle`
(`stuck_math_audit_coverage_replanning_trigger`), the Trigger-C preempt block
dispatches a coverage-planner StuckMathAudit: the `initial_planning` carrier is
synthesized at arming (`maybe_arm_coverage_planner`, with
`coverage_replanning: true` and the uncovered/covered target packet) and the
burst runs the same planner contract with a swapped role fragment. A LIVE
planner plan does not block the cadence — the planner-plan suppression in
`should_dispatch_stuck_math_audit` returns exactly this trigger, and the
accepted re-plan supersedes the live plan through
`apply_initial_planning_response`'s existing `superseded_audit_plan` snapshot
(stagnation triggers stay fully suppressed by a live planner plan; on-demand
requests and the forced-after-rewind flag keep absolute priority). PV scope:
the trigger keys on configured PAPER targets, so it is live on PV mode-A runs
(whose verification targets are seeded into `configured_targets` at setup) and
structurally dead on challenge-only mode-B and skeleton configs. Once every
target is covered the trigger is false forever and every dispatch decision is
byte-identical to the pre-feature kernel.

**Disposition of the amendment.** Same collapse, documentation-only: coverage
dispatches are further resolutions of the already-nondeterministic
`Stage::Start → Stage::StuckMathAudit` branch (#14, this section); plan
supersession projects onto the existing plan-overwrite step. No new spec
stage, transition, event kind, or variable; the `tla_replay` fixtures are
unchanged (no fixture arms the source, so replays are inert).

## 26. C1 + C4′ soundness scheduling: stability backlog + auto-dispatch fallback ordering

**Kernel (2026-07-15, branch `sound-backlog-c1c4`).** Two scheduling changes
in `select_theorem_sound_verify_node` / the theorem Review-handoff sites:

1. **C4′ fallback ordering.** Arm 3 of `select_theorem_sound_verify_node` no
   longer defers to `select_theorem_held_target()` (single rank-maximal
   candidate); it scans `live.present_nodes ∩ sound_auto_dispatch_eligible ∩
   current_sound_unknown`, ordered by assessment class (FreshUnknown →
   SelfEditUnknown/SplitUnknown → DepEditOnlyStalePassDeferred →
   ReviewerAcceptedPass), then open-dependent fanout DESCENDING (reverse-dep
   reachability over open proof nodes, computed on the fly), then node name
   ascending. `select_theorem_held_target` is byte-identical (it still
   defines the reviewer next_active envelope).

2. **C1 first-review backlog.** A per-node stability tracker
   (`sound_backlog_stability`: own-tex fingerprint + monotonic snapshot
   index, refreshed at `commit_live`) admits never-reviewed (`FreshUnknown`)
   nodes that pass the per-node gate (`needs_sound` + non-SKETCH +
   `sound_repair_ready`) WITHOUT the global `corr_blockers_exist` gate, once
   own-tex has been stable ≥ K snapshots (default 8,
   `TRELLIS_SOUND_BACKLOG_STABILITY_CYCLES`). The engine service point
   `issue_sound_backlog_or_review` may replace a theorem Review handoff with
   one backlog Sound panel per cycle (guard: `last_sound_response_cycle`);
   the dispatched node is persisted in `sound_backlog_dispatch` so the
   in-flight request re-derives byte-identically (validate() invariant,
   Malformed reissues).

**Spec.** `SoundVerifyNodes` (theorem arm), `PostSoundVerifyNodes`,
`RequestSoundVerifyNode` / `RequestVerifyNodes`, the four theorem accept
actions' stage routing, and `ReviewContinueAfterInvalid`'s next-stage sound
arm now encode the scan (`SoundScanEligibleFor` + ordered
`SelectedTheoremSoundVerifyNode`) and the backlog arm (`SoundBacklogNodes`,
`SoundBacklogNodesFor` at post-map accept sites). Abstractions:

* **Stability clock below the abstraction.** The spec has no snapshot
  counter; `SoundBacklogNodes` over-approximates stability as always
  satisfied. Every kernel schedule is a refinement of the spec behaviors.
* **Persisted dispatch field re-derived.** The kernel pins the backlog node
  in `sound_backlog_dispatch`; the spec re-derives it in
  `RequestSoundVerifyNode` via the deterministic fanout/name CHOOSE. The two
  agree while the request is in flight because verifier dispatches do not
  mutate the tablet (membership is stable across the round-trip).
* **Once-per-cycle guard.** At the paper / substantiveness / corr theorem
  accepts the kernel guard is vacuously open (a Sound response never routes
  to VerifyPaper/VerifyCorr within a cycle), so the spec fires the backlog
  arm unguarded there; at `AcceptSoundArtifactTheorem` the guard was just
  stamped by the response itself, so the spec keeps the unconditional
  Reviewer handoff. No `lastSoundResponseCycle` variable is introduced.
* **Proof-phase backlog service point unmodeled.** The kernel's
  `issue_review_or_stuck_math_audit` REVIEWER branch (shared with
  ProofFormalization) is also a backlog service point; the spec's
  proof-phase accepts keep their pre-existing Reviewer/audit routing. This
  under-approximates (the spec admits fewer dispatches than the kernel);
  converge if a proof-phase invariant ever needs it.
* **`ReviewContinueAfterValid`** routes through `Start` → `StartCycle`,
  whose priority ladder reads the amended `SoundVerifyNodes`, so it needed
  no local edit (the kernel's `theorem_start_request_kind` consults
  `sound_verify_nodes()` the same way; the backlog is deliberately NOT a
  cycle-start source in either artifact).

**Disposition.** Kernel refinement, spec amended at the theorem sites;
residual gaps are documented over-/under-approximations. TLC was not run for
this amendment (no TLC toolchain in the implementation environment);
`kernel/tests/tla_replay.rs` passes unchanged (the abstract engine does not
model the Sound lane).
---

## 27. Decide polarity flips: phase scope and targeted binding

**Design.** The audit-done flip arm (`SupervisorProtocol.tla`
`AcceptStuckMathAudit`, mirroring `SupervisorCore!AcceptStuckMathAudit`)
admits a polarity flip only in `phase = "proof_formalization"` ("Mode-B only
and proof_formalization only"). The arm is already ∃-target
(`\E t \in ChallengeTargets`), so a flip may bind any configured pair — no
action-shape change is needed for targeted (`set_live_polarity_target`)
flips.

**Kernel reality.** `apply_stuck_math_audit_response` applies a validated
`set_live_polarity` in any phase that admits the StuckMathAudit lane —
TheoremStating included (the dec2flt incidents flipped from TheoremStating).
The flip's pair binds via `decide_primary_binding`: a named
`set_live_polarity_target` (primary id, refutation target id, or pair node)
wins over the active/held seat. The phase-agnostic flip is guarded by the
approved-target demote bounce in `stuck_math_audit_validation_failure`:
demoting a node in `approved_target_nodes()` — the post-AdvancePhase
human-vouched set — is refused pending re-approval, alongside the
consumer-import and anti-thrash bounces.

**Disposition.** Kernel deviation noted (phase scope); the design's ∃-target
arm already covers targeted binding. The arm could be amended to
`phase \in {"theorem_stating", "proof_formalization"}`, but TLC was not run
in this environment, so the widened guard's interaction with the flip
properties (e.g. `AuditSoleFlipper`) would be unverified — the divergence is
documented instead of amended. `kernel/tests/tla_replay.rs` passes unchanged
(it replays JSON fixtures through the abstract engine and does not consume
the `.tla` arms).

---

## 28. Sound frontier yields to the worker; one frontier Sound per cycle

**Kernel (2026-07, branch `feature/sound-yields-to-worker`).** Two
scheduling changes on top of #26, motivated by a live TheoremStating run
where a large unverified sound set starved reviewer-routed worker tasks
(every cycle start dispatched a frontier Sound, whose issue path silently
cleared the pending empty-`task_blockers` task):

1. **Cycle-start yield.** The sound-frontier arm AND the
   reviewer-requested-Sound arm of `theorem_start_request_kind` /
   `proof_start_request_kind` fire only when `pending_task` is None. A
   pending worker task — including the empty-`task_blockers` Global
   shape that the existing first-arm preemption did not cover — now
   wins over both Sound sources (a single Continue can legally route a
   worker task and request sound verifiers; the requested set is
   persisted and served with priority at the next sound slot, so the
   reviewer-requested dispatch is deferred one slot, never lost).
   Paper / corr cycle-start precedence is byte-identical: an accepted
   asymmetry — a paper / corr frontier, including one the same
   response opened via `reset_blocker_ids`, may still displace an
   empty-`task_blockers` routed task (those frontiers are small and
   bounded, unlike the run-sized sound backlog that motivated the
   yield). With no pending task the Sound arms fill the idle slot
   exactly as before. Accepted displacement #2: the TheoremStating
   StuckMathAudit preempt (`theorem_stating_audit_preempt` in
   `start_cycle`) still deliberately displaces a routed worker task —
   clearing it — when its stagnation / forced / on-demand triggers
   fire; the post-audit Review re-routes and the dispatch cooldown
   bounds repetition (intentional phase machinery, cf. #14 / #25).

2. **Intra-cycle cap.** Frontier Sound dispatch from the cycle-start /
   accept-drain chain is capped at one per cycle by the same
   `last_sound_response_cycle` guard the C1 backlog service point
   already uses. The theorem Sound-accept Pass self-loop (drain every
   eligible Unknown before Reviewer — the C4′ post-verdict scan of
   #26) is retired: a theorem Sound accept always hands off to
   `issue_sound_backlog_or_review`, whose just-stamped guard routes to
   Reviewer, so the backlog never fires after a same-cycle sound
   response. The theorem paper/corr accept frontier sites carry the
   guard explicitly (normally open — no Sound response precedes them
   within a cycle — structural defense only). The cap is not a global
   invariant: the non-adjudicable-Unknown routing sites
   (`route_after_progress`,
   `route_non_adjudicable_unknown_verifier`) sit outside it by design
   (K-1 deadlock prevention) and can add a Sound in the same cycle on
   rare Fail-escalation / reissue traces, each separated by a
   review/worker attempt — no drain loop exists there. The resulting
   ordinary cycle shape is reviewer → worker → at most one kernel
   Sound → reviewer.

**ProofFormalization scope.** The cycle-start yield applies to PF
(`proof_start_request_kind` had the identical displacement shape). The
intra-cycle cap does NOT: the PF Sound-accept drain loop deliberately
verifies every Unknown before the Reviewer sees the state (Audit
Finding 3 — capping it would re-leak unverified blockers into the
reviewer's view and regenerate bogus `task_blockers`), and PF sound
sets are cone-scoped rather than run-sized, so the starvation mechanism
is the cycle-start preemption alone.

**Reviewer provenance.** Kernel-auto-scheduled sound results (frontier
or backlog — any served node not in
`reviewer_requested_sound_verifier_nodes` at dispatch time) are recorded
in `latest_sound_auto_review_nodes` (persisted; serde default +
skip-when-empty; cleared with `latest_sound_review_nodes`), surfaced on
Review requests as `kernel_scheduled_sound_review_nodes` /
`request_summary.kernel_scheduled_sound_nodes`, and explained by the
conditional fragment `review/common/25a_kernel_scheduled_sound.md`.
State-only + request-view; no response validator surface.

**Spec.** Amended in `SupervisorProtocol.tla`: `StartCycle`'s theorem
and proof sound arms gain `pendingTask = NoPendingTask` (the kernel's
reviewer-requested arm is unmodeled at theorem_stating — see the
`SoundVerifyNodes` comment — and folds into `SoundVerifyNodes` at
proof_formalization, so the single gate covers both Sound sources);
`AcceptSoundArtifactTheorem`'s stage routing is unconditionally
`Reviewer`; `ReviewContinueAfterInvalid`'s next-stage sound-scan arm is
removed (that arm always installs a pending task, so the yielded
frontier can never fire there — reviewer-requested Sound at that site
stays an under-approximation, as before). The theorem paper /
substantiveness / corr accept sound arms are unchanged (the kernel
guard is vacuously open there, per #26's once-per-cycle bullet).
`SupervisorCore` abstracts the cycle-start ladder non-deterministically
and needs no change. The provenance fields are below the spec's
abstraction (request-view only).

**Disposition.** Kernel refinement, spec amended at the three sites;
verified with the bounded sim harness (`scripts/run_tlc.sh sim`).

---

## Summary

The deviations fall into three buckets:

1. **Kernel optimization, design implicit (#1, #4, #5, #8, #11, #20):**
   the kernel maintains richer state for dispatch decisions, scheduling,
   or fingerprint-pinning; the design abstracts those away.

2. **Design intentional collapse (#3, #7, #9, #12, #16, #22, #23, #25):** the
   small spec replaces a multi-field cluster with non-determinism or a
   single abstract value (e.g. the sparse `node_target` + `tablet_target`
   collapse into a total `nodeTarget`, #23).  Refinement projects the
   kernel's structure onto the abstract.

3. **Kernel deviation worth flagging (#14, #18, #27):** the kernel fuses two
   design-distinct lanes (StuckMathAudit + NeedInputAuditor), executes
   a design-reviewer-mediated transition automatically (mid-cycle
   ProofFormalization → Cleanup), or applies Decide polarity flips
   phase-agnostically where the spec's arm is proof_formalization-only
   (#27).  These are the cases where the big
   spec should converge to the design at the next opportunity.

Aligned items (#10, #13, #15, #17, #19, #21) are noted for completeness;
they're not deviations but worth documenting because the spec's
abstraction is non-obvious.
