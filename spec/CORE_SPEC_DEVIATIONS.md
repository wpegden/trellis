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

**Design.** Phase advance is reviewer-mediated via `ReviewAdvancePhase`.
The big spec also has automatic phase advance from ProofFormalization
to Cleanup when formalization becomes complete (no blockers, all
proofs closed, all closure records present).

**Kernel reality.** The kernel has **four** phase-flip sites
(`apply_proof_paper_accept`, `apply_proof_corr_accept`,
`apply_proof_sound_response`, `apply_proof_review_response`) that
all call `enter_cleanup_phase` when `formalization_complete()` returns
TRUE.

**Disposition.** Design intentional collapse.  The phase advance from
ProofFormalization to Cleanup happens at the next reviewer hand-off in
the small spec (via `HumanApproveAdvance`'s ProofFormalization branch);
the kernel's mid-cycle phase flips are an optimization that avoids a
pointless reviewer round-trip when all gates pass.

This is a true deviation in observable behavior: the kernel can advance
phase mid-cycle (at a verifier accept point); the small spec only
advances at the HumanGate.  The big spec models the kernel's mid-cycle
flips faithfully.

The refinement obligation: the BigSpec's mid-cycle phase flips
correspond to the small spec's `HumanApproveAdvance` + skip-the-
reviewer pattern.  Either: (a) add a `ProofFormalizationAutoAdvance`
action to the small spec; or (b) treat the kernel's flip as a
stuttering step that doesn't change small-spec state until the next
HumanGate engagement.

**Recommended action**: extend the small spec with a
`MaybeAutoAdvanceToCleanup` action when `FormalizationComplete` holds.

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

## Summary

The deviations fall into three buckets:

1. **Kernel optimization, design implicit (#1, #4, #5, #8, #11, #20):**
   the kernel maintains richer state for dispatch decisions, scheduling,
   or fingerprint-pinning; the design abstracts those away.

2. **Design intentional collapse (#3, #7, #9, #12, #16):** the small
   spec replaces a multi-field cluster with non-determinism or a single
   abstract value.  Refinement projects the kernel's structure onto the
   abstract.

3. **Kernel deviation worth flagging (#14, #18):** the kernel fuses two
   design-distinct lanes (StuckMathAudit + NeedInputAuditor) or executes
   a design-reviewer-mediated transition automatically (mid-cycle
   ProofFormalization → Cleanup).  These are the cases where the big
   spec should converge to the design at the next opportunity.

Aligned items (#10, #13, #15, #17, #19, #21) are noted for completeness;
they're not deviations but worth documenting because the spec's
abstraction is non-obvious.
