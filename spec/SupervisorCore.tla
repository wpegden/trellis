------------------------------- MODULE SupervisorCore -------------------------------
EXTENDS Integers, FiniteSets, Sequences

(***************************************************************************)
(* SupervisorCore is an abstract, intent-level model of the Trellis        *)
(* supervisor protocol.  Whereas `SupervisorProtocol.tla` faithfully       *)
(* refines the Rust kernel in `kernel/`, this spec records the design      *)
(* contract that the kernel is meant to implement.                         *)
(*                                                                        *)
(*   Big spec      : `SupervisorProtocol.tla` (~16,600 lines, ~121 vars). *)
(*   Refinement    : `BigSpec ⇒ SupervisorCore!Spec`  (gated, not yet     *)
(*                   discharged — see SPEC_TODO.md).                      *)
(*   Deviations    : Cases where the kernel deviates from the intent      *)
(*                   modeled here are recorded in                         *)
(*                   `CORE_SPEC_DEVIATIONS.md`.                           *)
(*                                                                        *)
(* DESIGN STANCE                                                          *)
(*                                                                        *)
(*   * Retry counters, transport counters, lane ids, fingerprint values   *)
(*     are abstracted away.  Where the kernel says "retry up to N then   *)
(*     escalate", the spec says "may stutter or escalate", non-          *)
(*     deterministically.                                                 *)
(*                                                                        *)
(*   * Verifier verdicts are 3-valued (unknown / pass / fail) per lane.   *)
(*     The kernel's richer 11-status SoundAssessmentStatus taxonomy is    *)
(*     a refinement detail; see CORE_SPEC_DEVIATIONS.md §1.               *)
(*                                                                        *)
(*   * Five verifier lanes are first-class: Correspondence,               *)
(*     Faithfulness, Soundness, Substantiveness, Deviation.  Each lane    *)
(*     contributes to `globalBlockers` independently.                     *)
(*                                                                        *)
(*   * Three audit lanes are kept distinct: `CleanupAudit`,                *)
(*     `StuckMathAudit`, `NeedInputAuditor`.  Each has its own active     *)
(*     flag and stage; cross-audit collapsing would lose the              *)
(*     CleanupAudit-only task-list lifecycle.                             *)
(*                                                                        *)
(*   * Reviewer blocker partition is TWO buckets: `task_blockers` and     *)
(*     `reset_blockers`.  Pass-override authority was retired             *)
(*     (Option C, 2026-06-04).                                            *)
(*                                                                        *)
(*   * Worker mode is 4-valued: `local`, `restructure`,                   *)
(*     `coarse_restructure`, `cleanup`.  The carve-out invariants         *)
(*     `LocalModeSoundnessCarveOut`, `AuthorizedNodesScopeContract`, and *)
(*     `ReviewerScopeAuthorizationComplete` pin the meaning of the four   *)
(*     values plus the global-repair escape hatch.                        *)
(*                                                                        *)
(*   * Local-closure verification is its own status component (not       *)
(*     folded into `blocking`).  A node sitting in                       *)
(*     `localClosureUnverified` blocks `Cleanup` entry but is not the    *)
(*     reviewer's concern — recovery is a worker re-attempt on the same  *)
(*     node, not a reviewer reset or repartition.                        *)
(*                                                                        *)
(* WHAT THIS SPEC DOES NOT MODEL                                          *)
(*                                                                        *)
(*   * Live versus committed mirrors (the big spec maintains both).      *)
(*     The core spec has only one copy of each structural field; a       *)
(*     reviewer reset is modeled as a non-deterministic re-seeding of    *)
(*     status maps, not as a structural rollback.                        *)
(*                                                                        *)
(*   * Fingerprint mirrors and `*_currentFp` / `*_approvedFp` maps:      *)
(*     these are the kernel's mechanism for "fingerprint drift           *)
(*     invalidates prior approval".  At the design level, the equivalent *)
(*     is: any worker delta may flip statuses back to `unknown`, modeled *)
(*     by `AcceptWorker` being free to re-seed lane status maps.         *)
(*                                                                        *)
(*   * Wrapper request envelopes (`WrapperRequest`, `WrapperResponse`,   *)
(*     payload schemas).  The only request-side state is                 *)
(*     `inFlightRequestKind`.                                             *)
(*                                                                        *)
(*   * Reviewer prompt context, schema versions, paper focus ranges,     *)
(*     work style hints — all out.                                        *)
(***************************************************************************)

CONSTANTS
    Nodes,
    Targets,
    ChallengeTargets,
    NoNode,
    MaxCycle,
    InitialConfiguredTargets,
    InitialPresentNodes,
    \* Phase IV step 12: the proof-assistant backend axis (kernel `BackendId`).
    \* `Backends` is the closed set of backends; `TabletTarget` is the tablet-
    \* wide default (kernel `ProtocolState.tablet_target`); `FormalImports` is
    \* the formal-import edge relation `a -> b` (kernel `deps`), modeled as a
    \* CONSTANT because import edges are structural, not protocol transitions
    \* (cf. CORE_SPEC_DEVIATIONS.md).
    Backends,
    TabletTarget,
    FormalImports,
    \* ---- PV mode-B / prove-or-disprove axis (forward design) -------------
    \* `GoalMode` is the run-wide goal mode (kernel `goal_mode`): "lean" is
    \* mode-B (Lean-goals / byte-pinned challenge specs, no paper-faithfulness
    \* bucket); "prose" is mode-A (math autoformalization, paper-grounded).
    \* In mode-B the configured (paper-faithfulness) target bucket is empty —
    \* see `ModeBConfiguredTargetsEmpty`. The prove-or-disprove machinery
    \* below (per-target live polarity, abstract truth model, audit-sole
    \* flipping) is FORWARD DESIGN — no kernel parity yet, specifying ahead
    \* of Rust per the README "Rust Parity Rule" carve-out. The challenge
    \* substrate it builds on (ChallengeTargets / challengeCoverage / audit
    \* lanes) does have kernel parity.
    GoalMode,
    \* Anti-thrash bound on per-target live-polarity flips: the audit lane may
    \* flip a target's live side Prove<->Disprove at most `MaxPolarityFlips`
    \* times across a run. Keeps the sole-flipper lane from livelocking the
    \* Decide direction.
    MaxPolarityFlips,
    \* Abstract truth model. `ChallengeTruth[t] \in Polarities` is which side
    \* of the Decide pair (T vs ~T) is ACTUALLY true under the abstract
    \* reality. The axiom-floor closure gate (CloseSidePermitted) only lets a
    \* covering node close the side that matches `ChallengeTruth[t]` — so
    \* closing a FALSE statement is impossible on EITHER polarity. This is the
    \* abstraction of "passing the same clean-axiom gate" (kernel
    \* CANONICAL_APPROVED_AXIOMS = {propext, funext, Classical.choice,
    \* Quot.sound}); a false Lean goal has no axiom-clean closed proof. Seeded
    \* by a CONSTANT so a single sim explores both true-Prove and true-Disprove
    \* targets; immutable thereafter (the reality of a Decide question does not
    \* move during a run).
    ChallengeTruth,
    \* Dedicated PV under-model assumptions node (`Tablet/Assumptions` in the
    \* kernel). Setup also seeds an approved uninterpreted `axiom` predicate
    \* hook naming Rust-valid raw Aeneas values. Workers stage paired `.spec` /
    \* `.tex` conditional property axioms over that hook here; ordinary
    \* Correspondence gates the pair before the assumptions lane gates Rust-
    \* language eligibility. Eligibility is limited to hook-conditional
    \* property axioms.
    AssumptionsNode

ASSUME Nodes # {}
ASSUME Targets # {}
ASSUME ChallengeTargets \cap Targets = {}
ASSUME NoNode \notin Nodes
ASSUME InitialConfiguredTargets \subseteq Targets
ASSUME InitialPresentNodes \subseteq Nodes
ASSUME MaxCycle \in Nat \ {0}
ASSUME TabletTarget \in Backends
ASSUME FormalImports \subseteq (Nodes \X Nodes)
ASSUME GoalMode \in {"lean", "prose"}
ASSUME MaxPolarityFlips \in Nat
\* Mode-B contract: the paper-faithfulness bucket is empty in a Lean-goals
\* run (byte-pinned challenge specs are the sole target type).
ASSUME (GoalMode = "lean") => (InitialConfiguredTargets = {})
ASSUME ChallengeTruth \in [ChallengeTargets -> {"prove", "disprove"}]
ASSUME AssumptionsNode \in Nodes

(***************************************************************************)
(* Enums.  Stated as plain string-set constants so TLC observes a closed   *)
(* type universe per variable.                                             *)
(***************************************************************************)

\* The kernel's `Phase::RevisionStating` (revision-mode statement editing)
\* is modeled here as the `theorem_stating` phase: it shares every
\* transition (worker/reviewer validation, all four verifier gates, the
\* HumanGate -> proof_formalization edge) and differs only in out-of-spec
\* setup (import, inherited approvals, frozen nodes). See
\* CORE_SPEC_DEVIATIONS.md §22.
Phases == {"theorem_stating", "proof_formalization", "cleanup", "complete"}

(***************************************************************************)
(* Stages.  One stage per "kind of request is in flight (or about to be)". *)
(* The three audit stages are intentionally distinct from each other (per *)
(* the brief's partition decision #3) and from `Reviewer`.                *)
(***************************************************************************)
Stages ==
    {
        "Start",
        "Worker",
        "VerifyFaithfulness",
        "VerifySubstantiveness",
        "VerifyCorrespondence",
        "VerifySoundness",
        "VerifyDeviation",
        "Reviewer",
        "HumanGate",
        "CleanupAudit",
        "StuckMathAudit",
        "NeedInputAuditor",
        \* GapResearch planner ↔ critic loop (Option C). Both share the
        \* kernel's StuckMathAudit lane; the spec gives each its own stage
        \* so the role/contract pairing is explicit.
        "GapResearchPlanner",
        "GapPlanCritic"
    }

(***************************************************************************)
(* RequestKinds: what kind of role is being asked to produce a response.   *)
(* Each maps one-to-one to a Stage value (except `Reviewer` which can     *)
(* take either of two routing reviewer responses).                        *)
(***************************************************************************)
RequestKinds ==
    {
        "none",
        "worker",
        "verifier_faithfulness",
        "verifier_substantiveness",
        "verifier_correspondence",
        "verifier_soundness",
        "verifier_deviation",
        "reviewer",
        "human_gate",
        "cleanup_audit",
        "stuck_math_audit",
        "need_input_auditor",
        "gap_research",
        "gap_plan_critic"
    }

GateKinds == {"none", "advance", "need_input", "protected_reapproval", "assumption_review"}

(***************************************************************************)
(* Worker outcome taxonomy.  The PV under-model outcome is a distinct      *)
(* non-progress verdict: it routes like NeedsRestructure, but carries a     *)
(* falsifying-model witness to the reviewer/auditor lane in the kernel.    *)
(***************************************************************************)
WorkerOutcomes ==
    {"valid", "invalid", "stuck", "needs_restructure", "target_false_under_model"}

(***************************************************************************)
(* Worker mode.  Four values per partition decision #5.                    *)
(***************************************************************************)
WorkerModes == {"local", "restructure", "coarse_restructure", "cleanup"}

(***************************************************************************)
(* Polarities of a Decide challenge target.  "prove" works the node that  *)
(* closes T; "disprove" works the node that closes ~T.  Exactly one side  *)
(* is LIVE per target at any time (`challengePolarity[t]`); the other is  *)
(* dormant (suppressed from the worker frontier and the completion        *)
(* predicate).  A target whose live side closes is "decided".            *)
(***************************************************************************)
Polarities == {"prove", "disprove"}

(***************************************************************************)
(* Per-target closure record for the LIVE side.  "open" = the live side's *)
(* covering node has not closed; "closed" = it has.  Closing is gated by  *)
(* the abstract axiom-floor (CloseSidePermitted), identical for both      *)
(* polarities.  The dormant side has no record — it is suppressed.        *)
(***************************************************************************)
ChallengeClosureStatuses == {"open", "closed"}

(***************************************************************************)
(* Verifier lanes (five, per partition decision #2).                       *)
(***************************************************************************)
VerifierLanes ==
    {
        "faithfulness",
        "substantiveness",
        "correspondence",
        "soundness",
        "deviation"
    }

(***************************************************************************)
(* Per-lane verdict status.  3-valued: a lane is either still unknown,    *)
(* has agreed Pass, or has agreed Fail.  Split verdicts and structural    *)
(* refinements are kernel-level taxonomy detail (see                       *)
(* CORE_SPEC_DEVIATIONS.md §1).                                            *)
(***************************************************************************)
LaneStatuses == {"unknown", "pass", "fail"}

(***************************************************************************)
(* Local-closure status.  Per partition decision #6, this is its own       *)
(* status discriminant, not a value of the lane status set.  A node is    *)
(* `verified` iff it has a fresh local-closure record; `unverified` means *)
(* the record is stale or absent.                                          *)
(***************************************************************************)
LocalClosureStatuses == {"verified", "unverified"}

(***************************************************************************)
(* Review decisions the reviewer may emit.  `done` is cleanup-only.       *)
(***************************************************************************)
ReviewDecisions == {"continue", "advance_phase", "need_input", "done"}

(***************************************************************************)
(* Global-repair lifecycle steps.  Step A — reviewer proposes; Step B —   *)
(* StuckMathAudit grants; Step C — reviewer consumes grant in a Continue. *)
(* `none` means no live request/grant on the table.                       *)
(***************************************************************************)
GlobalRepairSteps == {"none", "request_pending", "grant_available"}

(***************************************************************************)
(* Three audit lanes per partition decision #3.  Each has its own         *)
(* active flag and Stage / RequestKind; the brief calls out that          *)
(* collapsing them loses the CleanupAudit-only task-list lifecycle and    *)
(* the StuckMathAudit-only audit_plan lifecycle.                           *)
(***************************************************************************)
AuditLanes == {"cleanup_audit", "stuck_math_audit", "need_input_auditor"}

(***************************************************************************)
(* GapResearch planner ↔ critic loop: the bounded consecutive-reject       *)
(* ceiling N (kernel: `gap_plan_reject_limit()`, default 2). When          *)
(* gapRejectCount reaches this, the loop stops re-planning and escalates   *)
(* to a real HumanGate — the livelock guard.                               *)
(***************************************************************************)
GapRejectLimit == 2

(***************************************************************************)
(* PendingTask request kinds the spec exposes.  Pending tasks are kept    *)
(* across stage transitions only for the worker assignment; cleanup       *)
(* tasks have their own machinery (the `cleanupAuditActive` flag plus    *)
(* the set-shaped global blocker carrier additions).                      *)
(*                                                                        *)
(* This abstracts away the kernel's richer `PendingTask` struct           *)
(* (orphan_cleanup_nodes, paper_focus_ranges, next_worker_context_mode,  *)
(* etc.) — see CORE_SPEC_DEVIATIONS.md §3.                                *)
(***************************************************************************)
PendingTaskKinds == {"none", "worker"}

NoStatus(default) == [n \in Nodes |-> default]
NoTargetStatus(default) == [t \in Targets |-> default]

(***************************************************************************)
(* Variables.                                                             *)
(*                                                                        *)
(* ~30 variables, grouped by purpose.  Each cluster has its own           *)
(* `<cluster>Vars` alias for UNCHANGED idioms in actions.                 *)
(***************************************************************************)

(* --- Phase / stage / cycle skeleton (5 vars) ---------------------------- *)
VARIABLES
    phase,
    stage,
    cycle,
    activeNode,
    activeCoarseNode

(* --- Structure (6 vars) ------------------------------------------------- *)
VARIABLES
    presentNodes,
    openNodes,
    coverage,
    approvedCoverage,
    \* Challenge-target coverage (kernel: `challenge_coverage` derived
    \* from `challenge_claims`).  The byte-conformance check on the
    \* claimed declaration text is observation-layer and not modeled;
    \* the spec models claim-update legality and coverage gating only.
    challengeCoverage,
    configuredTargets,
    approvedConfiguredTargets

(* --- Coarse DAG (1 var) ------------------------------------------------- *)
VARIABLES
    coarseDagNodes

(* --- Five lane status maps (4 over Nodes, 1 over Targets) --------------- *)
(*                                                                        *)
(* Lane reopen behavior is NOT symmetric across lanes; each lane composes *)
(* its fingerprint from different inputs, so dep-node edits propagate to *)
(* some lanes but not others.  Notably, substantivenessStatus is         *)
(* deliberately dep-independent (the kernel's SubstantivenessFingerprint *)
(* omits dep signal entirely).  See PROCESS_SEMANTICS.md §2.1 for the    *)
(* per-lane composition table and the rationale — do NOT extend the     *)
(* substantiveness reopen story to mirror correspondence/soundness.      *)
VARIABLES
    correspondenceStatus,
    substantivenessStatus,
    soundnessStatus,
    deviationStatus,
    faithfulnessStatus

(* --- Local-closure status (1 var, distinct from blocking — decision #6) - *)
VARIABLES
    localClosureStatus

(* --- Authorized scope envelope for the active worker dispatch ----------- *)
VARIABLES
    authorizedNodes

(* --- Gates (3 vars) ----------------------------------------------------- *)
VARIABLES
    gateKind,
    humanInputOutstanding,
    pendingProtectedReapproval,
    stagedUnderModelAssumptionDraft,
    pendingUnderModelAssumptions

(* --- Worker dispatch staging (4 vars) ----------------------------------- *)
VARIABLES
    hasPendingTask,
    pendingTaskKind,
    pendingTaskCarriers,
    workerMode

(* --- Three audit lane active flags ------------------------------------- *)
VARIABLES
    cleanupAuditActive,
    stuckMathAuditActive,
    needInputAuditorActive,
    assumptionLaneActive

(* --- GapResearch planner ↔ critic loop --------------------------------- *)
VARIABLES
    gapPlannerActive,
    gapCriticActive,
    gapRejectCount

(* --- Routing latches --------------------------------------------------- *)
VARIABLES
    postAdvanceRoutingPending,
    forceReviewAfterConeClean

(* --- Global repair lifecycle ------------------------------------------- *)
VARIABLES
    globalRepairStep

(* --- Closure history --------------------------------------------------- *)
VARIABLES
    cyclesSinceClean,
    hasEverBeenClean

(* --- In-flight request kind (sole wrapper boundary observable) --------- *)
VARIABLES
    inFlightRequestKind

(* --- Per-node backend target (Phase IV step 12) ------------------------ *)
(* Total map [Nodes -> Backends].  The kernel's `node_target` is a SPARSE   *)
(* override over `tablet_target`; the spec collapses both into a total      *)
(* function (a node with no override takes `TabletTarget`).  IMMUTABLE in   *)
(* step 12 (no action assigns it), so it is held UNCHANGED everywhere via   *)
(* `BackendVars`; per-node reassignment is a later phase.                   *)
VARIABLES
    nodeTarget

(* --- PV mode-B prove-or-disprove axis (forward design) ----------------- *)
(* `challengePolarity[t]`  : the LIVE side of Decide target t (default     *)
(*                           "prove"; only AcceptStuckMathAudit flips it). *)
(* `polarityFlips[t]`      : per-target flip counter (anti-thrash bound).  *)
(* `challengeClosedSide[t]`: closure record of the live side ("open" /     *)
(*                           "closed").  Closing is gated on the abstract  *)
(*                           truth model so a false statement can never    *)
(*                           record "closed".                              *)
VARIABLES
    challengePolarity,
    polarityFlips,
    challengeClosedSide

(***************************************************************************)
(* Cluster aliases.  Used for UNCHANGED in actions.                       *)
(***************************************************************************)

PhaseStageVars ==
    <<phase, stage, cycle, activeNode, activeCoarseNode>>

StructureVars ==
    <<presentNodes, openNodes, coverage, approvedCoverage,
      challengeCoverage, configuredTargets, approvedConfiguredTargets>>

CoarseDagVars == <<coarseDagNodes>>

LaneStatusVars ==
    <<correspondenceStatus, substantivenessStatus,
      soundnessStatus, deviationStatus, faithfulnessStatus>>

LocalClosureVars == <<localClosureStatus>>

AuthorizedScopeVars == <<authorizedNodes>>

GateVars ==
    <<gateKind, humanInputOutstanding, pendingProtectedReapproval,
      stagedUnderModelAssumptionDraft, pendingUnderModelAssumptions>>

PendingTaskVars ==
    <<hasPendingTask, pendingTaskKind, pendingTaskCarriers, workerMode>>

AuditLaneVars ==
    <<cleanupAuditActive, stuckMathAuditActive, needInputAuditorActive,
      assumptionLaneActive>>

GapResearchVars ==
    <<gapPlannerActive, gapCriticActive, gapRejectCount>>

RoutingLatchVars ==
    <<postAdvanceRoutingPending, forceReviewAfterConeClean>>

GlobalRepairVars == <<globalRepairStep>>

ClosureHistoryVars == <<cyclesSinceClean, hasEverBeenClean>>

InFlightVars == <<inFlightRequestKind>>

(* Phase IV step 12: immutable backend axis.  Folding `nodeTarget` into a   *)
(* single cluster lets every action hold it UNCHANGED with one conjunct;    *)
(* TLC reports a "variable not assigned" error for any action that omits    *)
(* it, machine-checking completeness.                                       *)
BackendVars == <<nodeTarget>>

(* PV mode-B polarity axis.  Like BackendVars, folding the three polarity   *)
(* vars into one cluster lets every action hold them UNCHANGED with one     *)
(* conjunct; TLC flags any action that omits one ("variable not assigned"), *)
(* machine-checking the sole-flipper discipline.                            *)
PolarityVars == <<challengePolarity, polarityFlips, challengeClosedSide>>

Vars ==
    <<phase, stage, cycle, activeNode, activeCoarseNode,
      presentNodes, openNodes, coverage, approvedCoverage,
      challengeCoverage, configuredTargets, approvedConfiguredTargets,
      coarseDagNodes,
      correspondenceStatus, substantivenessStatus, soundnessStatus,
      deviationStatus, faithfulnessStatus,
      localClosureStatus, authorizedNodes,
      gateKind, humanInputOutstanding, pendingProtectedReapproval,
      stagedUnderModelAssumptionDraft, pendingUnderModelAssumptions,
      hasPendingTask, pendingTaskKind, pendingTaskCarriers, workerMode,
      cleanupAuditActive, stuckMathAuditActive, needInputAuditorActive,
      assumptionLaneActive,
      gapPlannerActive, gapCriticActive, gapRejectCount,
      postAdvanceRoutingPending, forceReviewAfterConeClean,
      globalRepairStep,
      cyclesSinceClean, hasEverBeenClean,
      inFlightRequestKind,
      nodeTarget,
      challengePolarity, polarityFlips, challengeClosedSide>>

(***************************************************************************)
(* Derived predicates.                                                    *)
(***************************************************************************)

(* True iff lane `L`'s view of node/target `x` is decisive-Pass.          *)
LanePassNode(L, n) ==
    CASE L = "correspondence"   -> correspondenceStatus[n]   = "pass"
      [] L = "substantiveness"  -> substantivenessStatus[n]  = "pass"
      [] L = "soundness"        -> soundnessStatus[n]        = "pass"
      [] L = "deviation"        -> deviationStatus[n]        = "pass"
      [] OTHER                  -> FALSE

LanePassTarget(L, t) ==
    L = "faithfulness" /\ faithfulnessStatus[t] = "pass"

(* A node is "ok" iff every node-lane that applies to it is Pass and its  *)
(* local-closure record is verified.                                       *)
NodeIsOk(n) ==
    /\ correspondenceStatus[n]   = "pass"
    /\ substantivenessStatus[n]  = "pass"
    /\ soundnessStatus[n]        = "pass"
    /\ deviationStatus[n]        = "pass"
    /\ localClosureStatus[n]     = "verified"

(* `globalBlockers` is a derived set.  Each member is the carrier         *)
(* (node/target) that is non-Pass on at least one lane.                    *)
NodeBlocked(n) ==
    \/ correspondenceStatus[n]  # "pass"
    \/ substantivenessStatus[n] # "pass"
    \/ soundnessStatus[n]       # "pass"
    \/ deviationStatus[n]       # "pass"

TargetBlocked(t) ==
    faithfulnessStatus[t] # "pass"

(* ===================================================================== *)
(* PV mode-B prove-or-disprove derived predicates (forward design).      *)
(* ===================================================================== *)
(*                                                                       *)
(* A Decide challenge target t is a pair {T, ~T}. `challengePolarity[t]`  *)
(* names the LIVE side; the dormant side is suppressed. `challengeCoverage *)
(* [t]` is the live side's covering node-set (a node claims it, exactly  *)
(* like the math challenge-claim). `challengeClosedSide[t]` records       *)
(* whether the live covering node's PROOF has closed.                    *)
(*                                                                       *)
(* CloseSidePermitted(t, side) is the abstract AXIOM-FLOOR gate: a side   *)
(* may close only if it is TRUE under the abstract reality               *)
(* `ChallengeTruth[t]`.  Identical for "prove" and "disprove" — the gate  *)
(* is polarity-symmetric, which is what makes the disprove side as sound  *)
(* as the prove side.  Closing a false T or a false ~T is impossible.     *)
CloseSidePermitted(t, side) ==
    ChallengeTruth[t] = side

(* The live side has a covering node AND that covering proof has closed.  *)
ChallengeTargetDecided(t) ==
    /\ challengeCoverage[t] # {}
    /\ challengeClosedSide[t] = "closed"

(* A configured challenge target is a kernel-derived blocker (kernel:     *)
(* `BlockerKind::ChallengeCoverage`).  It has no verifier lane.  In the   *)
(* prove-or-disprove model it clears only when the LIVE side is DECIDED   *)
(* (covered AND closed) — never via the dormant side (CoverageOnLiveOnly *)
(* / NotBothSidesClosed).                                                 *)
(*                                                                       *)
(* PHASE-GATING: a challenge target is a blocker in proof_formalization   *)
(* and cleanup (where the proving happens and where completion is gated), *)
(* but NOT in theorem_stating.  The prove-vs-disprove DIRECTION is        *)
(* discovered DURING proof_formalization (the audit polarity flip lives   *)
(* there), so gating the theorem_stating->proof_formalization advance on  *)
(* "already decided" would deadlock a target whose default Prove side is   *)
(* false — it could never reach the phase where it gets flipped.  This    *)
(* mirrors Substantiveness's phase-dormancy.  Coverage/decision is still  *)
(* required for COMPLETION (cleanup -> complete via ReviewDone, which      *)
(* requires GlobalBlockers = {} with the challenge target now active).    *)
ChallengeTargetBlocked(t) ==
    /\ phase \in {"proof_formalization", "cleanup"}
    /\ ~ ChallengeTargetDecided(t)

(***************************************************************************)
(* `globalBlockers` mirrors the kernel's derived blocker set.  It is a    *)
(* set of {nodes ∪ targets} carriers — anyone whose lane verdict is non-  *)
(* Pass.  Substantiveness is phase-dormant in `cleanup` and `complete`,   *)
(* matching the kernel.                                                    *)
(***************************************************************************)
NodeBlockersActive(n) ==
    LET corrFail   == correspondenceStatus[n] # "pass"
        soundFail  == soundnessStatus[n]      # "pass"
        devFail    == deviationStatus[n]      # "pass"
        subFail    == /\ phase \in {"theorem_stating", "proof_formalization"}
                      /\ substantivenessStatus[n] # "pass"
    IN corrFail \/ soundFail \/ devFail \/ subFail

GlobalBlockers ==
    {n \in presentNodes : NodeBlockersActive(n)}
        \cup
    {t \in configuredTargets : TargetBlocked(t)}
        \cup
    {t \in ChallengeTargets : ChallengeTargetBlocked(t)}

CoreSupportRoots(liveCoverage, liveChallengeCoverage) ==
    UNION {liveCoverage[t] : t \in Targets}
        \cup
    UNION {liveChallengeCoverage[c] : c \in ChallengeTargets}

CoreOrphanFree(livePresent, liveCoverage, liveChallengeCoverage) ==
    \A n \in livePresent :
        \/ n = "Preamble"
        \/ n \in CoreSupportRoots(liveCoverage, liveChallengeCoverage)

CoreOrphanNodes(livePresent, liveCoverage, liveChallengeCoverage) ==
    {n \in livePresent :
        /\ n # "Preamble"
        /\ n \notin CoreSupportRoots(liveCoverage, liveChallengeCoverage)}

AcceptedValidWorkerOrphanContract(deletedNodes, orphanedByDelta, livePresent, liveCoverage, liveChallengeCoverage) ==
    \* `deletedNodes` is the explicit worker response field. The core
    \* spec abstracts the concrete import-closure calculation as
    \* `orphanedByDelta`; SupervisorProtocol.tla refines it to the exact
    \* same-burst orphan set.
    /\ deletedNodes \subseteq presentNodes
    /\ deletedNodes = orphanedByDelta
    /\ CoreOrphanFree(livePresent, liveCoverage, liveChallengeCoverage)

\* ------------------------------------------------------------------------
\* Verifier lane-ordering gate.  Mirrors the kernel's dispatch frontier
\* (model.rs corr_verify_nodes / sound_verify_nodes / corr_blockers_exist)
\* and the big SupervisorProtocol spec (CorrVerifyNodes / SoundVerifyNodes /
\* CorrespondenceBlockersExist): Correspondence dispatch needs the node's
\* Substantiveness Pass (its paper basis); Soundness dispatch needs every
\* other lane globally clear.  Parameterised over the status functions so
\* the AcceptWorker / AcceptVerifier guards can pass *primed* variables as
\* arguments (used to evaluate eligibility on the post-accept state).
SubstantivenessSatisfiedForCorr(n, subS) ==
    \/ subS[n] = "pass"
    \/ /\ GoalMode = "lean"
       /\ n = AssumptionsNode

\* Abstract body-shape contract for worker-authored under-model drafts.
\* Formula bodies stay abstract: setup seeded the approved Rust-validity
\* predicate hook, and this flag marks a worker-authored `.spec` / `.tex`
\* property axiom conditional on that hook.
UnderModelAssumptionShapeOK == TRUE

UnderModelAssumptionDraftReadyOn(pres, corrS, draftExists) ==
    /\ GoalMode = "lean"
    /\ AssumptionsNode \in pres
    /\ draftExists
    /\ corrS[AssumptionsNode] = "unknown"
    /\ UnderModelAssumptionShapeOK

UnderModelDraftStageableOn(pres, corrS) ==
    /\ GoalMode = "lean"
    /\ AssumptionsNode \in pres
    /\ corrS[AssumptionsNode] = "unknown"

CorrBlockersOn(pres, tgts, corrS, faithS, subS, devS, ph) ==
    \/ \E n \in pres : corrS[n]  # "pass"
    \/ \E t \in tgts : faithS[t] # "pass"
    \/ /\ ph \in {"theorem_stating", "proof_formalization"}
       /\ \E n \in pres : ~ SubstantivenessSatisfiedForCorr(n, subS)
    \/ \E n \in pres : devS[n] # "pass"

CorrDispatchableOn(pres, corrS, subS, ph, draftExists) ==
    \E n \in pres :
        /\ corrS[n] = "unknown"
        /\ (n # AssumptionsNode
             \/ UnderModelAssumptionDraftReadyOn(pres, corrS, draftExists))
        /\ (ph \in {"theorem_stating", "proof_formalization"}
                => SubstantivenessSatisfiedForCorr(n, subS))

SoundDispatchableOn(pres, tgts, corrS, faithS, subS, devS, soundS, ph) ==
    /\ ~ CorrBlockersOn(pres, tgts, corrS, faithS, subS, devS, ph)
    /\ \E n \in pres :
        /\ soundS[n] = "unknown"
        /\ corrS[n]  = "pass"
        /\ subS[n]   = "pass"

(* TheoremStating-phase blockers excluding Soundness (which is partly     *)
(* relaxed in TheoremStating — see PROCESS_SEMANTICS §2).                 *)

(***************************************************************************)
(* `formalizationComplete` mirrors the kernel's `formalization_complete`. *)
(* The four clauses are:                                                  *)
(*   1. textual clean: every proof node is closed (none in openNodes);    *)
(*   2. blockers clean: globalBlockers is empty;                          *)
(*   3. local-closure clean: no present node is localClosureUnverified;   *)
(*   4. records present: every present node has a closure-status entry —  *)
(*      vacuous here because we model `localClosureStatus` as total over  *)
(*      Nodes.                                                            *)
(***************************************************************************)
FormalizationComplete ==
    /\ presentNodes \cap openNodes = {}
    /\ GlobalBlockers = {}
    /\ \A n \in presentNodes : localClosureStatus[n] = "verified"

(* ActiveCoarseChangeAllowed is conservative here: change is allowed     *)
(* when the anchor mechanism is dormant (no coarse DAG) or there is no    *)
(* anchor yet, or the run is at a quiescent post-clean state.  The big   *)
(* spec also has a starvation guard; here non-determinism in              *)
(* `ReviewContinue` covers that escape.                                   *)
ActiveCoarseChangeAllowed ==
    \/ coarseDagNodes = {}
    \/ activeCoarseNode = NoNode
    \/ /\ activeCoarseNode \in coarseDagNodes
       /\ GlobalBlockers = {}

(***************************************************************************)
(* Initial state.  Every variable starts in its dormant value.            *)
(***************************************************************************)
Init ==
    /\ phase = "theorem_stating"
    /\ stage = "Start"
    /\ cycle = 0
    /\ activeNode = NoNode
    /\ activeCoarseNode = NoNode
    /\ presentNodes =
          InitialPresentNodes
              \cup (IF GoalMode = "lean" THEN {AssumptionsNode} ELSE {})
    /\ openNodes = {}
    /\ coverage = [t \in Targets |-> {}]
    /\ approvedCoverage = [t \in Targets |-> {}]
    /\ challengeCoverage = [t \in ChallengeTargets |-> {}]
    /\ configuredTargets = InitialConfiguredTargets
    /\ approvedConfiguredTargets = {}
    /\ coarseDagNodes = {}
    /\ correspondenceStatus  =
          [n \in Nodes |-> IF GoalMode = "lean" /\ n = AssumptionsNode
                            THEN "pass" ELSE "unknown"]
       \* Fresh empty Tablet/Assumptions.{lean,tex} is vacuous; a later worker
       \* edit drifts AssumptionsNode correspondence back to unknown.
    /\ substantivenessStatus =
          [n \in Nodes |-> IF GoalMode = "lean" /\ n = AssumptionsNode
                            THEN "pass" ELSE "unknown"]
    /\ soundnessStatus       = NoStatus("unknown")
    /\ deviationStatus       = NoStatus("pass")
       \* deviation default is `pass` because the kernel's contract is
       \* "no deviation declared ⇒ no blocker".  The abstract status map
       \* is total over Nodes; non-claiming nodes contribute no blocker.
    /\ faithfulnessStatus    = NoTargetStatus("unknown")
    /\ localClosureStatus    = [n \in Nodes |-> "verified"]
       \* In TheoremStating, no closure record is needed (no sorry-free
       \* proof nodes yet); the dormant `verified` value matches.
    /\ authorizedNodes = {}
    /\ gateKind = "none"
    /\ humanInputOutstanding = FALSE
    /\ pendingProtectedReapproval = {}
    /\ stagedUnderModelAssumptionDraft = FALSE
    /\ pendingUnderModelAssumptions = 0
    /\ hasPendingTask = FALSE
    /\ pendingTaskKind = "none"
    /\ pendingTaskCarriers = {}
    /\ workerMode = "local"
    /\ cleanupAuditActive = FALSE
    /\ stuckMathAuditActive = FALSE
    /\ needInputAuditorActive = FALSE
    /\ assumptionLaneActive = FALSE
    /\ gapPlannerActive = FALSE
    /\ gapCriticActive = FALSE
    /\ gapRejectCount = 0
    /\ postAdvanceRoutingPending = FALSE
    /\ forceReviewAfterConeClean = FALSE
    /\ globalRepairStep = "none"
    /\ cyclesSinceClean = 0
    /\ hasEverBeenClean = FALSE
    /\ inFlightRequestKind = "none"
    /\ nodeTarget = [n \in Nodes |-> TabletTarget]
       \* All-Lean default: every node takes the tablet-wide target (the
       \* kernel's empty `node_target` ⇒ `effective_node_target` = tablet_target).
    /\ challengePolarity = [t \in ChallengeTargets |-> "prove"]
       \* Every Decide target starts on the Prove side (default polarity);
       \* only the audit lane may flip it (AcceptStuckMathAudit).
    /\ polarityFlips = [t \in ChallengeTargets |-> 0]
    /\ challengeClosedSide = [t \in ChallengeTargets |-> "open"]

(***************************************************************************)
(* Helper: clear all latest-frontier / scope artifacts when a cycle ends. *)
(* The core spec doesn't maintain a frontier; this is a stub for symmetry *)
(* with the big spec.                                                     *)
(***************************************************************************)
ClearAuthorizedScope ==
    authorizedNodes' = {}

(* When a Worker burst is accepted with an outcome that isn't Valid, the *)
(* spec's local-closure tier may flip back to `unverified` on any present *)
(* node — the kernel models this via the closure record's fingerprint    *)
(* drift; the abstract analog is "any subset of present nodes may become *)
(* unverified".                                                           *)

(***************************************************************************)
(* ----------------------- ACTIONS -------------------------------------- *)
(***************************************************************************)

(* Priority ladder.  At cycle start, the kernel chooses the next request *)
(* kind by consulting state.  The spec abstracts the priority ladder as a *)
(* non-deterministic choice over the legal first-stage targets, with     *)
(* `forceReviewAfterConeClean` and `postAdvanceRoutingPending` taking    *)
(* precedence (cf. PROCESS_SEMANTICS §4.1).                              *)
StartCycle ==
    /\ stage = "Start"
    /\ phase # "complete"
    /\ inFlightRequestKind = "none"
    /\ cycle < MaxCycle
    /\ cycle' = cycle + 1
    /\ \/ \* Routing-latch precedence: route to Reviewer.
          /\ \/ postAdvanceRoutingPending
             \/ forceReviewAfterConeClean
          /\ stage' = "Reviewer"
          /\ inFlightRequestKind' = "reviewer"
          /\ postAdvanceRoutingPending' = FALSE
          /\ forceReviewAfterConeClean' = FALSE
          /\ UNCHANGED AuditLaneVars
       \/ \* Cleanup phase begins with a CleanupAudit burst when no
          \* worker task is pending and the audit lane is freshly opened.
          /\ phase = "cleanup"
          /\ ~ postAdvanceRoutingPending
          /\ ~ forceReviewAfterConeClean
          /\ ~ hasPendingTask
          /\ \/ /\ ~ cleanupAuditActive
                /\ stage' = "CleanupAudit"
                /\ inFlightRequestKind' = "cleanup_audit"
                /\ cleanupAuditActive' = TRUE
                /\ UNCHANGED <<stuckMathAuditActive, needInputAuditorActive,
                               assumptionLaneActive>>
                /\ UNCHANGED <<postAdvanceRoutingPending,
                               forceReviewAfterConeClean>>
             \/ /\ cleanupAuditActive
                /\ stage' = "Reviewer"
                /\ inFlightRequestKind' = "reviewer"
                /\ UNCHANGED AuditLaneVars
                /\ UNCHANGED <<postAdvanceRoutingPending,
                               forceReviewAfterConeClean>>
       \/ \* Ordinary worker dispatch (or StuckMathAudit substitution).
          /\ ~ postAdvanceRoutingPending
          /\ ~ forceReviewAfterConeClean
          /\ phase \in {"theorem_stating", "proof_formalization", "cleanup"}
          /\ \/ /\ stuckMathAuditActive
                /\ phase = "proof_formalization"
                /\ stage' = "StuckMathAudit"
                /\ inFlightRequestKind' = "stuck_math_audit"
                /\ UNCHANGED AuditLaneVars
                /\ UNCHANGED <<postAdvanceRoutingPending,
                               forceReviewAfterConeClean>>
             \/ /\ stage' = "Worker"
                /\ inFlightRequestKind' = "worker"
                /\ UNCHANGED AuditLaneVars
                /\ UNCHANGED <<postAdvanceRoutingPending,
                               forceReviewAfterConeClean>>
    /\ UNCHANGED <<phase, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED GateVars
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* IssueRequest(kind).  In the big spec, request issuance and stage      *)
(* transition are split across separate actions ("Issue<X>Request" then  *)
(* "EnvStage<X>Artifact" then "Accept<X>Artifact").  Here we merge them: *)
(* the role's response is reasoned about atomically by the Accept*       *)
(* actions, which already include the "stage-and-clear" step.            *)
(*                                                                        *)
(* StartCycle is the sole "issue" action because it owns the stage       *)
(* transition into Worker / Reviewer / CleanupAudit / StuckMathAudit.    *)
(* Subsequent role transitions are owned by the Accept* actions and the  *)
(* Review* actions.                                                       *)
(***************************************************************************)

(***************************************************************************)
(* AcceptWorker.  The four worker outcomes branch the next stage.        *)
(* Worker mode and outcome interact: Local mode plus a Valid outcome may *)
(* close the active node's proof; Restructure / CoarseRestructure may    *)
(* add or remove nodes.                                                   *)
(*                                                                        *)
(* The kernel runs the verifier drain (Faithfulness → Substantiveness →  *)
(* Correspondence → Soundness) after a Valid-with-delta worker.  In the  *)
(* abstract spec, we route to a verifier stage subject to the lane-       *)
(* ordering gate (CorrDispatchableOn / SoundDispatchableOn above);        *)
(* finer panel sequencing remains a sub-refinement.                       *)
(*                                                                        *)
(* AcceptWorker takes a worker outcome non-deterministically (the kernel *)
(* has retry counters; the spec doesn't model them).                     *)
(***************************************************************************)

(* Helpers for worker delta: a worker delta either drifts a single node's *)
(* status maps back to unknown (kernel: fingerprint drift invalidates a   *)
(* prior verdict), closes a single open node (`openNodes' = openNodes \  *)
(* {n}`), or marks a single node's local-closure record unverified.      *)
(* Multi-node deltas are out of scope: TLC sim runs would multiply the   *)
(* successor count without adding semantic coverage.                      *)

DriftNodeStatusesToUnknown(n) ==
    /\ correspondenceStatus'   = [correspondenceStatus   EXCEPT ![n] = "unknown"]
    /\ substantivenessStatus'  = [substantivenessStatus  EXCEPT ![n] = "unknown"]
    /\ soundnessStatus'        = [soundnessStatus        EXCEPT ![n] = "unknown"]
    /\ deviationStatus'        = [deviationStatus        EXCEPT ![n] = "unknown"]
    /\ UNCHANGED faithfulnessStatus

DriftTargetFaithfulnessToUnknown(t) ==
    /\ faithfulnessStatus' = [faithfulnessStatus EXCEPT ![t] = "unknown"]
    /\ UNCHANGED <<correspondenceStatus, substantivenessStatus,
                   soundnessStatus, deviationStatus>>

(***************************************************************************)
(* AcceptWorker. The four worker outcomes branch the next stage.          *)
(* Worker mode and outcome interact: Local mode plus a Valid outcome may *)
(* close the active node's proof; Restructure / CoarseRestructure may    *)
(* add or remove nodes.                                                   *)
(*                                                                        *)
(* The kernel runs the verifier drain (Faithfulness → Substantiveness →  *)
(* Correspondence → Soundness) after a Valid-with-delta worker.  In the  *)
(* abstract spec, we route to a verifier stage subject to the lane-       *)
(* ordering gate (CorrDispatchableOn / SoundDispatchableOn above);        *)
(* finer panel sequencing remains a sub-refinement.                       *)
(*                                                                        *)
(* AcceptWorker takes a worker outcome non-deterministically (the kernel *)
(* has retry counters; the spec doesn't model them).                     *)
(***************************************************************************)
AcceptWorker ==
    /\ stage = "Worker"
    /\ inFlightRequestKind = "worker"
    /\ \/ \* Valid worker with semantic delta — routes to a verifier
          \* stage; the worker delta may also drift lane statuses or
          \* close a node.  Verifier request issuance is folded into
          \* the worker accept (the big spec has a separate
          \* IssueVerifierRequest action).
          /\ \/ /\ stage' = "VerifyFaithfulness"
                /\ inFlightRequestKind' = "verifier_faithfulness"
             \/ /\ stage' = "VerifySubstantiveness"
                /\ inFlightRequestKind' = "verifier_substantiveness"
             \/ /\ stage' = "VerifyCorrespondence"
                /\ inFlightRequestKind' = "verifier_correspondence"
             \/ /\ stage' = "VerifySoundness"
                /\ inFlightRequestKind' = "verifier_soundness"
             \/ /\ stage' = "VerifyDeviation"
                /\ inFlightRequestKind' = "verifier_deviation"
          /\ \/ \* No structural delta — lane status drift only.
                /\ UNCHANGED StructureVars
                /\ AcceptedValidWorkerOrphanContract({}, {}, presentNodes, coverage, challengeCoverage)
                /\ \E n \in presentNodes : DriftNodeStatusesToUnknown(n)
                /\ UNCHANGED localClosureStatus
                /\ UNCHANGED PolarityVars
             \/ \* No structural delta — single target faithfulness drift.
                /\ UNCHANGED StructureVars
                /\ AcceptedValidWorkerOrphanContract({}, {}, presentNodes, coverage, challengeCoverage)
                /\ \E t \in configuredTargets :
                       DriftTargetFaithfulnessToUnknown(t)
                /\ UNCHANGED localClosureStatus
                /\ UNCHANGED PolarityVars
             \/ \* Node closure: removes a single sorry from openNodes.
                /\ \E n \in openNodes :
                    /\ openNodes' = openNodes \ {n}
                    /\ presentNodes' = presentNodes
                    /\ coverage' = coverage
                /\ UNCHANGED <<approvedCoverage, challengeCoverage,
                               configuredTargets,
                               approvedConfiguredTargets>>
                /\ AcceptedValidWorkerOrphanContract({}, {}, presentNodes', coverage', challengeCoverage')
                /\ UNCHANGED LaneStatusVars
                /\ UNCHANGED localClosureStatus
             \/ \* Same-burst orphan deletion: the response declares the
                \* abstract orphan set made removable by the same valid
                \* delta; SupervisorProtocol.tla defines the concrete set.
                /\ \E deletedNodes \in SUBSET presentNodes :
                    /\ deletedNodes # {}
                    /\ activeNode \notin deletedNodes
                    /\ activeCoarseNode \notin deletedNodes
                    /\ deletedNodes \cap authorizedNodes = {}
                    /\ presentNodes' = presentNodes \ deletedNodes
                    /\ openNodes' = openNodes \ deletedNodes
                    /\ coverage' = [t \in Targets |-> coverage[t] \ deletedNodes]
                    /\ challengeCoverage' =
                            [c \in ChallengeTargets |-> challengeCoverage[c] \ deletedNodes]
                    /\ AcceptedValidWorkerOrphanContract(
                           deletedNodes,
                           CoreOrphanNodes(presentNodes, coverage', challengeCoverage') \cap deletedNodes,
                           presentNodes',
                           coverage',
                           challengeCoverage'
                       )
                /\ UNCHANGED <<approvedCoverage, configuredTargets,
                               approvedConfiguredTargets>>
                /\ UNCHANGED LaneStatusVars
                /\ UNCHANGED localClosureStatus
                /\ UNCHANGED PolarityVars
             \/ \* Challenge claim: a present node claims an uncovered
                \* challenge target's LIVE side.  Claim-update legality: one
                \* claiming node per target and one target per node
                \* (kernel: exclusive claim + name parity; the byte
                \* check is observation-layer).  A claim covers but does NOT
                \* close — closure is the separate challenge-close disjunct.
                /\ \E t \in ChallengeTargets, n \in presentNodes :
                       /\ challengeCoverage[t] = {}
                       /\ \A t2 \in ChallengeTargets :
                              n \notin challengeCoverage[t2]
                       /\ challengeCoverage' =
                              [challengeCoverage EXCEPT ![t] = {n}]
                /\ UNCHANGED <<presentNodes, openNodes, coverage,
                               approvedCoverage, configuredTargets,
                               approvedConfiguredTargets>>
                /\ AcceptedValidWorkerOrphanContract({}, {}, presentNodes', coverage', challengeCoverage')
                /\ UNCHANGED LaneStatusVars
                /\ UNCHANGED localClosureStatus
                /\ UNCHANGED PolarityVars
             \/ \* Challenge close (PV mode-B): the LIVE side's covering node
                \* closes its proof.  AXIOM-FLOOR GATE: closing is permitted
                \* only when the live side is TRUE under the abstract reality
                \* (`CloseSidePermitted`) — identical for "prove" and
                \* "disprove".  This is what makes closing a false statement
                \* impossible on either polarity (SymmetricAxioms /
                \* SoundnessFloor).  The dormant side is never closed
                \* (CoverageOnLiveOnly / NotBothSidesClosed).
                /\ \E t \in ChallengeTargets :
                       /\ challengeCoverage[t] # {}
                       /\ challengeClosedSide[t] = "open"
                       /\ CloseSidePermitted(t, challengePolarity[t])
                       /\ challengeClosedSide' =
                              [challengeClosedSide EXCEPT ![t] = "closed"]
                /\ UNCHANGED <<challengePolarity, polarityFlips>>
                /\ UNCHANGED StructureVars
                /\ UNCHANGED LaneStatusVars
                /\ UNCHANGED localClosureStatus
             \/ \* Worker invalidates a node's local-closure record
                \* (kernel: dep-edit fingerprint drift).
                /\ UNCHANGED StructureVars
                /\ AcceptedValidWorkerOrphanContract({}, {}, presentNodes, coverage, challengeCoverage)
                /\ UNCHANGED LaneStatusVars
                /\ \E n \in presentNodes :
                    /\ n \notin openNodes
                    /\ localClosureStatus' =
                            [localClosureStatus EXCEPT ![n] = "unverified"]
                /\ UNCHANGED PolarityVars
       \/ \* Valid-without-delta — routes directly to Reviewer.
          /\ stage' = "Reviewer"
          /\ inFlightRequestKind' = "reviewer"
          /\ UNCHANGED StructureVars
          /\ AcceptedValidWorkerOrphanContract({}, {}, presentNodes, coverage, challengeCoverage)
          /\ UNCHANGED LaneStatusVars
          /\ UNCHANGED LocalClosureVars
          /\ UNCHANGED PolarityVars
       \/ \* Invalid or Stuck — re-issue worker (retry) or escalate
          \* to Reviewer.  The spec collapses retry counters away.
          /\ \/ /\ stage' = "Worker"
                /\ inFlightRequestKind' = "worker"
             \/ /\ stage' = "Reviewer"
                /\ inFlightRequestKind' = "reviewer"
          /\ UNCHANGED StructureVars
          /\ UNCHANGED LaneStatusVars
          /\ UNCHANGED LocalClosureVars
          /\ UNCHANGED PolarityVars
       \/ \* NeedsRestructure / target_false_under_model — always to
          \* Reviewer.  The latter is PV-only and keeps its diagnostic
          \* witness at the implementation layer; the core model abstracts
          \* that payload away.
          /\ stage' = "Reviewer"
          /\ inFlightRequestKind' = "reviewer"
          /\ UNCHANGED StructureVars
          /\ UNCHANGED LaneStatusVars
          /\ UNCHANGED LocalClosureVars
          /\ UNCHANGED PolarityVars
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED <<gateKind, humanInputOutstanding,
                   pendingProtectedReapproval, pendingUnderModelAssumptions>>
    /\ stagedUnderModelAssumptionDraft' \in
          IF UnderModelDraftStageableOn(presentNodes', correspondenceStatus')
          THEN {stagedUnderModelAssumptionDraft, TRUE}
          ELSE {stagedUnderModelAssumptionDraft}
    \* Verifier lane-ordering gate: Correspondence / Soundness dispatch
    \* only when the next lane is actually eligible (see CorrDispatchableOn /
    \* SoundDispatchableOn).  Evaluated on the post-accept (primed) state.
    /\ (stage' = "VerifyCorrespondence") =>
           CorrDispatchableOn(presentNodes', correspondenceStatus',
                              substantivenessStatus', phase',
                              stagedUnderModelAssumptionDraft')
    /\ (stage' = "VerifySoundness") =>
           SoundDispatchableOn(presentNodes', configuredTargets',
                               correspondenceStatus', faithfulnessStatus',
                               substantivenessStatus', deviationStatus',
                               soundnessStatus', phase')
    /\ \* Worker acceptance consumes the pending task.  If no task was
       \* pending (e.g. a forced-routing initial Worker dispatch), the
       \* hasPendingTask flag stays false; either way it's not TRUE
       \* after acceptance.
       /\ hasPendingTask' = FALSE
       /\ pendingTaskKind' = "none"
       /\ pendingTaskCarriers' = {}
       /\ UNCHANGED workerMode
    /\ UNCHANGED AuditLaneVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    \* PolarityVars handled per-branch above (challenge-claim / challenge-
    \* close set/hold the polarity cluster explicitly; all other branches
    \* hold it UNCHANGED).

(***************************************************************************)
(* AcceptVerifier(lane).  A single action parameterized by lane.  A      *)
(* verifier panel votes on its frontier; the abstract spec models the    *)
(* effect as "lane status for any subset of carriers may flip to pass,   *)
(* fail, or stay unknown".  The kernel's panel reconciliation, split-   *)
(* unknown propagation, and "approved fingerprint pinned on decisive    *)
(* verdict" are all collapsed.                                            *)
(*                                                                        *)
(* Sequencing: after Faithfulness or Substantiveness, the kernel may stay *)
(* in the Paper stage to drain the other half; the spec takes one stage- *)
(* per-Accept step.  The next stage may be any verifier stage or         *)
(* Reviewer.                                                              *)
(***************************************************************************)
AcceptVerifier ==
    /\ stage \in {"VerifyFaithfulness", "VerifySubstantiveness",
                  "VerifyCorrespondence", "VerifySoundness",
                  "VerifyDeviation"}
    /\ inFlightRequestKind \in
            {"verifier_faithfulness", "verifier_substantiveness",
             "verifier_correspondence", "verifier_soundness",
             "verifier_deviation"}
    /\ \/ stage' = "VerifyFaithfulness"
       \/ stage' = "VerifySubstantiveness"
       \/ stage' = "VerifyCorrespondence"
       \/ stage' = "VerifySoundness"
       \/ stage' = "VerifyDeviation"
       \/ stage' = "Reviewer"
    /\ \/ /\ stage' \in {"VerifyFaithfulness", "VerifySubstantiveness",
                         "VerifyCorrespondence", "VerifySoundness",
                         "VerifyDeviation"}
          /\ inFlightRequestKind' \in
                {"verifier_faithfulness", "verifier_substantiveness",
                 "verifier_correspondence", "verifier_soundness",
                 "verifier_deviation"}
          /\ \* Next request kind must match next stage (InFlightKindMatchesStage).
             /\ (stage' = "VerifyFaithfulness")    =>
                    (inFlightRequestKind' = "verifier_faithfulness")
             /\ (stage' = "VerifySubstantiveness") =>
                    (inFlightRequestKind' = "verifier_substantiveness")
             /\ (stage' = "VerifyCorrespondence")  =>
                    (inFlightRequestKind' = "verifier_correspondence")
             /\ (stage' = "VerifySoundness")       =>
                    (inFlightRequestKind' = "verifier_soundness")
             /\ (stage' = "VerifyDeviation")       =>
                    (inFlightRequestKind' = "verifier_deviation")
       \/ /\ stage' = "Reviewer"
          /\ inFlightRequestKind' = "reviewer"
    /\ \* One single node's lane status flips (Pass / Fail / Unknown)
       \* — the abstraction of a lane vote.  Multi-node verdicts are
       \* off-spec (the kernel runs one per request).  The "no-op" branch
       \* allows empty-frontier panels to return without producing a
       \* status update (which fires when presentNodes or configuredTargets
       \* shrinks mid-cycle).
       \/ /\ stage = "VerifyCorrespondence"
          /\ \/ \E n \in presentNodes, v \in LaneStatuses :
                  /\ (n # AssumptionsNode
                       \/ UnderModelAssumptionDraftReadyOn(presentNodes,
                                                          correspondenceStatus,
                                                          stagedUnderModelAssumptionDraft))
                  /\ ~(GoalMode = "lean" /\ n = AssumptionsNode /\ v = "pass")
                  /\ correspondenceStatus' =
                          [correspondenceStatus EXCEPT ![n] = v]
             \/ /\ presentNodes = {}
                /\ UNCHANGED correspondenceStatus
          /\ UNCHANGED <<substantivenessStatus, soundnessStatus,
                         deviationStatus, faithfulnessStatus>>
       \/ /\ stage = "VerifySubstantiveness"
          /\ \/ \E n \in presentNodes, v \in LaneStatuses :
                  substantivenessStatus' =
                          [substantivenessStatus EXCEPT ![n] = v]
             \/ /\ presentNodes = {}
                /\ UNCHANGED substantivenessStatus
          /\ UNCHANGED <<correspondenceStatus, soundnessStatus,
                         deviationStatus, faithfulnessStatus>>
       \/ /\ stage = "VerifySoundness"
          /\ \/ \E n \in presentNodes, v \in LaneStatuses :
                  soundnessStatus' =
                          [soundnessStatus EXCEPT ![n] = v]
             \/ /\ presentNodes = {}
                /\ UNCHANGED soundnessStatus
          /\ UNCHANGED <<correspondenceStatus, substantivenessStatus,
                         deviationStatus, faithfulnessStatus>>
       \/ /\ stage = "VerifyDeviation"
          /\ \/ \E n \in presentNodes, v \in LaneStatuses :
                  deviationStatus' =
                          [deviationStatus EXCEPT ![n] = v]
             \/ /\ presentNodes = {}
                /\ UNCHANGED deviationStatus
          /\ UNCHANGED <<correspondenceStatus, substantivenessStatus,
                         soundnessStatus, faithfulnessStatus>>
       \/ /\ stage = "VerifyFaithfulness"
          /\ \/ \E t \in configuredTargets, v \in LaneStatuses :
                  faithfulnessStatus' =
                          [faithfulnessStatus EXCEPT ![t] = v]
             \/ /\ configuredTargets = {}
                /\ UNCHANGED faithfulnessStatus
          /\ UNCHANGED <<correspondenceStatus, substantivenessStatus,
                         soundnessStatus, deviationStatus>>
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED GateVars
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED AuditLaneVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars
    \* Verifier lane-ordering gate: Correspondence / Soundness dispatch
    \* only when the next lane is actually eligible (see CorrDispatchableOn /
    \* SoundDispatchableOn).  Evaluated on the post-accept (primed) state.
    /\ (stage' = "VerifyCorrespondence") =>
           CorrDispatchableOn(presentNodes', correspondenceStatus',
                              substantivenessStatus', phase',
                              stagedUnderModelAssumptionDraft')
    /\ (stage' = "VerifySoundness") =>
           SoundDispatchableOn(presentNodes', configuredTargets',
                               correspondenceStatus', faithfulnessStatus',
                               substantivenessStatus', deviationStatus',
                               soundnessStatus', phase')

(***************************************************************************)
(* AcceptAssumptionsCorrPass.  The worker-authored staged blocks in the  *)
(* dedicated Assumptions node are gated by ordinary Correspondence first. *)
(* A Corr Fail on Assumptions is handled by the generic AcceptVerifier    *)
(* branch above; only a Corr Pass dispatches the assumptions sub-lane of  *)
(* StuckMathAudit.                                                       *)
(***************************************************************************)
AcceptAssumptionsCorrPass ==
    /\ stage = "VerifyCorrespondence"
    /\ inFlightRequestKind = "verifier_correspondence"
    /\ UnderModelAssumptionDraftReadyOn(presentNodes, correspondenceStatus,
                                       stagedUnderModelAssumptionDraft)
    /\ correspondenceStatus' =
          [correspondenceStatus EXCEPT ![AssumptionsNode] = "pass"]
    /\ UNCHANGED <<substantivenessStatus, soundnessStatus,
                   deviationStatus, faithfulnessStatus>>
    /\ stage' = "StuckMathAudit"
    /\ inFlightRequestKind' = "stuck_math_audit"
    /\ stuckMathAuditActive' = TRUE
    /\ assumptionLaneActive' = TRUE
    /\ UNCHANGED <<cleanupAuditActive, needInputAuditorActive>>
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED GateVars
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* AcceptCleanupAudit.  The cleanup-v2 audit role's response either      *)
(* needs to continue (next burst) or is done (transition to Reviewer).    *)
(* The spec abstracts away the per-task lifecycle (cleanupAuditTasks);   *)
(* the only thing it observes is the active flag flipping off on done.    *)
(***************************************************************************)
AcceptCleanupAudit ==
    /\ stage = "CleanupAudit"
    /\ inFlightRequestKind = "cleanup_audit"
    /\ phase = "cleanup"
    /\ \E auditDone \in BOOLEAN :
        \/ /\ ~ auditDone
           /\ stage' = "CleanupAudit"
           /\ inFlightRequestKind' = "cleanup_audit"
           /\ UNCHANGED AuditLaneVars
        \/ /\ auditDone
           /\ stage' = "Reviewer"
           /\ inFlightRequestKind' = "reviewer"
           /\ cleanupAuditActive' = FALSE
           /\ UNCHANGED <<stuckMathAuditActive, needInputAuditorActive,
                          assumptionLaneActive>>
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED GateVars
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* AcceptStuckMathAudit.  StuckMathAudit returns to Reviewer (and may    *)
(* fire a cone-clean reset, setting forceReviewAfterConeClean).          *)
(*                                                                       *)
(* PV mode-B: the audit lane is the SOLE FLIPPER of a Decide target's    *)
(* live polarity.  It may flip one target's live side Prove<->Disprove   *)
(* (reversible — it may flip back, and idempotent in effect: re-flipping  *)
(* to the same side is a no-op of the FlipPolarity disjunct since toggle  *)
(* always changes the value), bounded by `MaxPolarityFlips` to prevent    *)
(* thrash.  A flip RESETS the new live side's closure record to "open"    *)
(* and clears its coverage (the prior claim covered the OTHER side).  No  *)
(* other action touches `challengePolarity` — every other action holds    *)
(* PolarityVars UNCHANGED, which IS the AuditSoleFlipper invariant.       *)
(***************************************************************************)
PolarityFlip(s) == IF s = "prove" THEN "disprove" ELSE "prove"

AcceptStuckMathAudit ==
    /\ stage = "StuckMathAudit"
    /\ inFlightRequestKind = "stuck_math_audit"
    /\ phase = "proof_formalization"
    /\ ~ assumptionLaneActive
    \* When the global-repair lane has a request pending, the
    \* AcceptGlobalRepairGrant action consumes the audit response
    \* instead (Step B).
    /\ globalRepairStep # "request_pending"
    /\ stage' = "Reviewer"
    /\ inFlightRequestKind' = "reviewer"
    /\ stuckMathAuditActive' = FALSE
    /\ assumptionLaneActive' = FALSE
    /\ \E coneClean \in BOOLEAN :
            forceReviewAfterConeClean' = coneClean
    /\ \* Sole-flipper effect: either flip one Decide target's live
       \* polarity (within the anti-thrash bound) or leave polarity alone.
       \/ \* No polarity change this audit burst.
          /\ UNCHANGED PolarityVars
          /\ UNCHANGED challengeCoverage
       \/ \* Flip one target's live side (reversible, bounded).
          /\ \E t \in ChallengeTargets :
                 /\ polarityFlips[t] < MaxPolarityFlips
                 /\ challengePolarity' =
                        [challengePolarity EXCEPT ![t] = PolarityFlip(@)]
                 /\ polarityFlips' =
                        [polarityFlips EXCEPT ![t] = @ + 1]
                 /\ challengeClosedSide' =
                        [challengeClosedSide EXCEPT ![t] = "open"]
                 \* The new live side has no covering node yet (the prior
                 \* claim was for the now-dormant side).
                 /\ challengeCoverage' =
                        [challengeCoverage EXCEPT ![t] = {}]
    /\ UNCHANGED <<cleanupAuditActive, needInputAuditorActive>>
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    \* challengeCoverage may change on a flip; the rest of StructureVars
    \* is held explicitly so the flip branch can touch coverage.
    /\ UNCHANGED <<presentNodes, openNodes, coverage, approvedCoverage,
                   configuredTargets, approvedConfiguredTargets>>
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED GateVars
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars

(***************************************************************************)
(* AcceptAssumptionsLane.  After Assumptions Corr Pass, the assumptions   *)
(* StuckMathAudit sub-lane either records one pending conditional         *)
(* property axiom over the setup Rust-validity hook for human review or   *)
(* rejects the staged blocks.  Rejection has no special routing: it       *)
(* returns to Reviewer like other lane rejects.                           *)
(***************************************************************************)
AcceptAssumptionsLane ==
    /\ stage = "StuckMathAudit"
    /\ inFlightRequestKind = "stuck_math_audit"
    /\ phase \in {"theorem_stating", "proof_formalization"}
    /\ assumptionLaneActive
    /\ stage' = "Reviewer"
    /\ inFlightRequestKind' = "reviewer"
    /\ stuckMathAuditActive' = FALSE
    /\ assumptionLaneActive' = FALSE
    /\ \/ /\ pendingUnderModelAssumptions < MaxCycle
          /\ pendingUnderModelAssumptions' = pendingUnderModelAssumptions + 1
       \/ /\ pendingUnderModelAssumptions' = pendingUnderModelAssumptions
    /\ stagedUnderModelAssumptionDraft' = FALSE
    /\ UNCHANGED <<cleanupAuditActive, needInputAuditorActive>>
    /\ UNCHANGED <<gateKind, humanInputOutstanding, pendingProtectedReapproval>>
    /\ UNCHANGED postAdvanceRoutingPending
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* AcceptNeedInputAuditor.  Either confirms the reviewer's NEED_INPUT    *)
(* escalation (next stage is HumanGate with gateKind = need_input) or    *)
(* declines and produces a recovery plan that returns control to        *)
(* Reviewer.                                                              *)
(***************************************************************************)
AcceptNeedInputAuditor ==
    /\ stage = "NeedInputAuditor"
    /\ inFlightRequestKind = "need_input_auditor"
    /\ \E confirm \in BOOLEAN :
        \/ /\ confirm
           \* GapResearch: a confirmed genuine gap no longer halts at a
           \* blind human gate; it dispatches the GapResearch Planner.
           \* The reject budget starts fresh for a NEW gap (the kernel
           \* keys it on the stable node+obligation identity; a same-gap
           \* re-confirmation resumes the existing budget instead — the
           \* abstract model's single scalar collapses both).
           /\ stage' = "GapResearchPlanner"
           /\ inFlightRequestKind' = "gap_research"
           /\ gateKind' = "none"
           /\ gapPlannerActive' = TRUE
           /\ gapCriticActive' = FALSE
           /\ gapRejectCount' = 0
        \/ /\ ~ confirm
           /\ stage' = "Reviewer"
           /\ inFlightRequestKind' = "reviewer"
           /\ gateKind' = "none"
           /\ UNCHANGED GapResearchVars
    /\ needInputAuditorActive' = FALSE
    /\ UNCHANGED <<cleanupAuditActive, stuckMathAuditActive,
                   assumptionLaneActive>>
    /\ UNCHANGED <<humanInputOutstanding, pendingProtectedReapproval,
                   stagedUnderModelAssumptionDraft, pendingUnderModelAssumptions>>
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* AcceptGapResearchPlanner.  The Planner produces a route. For an        *)
(* in-paper / deviation route it dispatches the independent Critic; when  *)
(* it sets `route_needs_human` it routes straight to the HumanGate (one   *)
(* of the two human paths).                                               *)
(***************************************************************************)
AcceptGapResearchPlanner ==
    /\ stage = "GapResearchPlanner"
    /\ inFlightRequestKind = "gap_research"
    /\ \E humanRequired \in BOOLEAN :
        \/ /\ ~ humanRequired
           /\ stage' = "GapPlanCritic"
           /\ inFlightRequestKind' = "gap_plan_critic"
           /\ gateKind' = "none"
           /\ gapPlannerActive' = FALSE
           /\ gapCriticActive' = TRUE
           /\ UNCHANGED gapRejectCount
        \/ /\ humanRequired
           \* Genuine human halt with the full attempt record. The budget
           \* persists to the human gate (Finding A/B); HumanFeedback
           \* clears it on satisfaction.
           /\ stage' = "HumanGate"
           /\ inFlightRequestKind' = "human_gate"
           /\ gateKind' = "need_input"
           /\ gapPlannerActive' = FALSE
           /\ gapCriticActive' = FALSE
           /\ UNCHANGED gapRejectCount
    /\ UNCHANGED AuditLaneVars
    /\ UNCHANGED <<humanInputOutstanding, pendingProtectedReapproval,
                   stagedUnderModelAssumptionDraft, pendingUnderModelAssumptions>>
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* AcceptGapPlanCritic.  ACCEPT routes the plan onto the normal           *)
(* AuditPlan → Reviewer rail (reject budget resets). REJECT increments    *)
(* the bounded counter and either re-dispatches the Planner (< N) or, at  *)
(* N consecutive rejects, escalates to a real HumanGate. The bounded      *)
(* counter is the livelock guard: the planner ↔ critic loop cannot spin   *)
(* forever — it always terminates into ACCEPT (→ Reviewer) or a human     *)
(* halt.                                                                   *)
(***************************************************************************)
AcceptGapPlanCritic ==
    /\ stage = "GapPlanCritic"
    /\ inFlightRequestKind = "gap_plan_critic"
    /\ \E accept \in BOOLEAN :
        \/ /\ accept
           /\ stage' = "Reviewer"
           /\ inFlightRequestKind' = "reviewer"
           /\ gateKind' = "none"
           /\ gapPlannerActive' = FALSE
           /\ gapCriticActive' = FALSE
           \* Finding A/B: the budget is NOT reset on ACCEPT. It persists
           \* (keyed on the stable gap identity in the kernel) so a
           \* persistently-failing fix for the same gap converges to a
           \* human across full ACCEPT -> worker-fail -> re-confirm laps.
           \* It is cleared only on a different gap or human-gate
           \* satisfaction (HumanFeedback).
           /\ UNCHANGED gapRejectCount
        \/ /\ ~ accept
           /\ gapRejectCount + 1 < GapRejectLimit
           \* Re-plan: hand feedback back to a fresh Planner burst.
           /\ stage' = "GapResearchPlanner"
           /\ inFlightRequestKind' = "gap_research"
           /\ gateKind' = "none"
           /\ gapPlannerActive' = TRUE
           /\ gapCriticActive' = FALSE
           /\ gapRejectCount' = gapRejectCount + 1
        \/ /\ ~ accept
           /\ gapRejectCount + 1 >= GapRejectLimit
           \* Convergence failure: escalate to a real HumanGate. The kernel
           \* post-increments the counter to the ceiling before escalating
           \* (Finding E): model that here so gapRejectCount actually
           \* REACHES GapRejectLimit and the GapLoopTerminates escalate
           \* clause is exercised. It is reset to 0 only when the human
           \* gate is satisfied (HumanFeedback).
           /\ stage' = "HumanGate"
           /\ inFlightRequestKind' = "human_gate"
           /\ gateKind' = "need_input"
           /\ gapPlannerActive' = FALSE
           /\ gapCriticActive' = FALSE
           /\ gapRejectCount' = gapRejectCount + 1
    /\ UNCHANGED AuditLaneVars
    /\ UNCHANGED <<humanInputOutstanding, pendingProtectedReapproval,
                   stagedUnderModelAssumptionDraft, pendingUnderModelAssumptions>>
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* Reviewer actions.                                                     *)
(*                                                                        *)
(* `ReviewContinue` writes a pending task with non-empty carriers for    *)
(* the next worker burst, and selects a worker mode and authorized       *)
(* scope.  In the abstract spec, the choice of carriers is non-          *)
(* deterministic but must obey:                                           *)
(*   * task_blockers ⊆ globalBlockers                                    *)
(*   * Local mode authorizedNodes = {} (carve-out invariant)             *)
(*   * Restructure / CoarseRestructure mode authorizedNodes ⊆ presentNodes *)
(*   * Local + task_blockers ⊆ {Soundness carrier active node} only      *)
(***************************************************************************)

ReviewContinue ==
    /\ stage = "Reviewer"
    /\ inFlightRequestKind = "reviewer"
    /\ \E nextMode \in WorkerModes,
          taskCarriers \in SUBSET (presentNodes \cup configuredTargets),
          newActive \in presentNodes \cup {NoNode},
          newAuthorized \in SUBSET presentNodes :
        /\ taskCarriers \subseteq GlobalBlockers
        /\ \* Local mode constraints (carve-out): empty authorized scope.
           nextMode = "local" => newAuthorized = {}
        /\ \* Restructure / CoarseRestructure: non-empty authorized scope.
           nextMode \in {"restructure", "coarse_restructure"} =>
                newAuthorized # {}
        /\ \* Cleanup mode is legal only inside the Cleanup phase.
           nextMode = "cleanup" => phase = "cleanup"
        /\ \* Cleanup mode also requires empty authorized envelope (the
           \* big spec models this differently; here the abstraction is
           \* "cleanup workers operate within a kernel-determined scope,
           \* not a reviewer-chosen one").
           nextMode = "cleanup" => newAuthorized = {}
        /\ \* Local+Soundness carve-out: under Local mode, the only
           \* legal task carrier set is {activeNode}, and only when the
           \* active node's soundness is non-Pass.  (Local mode lets
           \* the worker close the active node's proof, which is the
           \* only edit that clears Soundness.)
           (nextMode = "local" /\ taskCarriers # {}) =>
                /\ taskCarriers = {newActive}
                /\ newActive # NoNode
                /\ newActive \in Nodes
                /\ soundnessStatus[newActive] # "pass"
        /\ activeNode' = newActive
        /\ workerMode' = nextMode
        /\ authorizedNodes' = newAuthorized
        /\ pendingTaskCarriers' = taskCarriers
    /\ hasPendingTask' = TRUE
    /\ pendingTaskKind' = "worker"
    /\ stage' = "Start"
    /\ inFlightRequestKind' = "none"
    /\ \* cyclesSinceClean is set to 0 iff this Continue produces a clean
       \* checkpoint (no blockers); else incremented in spec — abstracted
       \* via two disjuncts.
       \/ /\ GlobalBlockers = {}
          /\ cyclesSinceClean' = 0
          /\ hasEverBeenClean' = TRUE
       \/ /\ GlobalBlockers # {}
          /\ cyclesSinceClean' = 1
          /\ UNCHANGED hasEverBeenClean
    /\ UNCHANGED phase
    /\ UNCHANGED cycle
    /\ UNCHANGED activeCoarseNode
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED GateVars
    /\ UNCHANGED AuditLaneVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(* ReviewNeedInput sets up a NeedInputAuditor follow-up.                  *)
(*                                                                         *)
(* NeedInput re-entry guard (Defect 2): a NeedInput escalation is only    *)
(* legal when there is no human input already outstanding.  Without this  *)
(* precondition the loop Reviewer -> NeedInputAuditor -> HumanGate ->     *)
(* (HumanFeedback sets humanInputOutstanding = TRUE) -> Reviewer ->       *)
(* ReviewNeedInput -> ...  can spin forever on a content-free approve:    *)
(* the reviewer still sees the same unresolved blocker, re-escalates, the *)
(* auditor re-confirms, the gate is re-granted, and no progress is made.  *)
(* Blocking re-entry until the outstanding human input has been consumed  *)
(* (cleared by a continuing reviewer decision, modeled elsewhere) forces  *)
(* the protocol to act on the human payload before it can escalate again. *)
ReviewNeedInput ==
    /\ stage = "Reviewer"
    /\ inFlightRequestKind = "reviewer"
    /\ humanInputOutstanding = FALSE
    \* Mode-B (PV): byte-pinned target statements cannot drift and a
    \* disproof never routes to a human, so the NeedInput human-stop lane
    \* never fires (NoHumanInModeB).  A genuine stuck state in mode-B is
    \* handled by the StuckMathAudit polarity-flip lane, not a human gate.
    /\ GoalMode # "lean"
    /\ stage' = "NeedInputAuditor"
    /\ inFlightRequestKind' = "need_input_auditor"
    /\ needInputAuditorActive' = TRUE
    /\ UNCHANGED <<cleanupAuditActive, stuckMathAuditActive,
                   assumptionLaneActive>>
    /\ humanInputOutstanding' = FALSE  \* NeedInputAuditor may later confirm
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED <<gateKind, pendingProtectedReapproval,
                   stagedUnderModelAssumptionDraft, pendingUnderModelAssumptions>>
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* AdvanceGateVacuous.  The AdvancePhase HumanGate certifies the          *)
(* informal->formal faithfulness of worker-authored target MEANING        *)
(* (semantic closure).  In mode-B (GoalMode = "lean") the targets are     *)
(* author-given + byte-pinned, so there is no faithfulness lane and the   *)
(* gate's meaning-review set is EMPTY — the gate is vacuous and must      *)
(* AUTO-ADVANCE rather than block on a human step.  In mode-A (prose) the *)
(* gate carries real meaning review and the human is in the loop.         *)
(***************************************************************************)
AdvanceGateVacuous ==
    /\ GoalMode = "lean"
    /\ configuredTargets = {}

(***************************************************************************)
(* ReviewAdvancePhase.  The reviewer signals phase advance.  Legal only  *)
(* when globalBlockers is empty.  In mode-A, routes to a HumanGate with   *)
(* gateKind = advance (human meaning-review).  In mode-B the gate is      *)
(* vacuous (AdvanceGateVacuous) so the phase advances directly with NO    *)
(* human turn (NoHumanInModeB) — the effect mirrors HumanApproveAdvance.  *)
(***************************************************************************)
ReviewAdvancePhase ==
    /\ stage = "Reviewer"
    /\ inFlightRequestKind = "reviewer"
    /\ phase \in {"theorem_stating", "proof_formalization"}
    /\ GlobalBlockers = {}
    /\ \/ \* Pending under-model assumptions take the mode-B human
          \* AssumptionReview gate live. This is not a meaning-review gate.
          /\ pendingUnderModelAssumptions > 0
          /\ stage' = "HumanGate"
          /\ inFlightRequestKind' = "human_gate"
          /\ gateKind' = "assumption_review"
          /\ UNCHANGED <<phase, activeCoarseNode>>
          /\ UNCHANGED StructureVars
          /\ UNCHANGED CoarseDagVars
          /\ UNCHANGED RoutingLatchVars
          /\ UNCHANGED GlobalRepairVars
       \/ \* Mode-A (or any non-vacuous gate): human meaning-review.
          /\ pendingUnderModelAssumptions = 0
          /\ ~ AdvanceGateVacuous
          /\ stage' = "HumanGate"
          /\ inFlightRequestKind' = "human_gate"
          /\ gateKind' = "advance"
          /\ UNCHANGED <<phase, activeCoarseNode>>
          /\ UNCHANGED StructureVars
          /\ UNCHANGED CoarseDagVars
          /\ UNCHANGED RoutingLatchVars
          /\ UNCHANGED GlobalRepairVars
       \/ \* Mode-B: vacuous advance gate auto-advances (no human step).
          \* Mirrors HumanApproveAdvance's phase-advance effect.
          /\ pendingUnderModelAssumptions = 0
          /\ AdvanceGateVacuous
          /\ \/ /\ phase = "theorem_stating"
                /\ phase' = "proof_formalization"
                /\ \E newCoarse \in SUBSET presentNodes :
                      coarseDagNodes' = newCoarse
                /\ UNCHANGED activeCoarseNode
                /\ UNCHANGED GlobalRepairVars
             \/ /\ phase = "proof_formalization"
                /\ FormalizationComplete
                /\ phase' = "cleanup"
                /\ activeCoarseNode' = NoNode
                /\ UNCHANGED coarseDagNodes
                /\ globalRepairStep' = "none"
          /\ stage' = "Start"
          /\ inFlightRequestKind' = "none"
          /\ gateKind' = "none"
          /\ postAdvanceRoutingPending' = (phase' = "proof_formalization")
          /\ UNCHANGED forceReviewAfterConeClean
          /\ approvedConfiguredTargets' = configuredTargets
          /\ approvedCoverage' = coverage
          /\ UNCHANGED <<presentNodes, openNodes, coverage,
                         challengeCoverage, configuredTargets>>
    /\ hasPendingTask' = FALSE
    /\ pendingTaskKind' = "none"
    /\ pendingTaskCarriers' = {}
    /\ UNCHANGED workerMode
    /\ UNCHANGED <<cycle, activeNode>>
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED <<humanInputOutstanding, pendingProtectedReapproval,
                   stagedUnderModelAssumptionDraft, pendingUnderModelAssumptions>>
    /\ UNCHANGED AuditLaneVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* ReviewDone.  Cleanup-only.  Transitions phase to Complete.            *)
(***************************************************************************)
ReviewDone ==
    /\ stage = "Reviewer"
    /\ inFlightRequestKind = "reviewer"
    /\ phase = "cleanup"
    /\ GlobalBlockers = {}
    /\ presentNodes \cap openNodes = {}
    /\ \A n \in presentNodes : localClosureStatus[n] = "verified"
    /\ phase' = "complete"
    /\ stage' = "Start"
    /\ inFlightRequestKind' = "none"
    /\ hasPendingTask' = FALSE
    /\ pendingTaskKind' = "none"
    /\ pendingTaskCarriers' = {}
    /\ activeNode' = NoNode
    /\ activeCoarseNode' = NoNode
    /\ UNCHANGED workerMode
    /\ UNCHANGED <<cycle>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED GateVars
    /\ UNCHANGED AuditLaneVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* Human gate actions.                                                   *)
(*                                                                        *)
(* HumanApproveAdvance promotes the approved snapshot and advances the   *)
(* phase.  HumanApproveProtectedReapproval clears the pending reapproval *)
(* set.  HumanFeedback sets humanInputOutstanding (the human's reply is  *)
(* outstanding for the next reviewer turn to clear).                     *)
(***************************************************************************)
HumanApproveAdvance ==
    /\ stage = "HumanGate"
    /\ inFlightRequestKind = "human_gate"
    /\ gateKind = "advance"
    /\ phase \in {"theorem_stating", "proof_formalization"}
    /\ \/ /\ phase = "theorem_stating"
          /\ phase' = "proof_formalization"
          /\ \E newCoarse \in SUBSET presentNodes :
                coarseDagNodes' = newCoarse
          /\ UNCHANGED activeCoarseNode
       \/ /\ phase = "proof_formalization"
          /\ FormalizationComplete
          /\ phase' = "cleanup"
          /\ activeCoarseNode' = NoNode
          /\ UNCHANGED coarseDagNodes
    /\ stage' = "Start"
    /\ inFlightRequestKind' = "none"
    /\ gateKind' = "none"
    /\ postAdvanceRoutingPending' = (phase' = "proof_formalization")
    /\ UNCHANGED forceReviewAfterConeClean
    /\ approvedConfiguredTargets' = configuredTargets
    /\ approvedCoverage' = coverage
    /\ UNCHANGED <<cycle, activeNode>>
    /\ UNCHANGED <<presentNodes, openNodes, coverage, challengeCoverage,
                   configuredTargets>>
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED <<humanInputOutstanding, pendingProtectedReapproval,
                   stagedUnderModelAssumptionDraft, pendingUnderModelAssumptions>>
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED AuditLaneVars
    /\ \* Phase advance out of proof_formalization clears the global-
       \* repair lane (any in-flight request or grant is invalidated
       \* by the phase change).
       \/ /\ phase' = "proof_formalization"
          /\ UNCHANGED GlobalRepairVars
       \/ /\ phase' # "proof_formalization"
          /\ globalRepairStep' = "none"
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

HumanApproveAssumptionReview ==
    /\ stage = "HumanGate"
    /\ inFlightRequestKind = "human_gate"
    /\ gateKind = "assumption_review"
    /\ pendingUnderModelAssumptions > 0
    /\ phase \in {"theorem_stating", "proof_formalization"}
    /\ \/ /\ phase = "theorem_stating"
          /\ phase' = "proof_formalization"
          /\ \E newCoarse \in SUBSET presentNodes :
                coarseDagNodes' = newCoarse
          /\ UNCHANGED activeCoarseNode
       \/ /\ phase = "proof_formalization"
          /\ FormalizationComplete
          /\ phase' = "cleanup"
          /\ activeCoarseNode' = NoNode
          /\ UNCHANGED coarseDagNodes
    /\ stage' = "Start"
    /\ inFlightRequestKind' = "none"
    /\ gateKind' = "none"
    /\ pendingUnderModelAssumptions' = 0
    /\ stagedUnderModelAssumptionDraft' = FALSE
    /\ postAdvanceRoutingPending' = (phase' = "proof_formalization")
    /\ UNCHANGED forceReviewAfterConeClean
    /\ approvedConfiguredTargets' = configuredTargets
    /\ approvedCoverage' = coverage
    /\ UNCHANGED <<cycle, activeNode>>
    /\ UNCHANGED <<presentNodes, openNodes, coverage, challengeCoverage,
                   configuredTargets>>
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED <<humanInputOutstanding, pendingProtectedReapproval>>
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED AuditLaneVars
    /\ \/ /\ phase' = "proof_formalization"
          /\ UNCHANGED GlobalRepairVars
       \/ /\ phase' # "proof_formalization"
          /\ globalRepairStep' = "none"
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

HumanRejectAssumptionReview ==
    /\ stage = "HumanGate"
    /\ inFlightRequestKind = "human_gate"
    /\ gateKind = "assumption_review"
    /\ pendingUnderModelAssumptions > 0
    /\ stage' = "Reviewer"
    /\ inFlightRequestKind' = "reviewer"
    /\ gateKind' = "none"
    /\ pendingUnderModelAssumptions' = 0
    /\ stagedUnderModelAssumptionDraft' = FALSE
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED <<humanInputOutstanding, pendingProtectedReapproval>>
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED AuditLaneVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

HumanApproveProtectedReapproval ==
    /\ stage = "HumanGate"
    /\ inFlightRequestKind = "human_gate"
    /\ gateKind = "protected_reapproval"
    /\ pendingProtectedReapproval # {}
    /\ stage' = "Reviewer"
    /\ inFlightRequestKind' = "reviewer"
    /\ gateKind' = "none"
    /\ pendingProtectedReapproval' = {}
    /\ approvedConfiguredTargets' = configuredTargets
    /\ approvedCoverage' = coverage
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED <<presentNodes, openNodes, coverage, challengeCoverage,
                   configuredTargets>>
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED <<humanInputOutstanding, stagedUnderModelAssumptionDraft,
                   pendingUnderModelAssumptions>>
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED AuditLaneVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

HumanFeedback ==
    /\ stage = "HumanGate"
    /\ inFlightRequestKind = "human_gate"
    /\ gateKind \in {"advance", "need_input", "protected_reapproval"}
    /\ stage' = "Reviewer"
    /\ inFlightRequestKind' = "reviewer"
    /\ gateKind' = "none"
    /\ humanInputOutstanding' = TRUE
    \* Human-gate satisfaction is the point where the GapResearch
    \* bounded-reject budget is cleared (Finding A/B). This also brings
    \* gapRejectCount back below the ceiling it reached at escalation.
    /\ gapRejectCount' = 0
    /\ UNCHANGED <<gapPlannerActive, gapCriticActive>>
    /\ UNCHANGED <<pendingProtectedReapproval, stagedUnderModelAssumptionDraft,
                   pendingUnderModelAssumptions>>
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED AuditLaneVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* MaybeIssueProtectedReapprovalGate.  At any in-cycle Reviewer pause    *)
(* where a worker delta has reopened an approved-target carrier, the     *)
(* kernel routes to a HumanGate of kind protected_reapproval before any  *)
(* further continuation.                                                  *)
(***************************************************************************)
MaybeIssueProtectedReapprovalGate ==
    /\ stage = "Reviewer"
    /\ inFlightRequestKind = "reviewer"
    /\ phase = "proof_formalization"
    \* Mode-B (PV): byte-pinned target statements cannot drift, so the
    \* monotonicity / protected-reapproval gate never fires (NoHumanInModeB).
    /\ GoalMode # "lean"
    /\ \E touchedNodes \in SUBSET approvedCoverage[CHOOSE t \in Targets : TRUE] :
            \/ TRUE \* abstract: any non-empty subset of presentNodes may be
                    \* the protected-reapproval set
            \/ touchedNodes # {}
    /\ \E touched \in SUBSET presentNodes :
            /\ touched # {}
            /\ pendingProtectedReapproval' = touched
    /\ stage' = "HumanGate"
    /\ inFlightRequestKind' = "human_gate"
    /\ gateKind' = "protected_reapproval"
    /\ UNCHANGED <<humanInputOutstanding, stagedUnderModelAssumptionDraft,
                   pendingUnderModelAssumptions>>
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED AuditLaneVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* EditConfiguredTargets.  Editing the configured-target set is an out- *)
(* of-band operator action.  Modeled as a non-deterministic mutation of *)
(* the configured-target set; the reviewer/expert gate is then forced to *)
(* re-approve.                                                            *)
(***************************************************************************)
EditConfiguredTargets ==
    /\ stage = "Start"
    /\ inFlightRequestKind = "none"
    /\ phase = "theorem_stating"
    \* Mode-B has no paper-faithfulness bucket; the configured-target set
    \* stays empty (ModeBConfiguredTargetsEmpty), so this operator edit is
    \* mode-A only.
    /\ GoalMode # "lean"
    /\ \E newConfigured \in SUBSET Targets :
        /\ newConfigured # configuredTargets
        /\ configuredTargets' = newConfigured
        /\ \* Pending-task target carriers are pruned to the new
           \* configured set (kernel: relegalize step drops carriers
           \* that no longer correspond to live blockers).
           pendingTaskCarriers' =
                {c \in pendingTaskCarriers :
                    c \in (presentNodes \cup newConfigured
                           \cup ChallengeTargets)}
    /\ UNCHANGED approvedConfiguredTargets
    /\ UNCHANGED <<phase, stage, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED <<presentNodes, openNodes, coverage, approvedCoverage,
                   challengeCoverage>>
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED GateVars
    /\ UNCHANGED <<hasPendingTask, pendingTaskKind, workerMode>>
    /\ UNCHANGED AuditLaneVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED InFlightVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* RequestGlobalRepairAudit.  Step A: reviewer asks for a global-repair *)
(* audit.  Sets globalRepairStep = "request_pending"; next StuckMathAudit *)
(* burst is the one that grants/declines. The core spec abstracts retry  *)
(* context away; this action is the intent-level non-protected escape    *)
(* hatch for any present node outside the current ordinary scope.        *)
(***************************************************************************)
RequestGlobalRepairAudit ==
    /\ stage = "Reviewer"
    /\ inFlightRequestKind = "reviewer"
    /\ phase = "proof_formalization"
    /\ globalRepairStep = "none"
    /\ \* Step A is dispatched as a Continue routed to StuckMathAudit.
       \* The spec is intentionally coarse here: the reviewer Continue's
       \* presentation choice (worker burst now, audit burst next) is a
       \* kernel-side detail; we just say "step transitions to pending".
       globalRepairStep' = "request_pending"
    /\ stage' = "StuckMathAudit"
    /\ inFlightRequestKind' = "stuck_math_audit"
    /\ stuckMathAuditActive' = TRUE
    /\ UNCHANGED <<cleanupAuditActive, needInputAuditorActive,
                   assumptionLaneActive>>
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED GateVars
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* RequestPolarityAudit (PV mode-B).  The reviewer (or an on-demand        *)
(* worker/reviewer audit request — see the on-demand-audit feature)        *)
(* dispatches a PLAIN StuckMathAudit burst that is NOT a global-repair      *)
(* request: `globalRepairStep` stays "none", so the next StuckMathAudit     *)
(* accept is `AcceptStuckMathAudit` (the polarity flipper), not             *)
(* `AcceptGlobalRepairGrant`.  This is what makes the Decide direction      *)
(* reachable: a target whose live side is false-and-thus-unclosable can     *)
(* only be resolved after the audit lane flips it.  Mode-B only (the        *)
(* prove-or-disprove axis does not exist in mode-A).                        *)
(***************************************************************************)
RequestPolarityAudit ==
    /\ stage = "Reviewer"
    /\ inFlightRequestKind = "reviewer"
    /\ phase = "proof_formalization"
    /\ globalRepairStep = "none"
    /\ GoalMode = "lean"
    /\ ~ stuckMathAuditActive
    /\ stage' = "StuckMathAudit"
    /\ inFlightRequestKind' = "stuck_math_audit"
    /\ stuckMathAuditActive' = TRUE
    /\ UNCHANGED <<cleanupAuditActive, needInputAuditorActive,
                   assumptionLaneActive>>
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED GateVars
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* AcceptGlobalRepairGrant.  Step B: the StuckMathAudit burst returns a *)
(* grant.  globalRepairStep flips to "grant_available".  Routes back to *)
(* Reviewer.                                                              *)
(***************************************************************************)
AcceptGlobalRepairGrant ==
    /\ stage = "StuckMathAudit"
    /\ inFlightRequestKind = "stuck_math_audit"
    /\ globalRepairStep = "request_pending"
    /\ \E grant \in BOOLEAN :
            \/ /\ grant
               /\ globalRepairStep' = "grant_available"
            \/ /\ ~ grant
               /\ globalRepairStep' = "none"
    /\ stage' = "Reviewer"
    /\ inFlightRequestKind' = "reviewer"
    /\ stuckMathAuditActive' = FALSE
    /\ assumptionLaneActive' = FALSE
    /\ UNCHANGED <<cleanupAuditActive, needInputAuditorActive>>
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED GateVars
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* ConsumeGlobalRepairGrant.  Step C: reviewer consumes the grant by    *)
(* issuing the next Continue with the granted scope.  Modeled as the    *)
(* grant transitioning back to "none" alongside an ordinary Continue;   *)
(* the carrier-narrowing and retry-accounting semantics are             *)
(* kernel-level detail.                                                  *)
(***************************************************************************)
ConsumeGlobalRepairGrant ==
    /\ stage = "Reviewer"
    /\ inFlightRequestKind = "reviewer"
    /\ phase = "proof_formalization"
    /\ globalRepairStep = "grant_available"
    /\ globalRepairStep' = "none"
    /\ \E nextMode \in WorkerModes,
          newActive \in presentNodes \cup {NoNode},
          newAuthorized \in SUBSET presentNodes :
        /\ nextMode \in {"restructure", "coarse_restructure"}
        /\ newAuthorized # {}
        /\ activeNode' = newActive
        /\ workerMode' = nextMode
        /\ authorizedNodes' = newAuthorized
    /\ stage' = "Start"
    /\ inFlightRequestKind' = "none"
    /\ hasPendingTask' = TRUE
    /\ pendingTaskKind' = "worker"
    /\ pendingTaskCarriers' = GlobalBlockers
    /\ UNCHANGED <<phase, cycle, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED GateVars
    /\ UNCHANGED AuditLaneVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* RescindApprovedAxiom.  Audit H-2 — operator-driven environmental edit *)
(* of `APPROVED_AXIOMS.json`.  In the kernel this is detected as a       *)
(* per-record `approved_axioms_hash` mismatch on the next                *)
(* `step_runtime` call; the rescission hook demotes affected records to *)
(* unverified.                                                            *)
(*                                                                        *)
(* Spec model: the operator non-deterministically picks one present node *)
(* and flips its `localClosureStatus` to `unverified`.  The              *)
(* `formalizationComplete` gate is then blocked until a fresh probe re-  *)
(* establishes the verified status.                                       *)
(*                                                                        *)
(* The action mirrors the kernel's runtime-CLI hook, NOT the in-flight   *)
(* request semantics; the kernel's hook fires regardless of in-flight   *)
(* stage (the policy change is an environmental fact that survives in-  *)
(* flight prompts).                                                      *)
(***************************************************************************)
RescindApprovedAxiom ==
    /\ \E n \in presentNodes :
        /\ localClosureStatus[n] = "verified"
        /\ localClosureStatus' =
                [localClosureStatus EXCEPT ![n] = "unverified"]
    /\ UNCHANGED <<phase, stage, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED GateVars
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED AuditLaneVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars
    /\ UNCHANGED GapResearchVars
    /\ UNCHANGED InFlightVars
    /\ UNCHANGED BackendVars
    /\ UNCHANGED PolarityVars

(***************************************************************************)
(* Next: disjunction of all actions.                                     *)
(***************************************************************************)
Next ==
    \/ StartCycle
    \/ AcceptWorker
    \/ AcceptVerifier
    \/ AcceptAssumptionsCorrPass
    \/ AcceptCleanupAudit
    \/ AcceptStuckMathAudit
    \/ AcceptAssumptionsLane
    \/ AcceptNeedInputAuditor
    \/ AcceptGapResearchPlanner
    \/ AcceptGapPlanCritic
    \/ ReviewContinue
    \/ ReviewNeedInput
    \/ ReviewAdvancePhase
    \/ ReviewDone
    \/ HumanApproveAdvance
    \/ HumanApproveAssumptionReview
    \/ HumanRejectAssumptionReview
    \/ HumanApproveProtectedReapproval
    \/ HumanFeedback
    \/ MaybeIssueProtectedReapprovalGate
    \/ EditConfiguredTargets
    \/ RequestGlobalRepairAudit
    \/ RequestPolarityAudit
    \/ AcceptGlobalRepairGrant
    \/ ConsumeGlobalRepairGrant
    \/ RescindApprovedAxiom

Spec == Init /\ [][Next]_Vars

(***************************************************************************)
(* ---------------------- INVARIANTS ----------------------------------- *)
(***************************************************************************)

(* TypeOK.  Narrower than the big spec's TypeOK; defines the universe of *)
(* values each variable inhabits.                                         *)
TypeOK ==
    /\ phase \in Phases
    /\ stage \in Stages
    /\ cycle \in 0..MaxCycle
    /\ activeNode \in Nodes \cup {NoNode}
    /\ activeCoarseNode \in Nodes \cup {NoNode}
    /\ presentNodes \subseteq Nodes
    /\ openNodes \subseteq presentNodes
    /\ coverage \in [Targets -> SUBSET Nodes]
    /\ approvedCoverage \in [Targets -> SUBSET Nodes]
    /\ \A t \in Targets : coverage[t] \subseteq presentNodes
    /\ challengeCoverage \in [ChallengeTargets -> SUBSET Nodes]
    /\ \A t \in ChallengeTargets : challengeCoverage[t] \subseteq presentNodes
    /\ configuredTargets \subseteq Targets
    /\ approvedConfiguredTargets \subseteq Targets
    /\ coarseDagNodes \subseteq Nodes
    /\ correspondenceStatus  \in [Nodes -> LaneStatuses]
    /\ substantivenessStatus \in [Nodes -> LaneStatuses]
    /\ soundnessStatus       \in [Nodes -> LaneStatuses]
    /\ deviationStatus       \in [Nodes -> LaneStatuses]
    /\ faithfulnessStatus    \in [Targets -> LaneStatuses]
    /\ localClosureStatus    \in [Nodes -> LocalClosureStatuses]
    /\ authorizedNodes \subseteq Nodes
    /\ gateKind \in GateKinds
    /\ humanInputOutstanding \in BOOLEAN
    /\ pendingProtectedReapproval \subseteq Nodes
    /\ stagedUnderModelAssumptionDraft \in BOOLEAN
    /\ pendingUnderModelAssumptions \in 0..MaxCycle
    /\ hasPendingTask \in BOOLEAN
    /\ pendingTaskKind \in PendingTaskKinds
    /\ pendingTaskCarriers \subseteq (Nodes \cup Targets \cup ChallengeTargets)
    /\ workerMode \in WorkerModes
    /\ cleanupAuditActive \in BOOLEAN
    /\ stuckMathAuditActive \in BOOLEAN
    /\ needInputAuditorActive \in BOOLEAN
    /\ assumptionLaneActive \in BOOLEAN
    /\ gapPlannerActive \in BOOLEAN
    /\ gapCriticActive \in BOOLEAN
    /\ gapRejectCount \in 0..GapRejectLimit
    /\ postAdvanceRoutingPending \in BOOLEAN
    /\ forceReviewAfterConeClean \in BOOLEAN
    /\ globalRepairStep \in GlobalRepairSteps
    /\ cyclesSinceClean \in 0..MaxCycle
    /\ hasEverBeenClean \in BOOLEAN
    /\ inFlightRequestKind \in RequestKinds
    /\ nodeTarget \in [Nodes -> Backends]
    /\ challengePolarity \in [ChallengeTargets -> Polarities]
    /\ polarityFlips \in [ChallengeTargets -> 0..MaxPolarityFlips]
    /\ challengeClosedSide \in [ChallengeTargets -> ChallengeClosureStatuses]

(***************************************************************************)
(* HumanGateMatchesState.  gateKind is non-`none` iff we are at a       *)
(* HumanGate stage.                                                      *)
(***************************************************************************)
HumanGateMatchesState ==
    (gateKind # "none") <=> (stage = "HumanGate")

(***************************************************************************)
(* NeedInputReentryGuarded.  A NeedInput-kind HumanGate is only reached  *)
(* from a reviewer state in which no human input was already outstanding *)
(* (see ReviewNeedInput).  Structurally this means: while a need_input   *)
(* gate is open, the only way humanInputOutstanding became TRUE is via   *)
(* the very HumanFeedback that grants this gate's successor, never via a *)
(* re-escalation that bypassed an outstanding human payload.  Stated as  *)
(* the contrapositive of the re-entry loop: the protocol is never at a   *)
(* NeedInputAuditor stage with humanInputOutstanding still set, which is *)
(* the configuration the content-free-approve livelock would re-enter.   *)
(***************************************************************************)
NeedInputReentryGuarded ==
    stage = "NeedInputAuditor" => humanInputOutstanding = FALSE

(***************************************************************************)
(* InFlightKindMatchesStage.  The in-flight request kind matches the    *)
(* stage that consumes it.  Between cycles (stage = Start) and when the *)
(* run has terminated (phase = complete), no request is in flight.       *)
(***************************************************************************)
InFlightKindMatchesStage ==
    /\ (stage = "Start")          => (inFlightRequestKind = "none")
    /\ (phase = "complete")        => (inFlightRequestKind = "none")
    /\ (stage = "Worker")          => (inFlightRequestKind = "worker")
    /\ (stage = "Reviewer")        => (inFlightRequestKind = "reviewer")
    /\ (stage = "HumanGate")       => (inFlightRequestKind = "human_gate")
    /\ (stage = "CleanupAudit")    => (inFlightRequestKind = "cleanup_audit")
    /\ (stage = "StuckMathAudit")  => (inFlightRequestKind = "stuck_math_audit")
    /\ (stage = "NeedInputAuditor") => (inFlightRequestKind = "need_input_auditor")
    /\ (stage = "GapResearchPlanner") => (inFlightRequestKind = "gap_research")
    /\ (stage = "GapPlanCritic")   => (inFlightRequestKind = "gap_plan_critic")
    /\ (stage = "VerifyFaithfulness") =>
            (inFlightRequestKind = "verifier_faithfulness")
    /\ (stage = "VerifySubstantiveness") =>
            (inFlightRequestKind = "verifier_substantiveness")
    /\ (stage = "VerifyCorrespondence") =>
            (inFlightRequestKind = "verifier_correspondence")
    /\ (stage = "VerifySoundness") =>
            (inFlightRequestKind = "verifier_soundness")
    /\ (stage = "VerifyDeviation") =>
            (inFlightRequestKind = "verifier_deviation")

(***************************************************************************)
(* CleanupHasNoBlockers.  The protocol may only be in the cleanup phase *)
(* with an empty global-blocker set.  This is the design's "happy stop" *)
(* contract.                                                              *)
(***************************************************************************)
CleanupHasNoBlockers ==
    phase = "cleanup" => GlobalBlockers = {}

(***************************************************************************)
(* NoAdvancePhaseWithBlockers.  Phase advance is illegal while any      *)
(* blocker is live.  The kernel enforces this at the AdvancePhase       *)
(* decision; the abstract spec enforces it at the ReviewAdvancePhase    *)
(* action precondition, so this invariant is structural.                *)
(***************************************************************************)
NoAdvancePhaseWithBlockers ==
    /\ stage = "HumanGate" /\ gateKind = "advance"
        => GlobalBlockers = {}

(***************************************************************************)
(* CompleteRequiresChallengeCoverage.  Completion requires full          *)
(* challenge coverage.  Enforcement is inherited: each uncovered        *)
(* challenge target sits in GlobalBlockers, and ReviewAdvancePhase /    *)
(* ReviewDone require GlobalBlockers = {}.                              *)
(***************************************************************************)
CompleteRequiresChallengeCoverage ==
    phase = "complete" =>
        \A t \in ChallengeTargets : challengeCoverage[t] # {}

(* ===================================================================== *)
(* PV mode-B prove-or-disprove invariants (forward design).              *)
(* ===================================================================== *)

(***************************************************************************)
(* ModeBConfiguredTargetsEmpty.  In a mode-B (Lean-goals) run the paper-  *)
(* faithfulness target bucket is always empty — byte-pinned challenge     *)
(* specs are the sole target type, so the faithfulness lane never has a   *)
(* carrier.  The configured set starts empty (ASSUME) and no action       *)
(* populates it in mode-B (EditConfiguredTargets is mode-A only).         *)
(***************************************************************************)
ModeBConfiguredTargetsEmpty ==
    GoalMode = "lean" => configuredTargets = {}

(***************************************************************************)
(* SoundnessFloor / SymmetricAxioms.  No reachable state has a Decide     *)
(* target whose LIVE side recorded "closed" while that side is FALSE      *)
(* under the abstract truth model.  The closure gate (CloseSidePermitted) *)
(* is polarity-symmetric, so this holds identically for "prove" and       *)
(* "disprove": a worker can never resolve a target by closing a false T   *)
(* or a false ~T.  This is the axiom-floor soundness wall — closing a     *)
(* statement requires it to be TRUE.                                      *)
(***************************************************************************)
SoundnessFloor ==
    \A t \in ChallengeTargets :
        (challengeClosedSide[t] = "closed")
            => (ChallengeTruth[t] = challengePolarity[t])

(***************************************************************************)
(* NotBothSidesClosed.  For a Decide pair {T, ~T}, the two sides are      *)
(* never both closed.  Structurally guaranteed: only the LIVE side has a  *)
(* closure record (`challengeClosedSide` tracks the live side), and a     *)
(* polarity flip RESETS the record to "open" — so a closed record always  *)
(* refers to exactly one (the current live) side, and SoundnessFloor      *)
(* forbids that side being false.  Since at most one of {T, ~T} is true   *)
(* (ChallengeTruth[t] is single-valued), at most one side is ever         *)
(* closable.                                                              *)
(***************************************************************************)
NotBothSidesClosed ==
    \A t \in ChallengeTargets :
        (challengeClosedSide[t] = "closed")
            => (challengePolarity[t] = ChallengeTruth[t])

(***************************************************************************)
(* NoHumanInModeB.  In a mode-B run the ordinary meaning-review advance   *)
(* gate remains vacuous and auto-advances.  The only HumanGate exception  *)
(* is `assumption_review`, which exists solely to ratify or reject         *)
(* under-model assumptions already staged, Corr-gated, and assumptions-   *)
(* lane-gated.  ProtectedReapproval / NeedInput human-stop lanes remain   *)
(* guarded off in mode-B.                                                 *)
(***************************************************************************)
NoHumanInModeB ==
    GoalMode = "lean" =>
        /\ ((stage = "HumanGate") => (gateKind = "assumption_review"))
        /\ gateKind \in {"none", "assumption_review"}
        /\ ((inFlightRequestKind = "human_gate") => (gateKind = "assumption_review"))
        /\ humanInputOutstanding = FALSE
        /\ pendingProtectedReapproval = {}

(***************************************************************************)
(* UnderModelAssumptionLifecycleContract.  Formula bodies stay abstract.  *)
(* The assumptions sub-lane can be active only after the Assumptions      *)
(* node's `.spec`/`.tex` pair has passed Correspondence; any pending      *)
(* count denotes a worker-authored conditional property axiom over the    *)
(* setup Rust-validity                                                    *)
(* hook.                                                                  *)
(***************************************************************************)
UnderModelAssumptionLifecycleContract ==
    /\ assumptionLaneActive =>
        /\ GoalMode = "lean"
        /\ AssumptionsNode \in presentNodes
        /\ stagedUnderModelAssumptionDraft
        /\ correspondenceStatus[AssumptionsNode] = "pass"
        /\ UnderModelAssumptionShapeOK
    /\ pendingUnderModelAssumptions > 0 =>
        /\ GoalMode = "lean"
        /\ UnderModelAssumptionShapeOK

(***************************************************************************)
(* CoverageOnLiveOnly.  A Decide target's challenge coverage / closure    *)
(* is bounded by the anti-thrash flip budget and never refers to a        *)
(* dormant side: closure is recorded only via the live side (the          *)
(* challenge-close disjunct gates on `challengePolarity[t]`), a flip      *)
(* always resets the record to "open" and clears coverage, and the flip   *)
(* counter stays within its bound.  The dormant side can therefore never  *)
(* independently satisfy coverage nor block completion — there is no       *)
(* orphan dormant node that keeps a target blocked (the G1/G2             *)
(* completion/deletion trap is impossible).                               *)
(***************************************************************************)
CoverageOnLiveOnly ==
    \A t \in ChallengeTargets :
        /\ polarityFlips[t] <= MaxPolarityFlips
        /\ challengePolarity[t] \in Polarities
        /\ challengeClosedSide[t] \in ChallengeClosureStatuses
        \* A closed record always belongs to the live side (never a stale
        \* dormant-side closure surviving a flip).
        /\ (challengeClosedSide[t] = "closed") => (challengeCoverage[t] # {})

(***************************************************************************)
(* DecidedResolves.  A disproved target (Disprove-side closed) counts as  *)
(* a resolved / "decided" target for completion exactly as a proved one   *)
(* does: ChallengeTargetDecided is polarity-agnostic, so a completed run  *)
(* may have reached Done via the Disprove side.  Stated as: at            *)
(* completion every challenge target is decided (covered AND closed), and *)
(* that decision may be on either polarity.                               *)
(***************************************************************************)
DecidedResolves ==
    phase = "complete" =>
        \A t \in ChallengeTargets :
            /\ ChallengeTargetDecided(t)
            /\ challengePolarity[t] \in Polarities

(***************************************************************************)
(* CleanupDoneTerminal.  When the protocol is at phase = complete, the *)
(* run is terminal: no in-flight request, no pending task, no audit     *)
(* lane active, no global blockers.                                      *)
(***************************************************************************)
CleanupDoneTerminal ==
    phase = "complete" =>
        /\ stage = "Start"
        /\ inFlightRequestKind = "none"
        /\ ~ hasPendingTask
        /\ ~ cleanupAuditActive
        /\ ~ stuckMathAuditActive
        /\ ~ needInputAuditorActive
        /\ ~ assumptionLaneActive
        /\ GlobalBlockers = {}

(***************************************************************************)
(* StalePassClosurePreventsCleanupAdvance.  A node with                 *)
(* localClosureStatus = "unverified" cannot satisfy formalizationComplete,*)
(* so phase advance from proof_formalization to cleanup is blocked.     *)
(***************************************************************************)
StalePassClosurePreventsCleanupAdvance ==
    \A n \in presentNodes :
        (localClosureStatus[n] = "unverified")
            => ~ FormalizationComplete

(***************************************************************************)
(* ClosureCoverageTotal.  Audit C-3 / M-1 — every present node has an     *)
(* entry in the localClosureStatus map. Modeled as a total function via *)
(* TypeOK; this invariant ratifies the C-3 continuous-scan guarantee:    *)
(* the kernel never leaves a sorry-free present proof_node without a    *)
(* representation in the closure tier (records ∪ unverified).            *)
(*                                                                        *)
(* The big spec models records and unverified separately. The intent    *)
(* spec collapses to a single status total over Nodes, so the invariant *)
(* is structural; we keep it as a named clause so future spec edits      *)
(* know to preserve coverage when adding closure-tier behaviors.        *)
(***************************************************************************)
ClosureCoverageTotal ==
    \A n \in presentNodes : localClosureStatus[n] \in LocalClosureStatuses

(***************************************************************************)
(* ClosureStatusMutex.  Audit H-1 / M-1 — verified and unverified are   *)
(* mutually exclusive per node (modeled here as the enum range of       *)
(* localClosureStatus). The big spec splits records and unverified into *)
(* two state variables and enforces an explicit set-disjointness clause; *)
(* the intent spec models that as the structural range constraint —     *)
(* localClosureStatus[n] cannot be both "verified" and "unverified" by  *)
(* construction.                                                          *)
(*                                                                        *)
(* The invariant is identically TRUE on a total function with a 2-      *)
(* element codomain; it is restated here so future spec edits that      *)
(* widen the codomain (e.g. add a "pending" status) know to revisit the *)
(* mutex contract.                                                        *)
(***************************************************************************)
ClosureStatusMutex ==
    \A n \in presentNodes :
        ~(localClosureStatus[n] = "verified" /\ localClosureStatus[n] = "unverified")

(***************************************************************************)
(* CrossTargetFormalIsolation (Phase IV step 12).  A formal-import edge   *)
(* `a -> b` (`FormalImports`, kernel `deps`) means `a`'s source physically *)
(* imports `b`'s declaration, so both are elaborated by the SAME backend.  *)
(* Every such edge whose endpoints are both present must therefore stay    *)
(* within one backend.  (NL `\noderef` citations are tracked separately    *)
(* under Soundness and are NOT formal imports — they may cross targets.)   *)
(* Vacuous when every node shares a target (the all-Lean case, `Backends`  *)
(* = {"lean"}), matching the byte-identical kernel check of the same name. *)
(***************************************************************************)
CrossTargetFormalIsolation ==
    \A e \in FormalImports :
        (e[1] \in presentNodes /\ e[2] \in presentNodes)
            => nodeTarget[e[1]] = nodeTarget[e[2]]

(***************************************************************************)
(* CoarseAnchorSafe.  Membership and dormancy invariants for the active *)
(* coarse anchor (proposal v32).                                         *)
(***************************************************************************)
CoarseAnchorSafe ==
    /\ activeCoarseNode \in (coarseDagNodes \cup {NoNode})
    /\ (phase # "proof_formalization") => activeCoarseNode = NoNode
    /\ (coarseDagNodes = {}) => activeCoarseNode = NoNode

(***************************************************************************)
(* AnchorChangeForbiddenDuringGlobalRepair.  When a global-repair grant *)
(* is on the table (step in {request_pending, grant_available}), the    *)
(* anchor cannot move — the repair is supposed to fix the current       *)
(* anchor's cone, not switch anchors.                                    *)
(***************************************************************************)
AnchorChangeForbiddenDuringGlobalRepair ==
    globalRepairStep \in {"request_pending", "grant_available"}
        => /\ phase = "proof_formalization"
           /\ activeCoarseNode \in (coarseDagNodes \cup {NoNode})

(***************************************************************************)
(* LocalModeSoundnessCarveOut.  A Local-mode pending task may only      *)
(* carry the active node as a task carrier, and only when the carrier   *)
(* corresponds to a Soundness blocker on the active node.               *)
(***************************************************************************)
LocalModeSoundnessCarveOut ==
    (hasPendingTask /\ workerMode = "local" /\ pendingTaskCarriers # {})
        => /\ pendingTaskCarriers = {activeNode}
           /\ activeNode \in Nodes
           /\ soundnessStatus[activeNode] # "pass"

(***************************************************************************)
(* AuthorizedNodesScopeContract.  The reviewer's authorized-node        *)
(* envelope is empty for Local and Cleanup modes (the worker has no    *)
(* cross-node edit authority); non-empty for Restructure /             *)
(* CoarseRestructure (the worker is given an explicit edit envelope).  *)
(***************************************************************************)
AuthorizedNodesScopeContract ==
    /\ workerMode \in {"local", "cleanup"} => authorizedNodes = {}
    /\ authorizedNodes # {} =>
            workerMode \in {"restructure", "coarse_restructure"}
    /\ authorizedNodes \subseteq presentNodes

(***************************************************************************)
(* PendingTaskStaging.  Pending tasks only exist between Start and the  *)
(* matching Worker dispatch.  The task's carrier set must lie in the    *)
(* live carrier universe (present nodes or configured targets).         *)
(*                                                                       *)
(* The big spec's stronger contract — task_blockers ⊆ globalBlockers —  *)
(* is captured at reviewer-pinning time (the ReviewContinue action      *)
(* requires `taskCarriers \subseteq GlobalBlockers`).  After acceptance,*)
(* subsequent worker / operator deltas may drop a carrier from the live *)
(* set; the kernel's relegalize step prunes stale carriers, modeled    *)
(* here by carrier-pruning in EditConfiguredTargets.                   *)
(***************************************************************************)
PendingTaskStaging ==
    /\ hasPendingTask => stage \in {"Start", "Worker"}
    /\ (~ hasPendingTask) <=> (pendingTaskKind = "none")
    /\ pendingTaskCarriers \subseteq (presentNodes \cup configuredTargets)

(***************************************************************************)
(* PhaseDormancyContract.  Variables that are phase-scoped must be in   *)
(* their dormant state when the phase doesn't apply.                    *)
(***************************************************************************)
PhaseDormancyContract ==
    /\ phase # "proof_formalization"
        => /\ activeCoarseNode = NoNode
           /\ forceReviewAfterConeClean = FALSE
           /\ globalRepairStep = "none"
    /\ phase \notin {"theorem_stating", "proof_formalization"}
        => /\ stuckMathAuditActive = FALSE
           /\ assumptionLaneActive = FALSE
    /\ phase = "theorem_stating"
        => (stuckMathAuditActive => assumptionLaneActive)
    /\ phase # "cleanup"
        => cleanupAuditActive = FALSE
    /\ phase = "complete"
        => /\ stage = "Start"
           /\ ~ hasPendingTask
           /\ activeNode = NoNode
           /\ activeCoarseNode = NoNode

(***************************************************************************)
(* QuiescentLiveEqualsCommitted.  At a quiescent resting point (Start, *)
(* no in-flight request), there is no pending Cleanup audit active.    *)
(*                                                                        *)
(* The big spec's `QuiescentLiveEqualsCommitted` checks that the live   *)
(* and committed tier mirrors agree at quiescent rest points; this spec *)
(* abstracts the live/committed split away, so the analog here is the   *)
(* structural property "no role is mid-flight when we're resting".      *)
(***************************************************************************)
QuiescentLiveEqualsCommitted ==
    (stage = "Start" /\ inFlightRequestKind = "none")
        => /\ gateKind = "none"
           /\ ~ cleanupAuditActive \/ phase = "cleanup"
           /\ ~ needInputAuditorActive
           /\ ~ assumptionLaneActive

(***************************************************************************)
(* GlobalBlockersExhaustive.  GlobalBlockers includes exactly the      *)
(* present nodes that have a non-Pass lane status (respecting phase    *)
(* dormancy for Substantiveness) and the configured targets that have a *)
(* non-Pass faithfulness status.                                        *)
(*                                                                       *)
(* This is structural by construction in `GlobalBlockers`'s definition; *)
(* the invariant restates it as the canonical contract.                  *)
(***************************************************************************)
GlobalBlockersExhaustive ==
    /\ \A n \in presentNodes :
        n \in GlobalBlockers
            <=> NodeBlockersActive(n)
    /\ \A t \in configuredTargets :
        t \in GlobalBlockers
            <=> faithfulnessStatus[t] # "pass"
    /\ \A t \in ChallengeTargets :
        t \in GlobalBlockers
            <=> ChallengeTargetBlocked(t)

(***************************************************************************)
(* ReviewerScopeAuthorizationComplete.  When the reviewer owns            *)
(* proof-formalization routing and no global-repair request/grant is      *)
(* active, the reviewer has a global-repair escape hatch available. The   *)
(* core spec does not model the concrete proposed node set or the         *)
(* paper-protected semantic closure; the detailed spec refines this into  *)
(* "any non-empty subset of present non-protected nodes".                 *)
(***************************************************************************)
ReviewerScopeAuthorizationComplete ==
    (/\ phase = "proof_formalization"
     /\ stage = "Reviewer"
     /\ inFlightRequestKind = "reviewer"
     /\ globalRepairStep = "none")
        => ENABLED RequestGlobalRepairAudit

(***************************************************************************)
(* GlobalRepairLifecycle.  Step progression is monotone within a       *)
(* request: none → request_pending → grant_available → none.           *)
(*                                                                       *)
(* TLA invariants are single-state; this captures the state-level       *)
(* preconditions per step:                                               *)
(*   - request_pending => stuckMathAuditActive (the audit role owns the *)
(*     next response).                                                   *)
(*   - grant_available => stage \in {Reviewer, Start} (waiting for the  *)
(*     reviewer Continue that consumes the grant).                       *)
(***************************************************************************)
GlobalRepairLifecycle ==
    /\ globalRepairStep = "request_pending"
        => /\ stuckMathAuditActive
           /\ ~ assumptionLaneActive
           /\ phase = "proof_formalization"
    /\ globalRepairStep = "grant_available"
        => phase = "proof_formalization"

(***************************************************************************)
(* SingleAuditAtATime.  At most one of the three audit lanes is active *)
(* at a given state.  The three lanes are distinct, but they can't run *)
(* concurrently — each owns its own stage.                              *)
(***************************************************************************)
SingleAuditAtATime ==
    Cardinality(
        {1 : i \in {1} \cap (IF cleanupAuditActive       THEN {1} ELSE {})}
            \cup
        {2 : i \in {2} \cap (IF stuckMathAuditActive     THEN {2} ELSE {})}
            \cup
        {3 : i \in {3} \cap (IF needInputAuditorActive   THEN {3} ELSE {})}
            \cup
        {4 : i \in {4} \cap (IF gapPlannerActive         THEN {4} ELSE {})}
            \cup
        {5 : i \in {5} \cap (IF gapCriticActive          THEN {5} ELSE {})}
    ) <= 1

(***************************************************************************)
(* AuditStageConsistency.  Each audit-lane stage is reached iff that   *)
(* lane's active flag is set.                                           *)
(***************************************************************************)
AuditStageConsistency ==
    /\ stage = "CleanupAudit"      => cleanupAuditActive
    /\ stage = "StuckMathAudit"    => stuckMathAuditActive
    /\ stage = "NeedInputAuditor"  => needInputAuditorActive
    /\ stage = "GapResearchPlanner" => gapPlannerActive
    /\ stage = "GapPlanCritic"     => gapCriticActive
    /\ assumptionLaneActive =>
          /\ stuckMathAuditActive
          /\ stage = "StuckMathAudit"

(***************************************************************************)
(* GapLoopTerminates.  The planner ↔ critic loop cannot livelock: the     *)
(* bounded reject counter never exceeds its ceiling, and whenever it is   *)
(* AT the ceiling the protocol is not parked in a critic/planner state    *)
(* that could re-increment it — i.e. the loop must have terminated into a *)
(* human halt (→ HumanGate) on the lap that reached the ceiling. The      *)
(* escalate branch of AcceptGapPlanCritic models the kernel's             *)
(* post-increment semantics (gapRejectCount' = gapRejectCount + 1 =       *)
(* GapRejectLimit, reset to 0 only by HumanFeedback), so the ceiling is   *)
(* genuinely reachable and the second conjunct here is non-vacuous —      *)
(* exercised at the escalation state. Together with TypeOK's              *)
(* `gapRejectCount \in 0..GapRejectLimit` bound this rules out an         *)
(* unbounded plan/reject/re-plan spin.                                     *)
(***************************************************************************)
GapLoopTerminates ==
    /\ gapRejectCount <= GapRejectLimit
    /\ (gapRejectCount = GapRejectLimit) => (~ gapCriticActive /\ ~ gapPlannerActive)

(***************************************************************************)
(* GapStageRejectConsistency.  The gap-loop active flags are confined to  *)
(* their own stages, and the planner/critic flags are mutually exclusive. *)
(***************************************************************************)
GapStageRejectConsistency ==
    /\ gapPlannerActive => (stage = "GapResearchPlanner")
    /\ gapCriticActive  => (stage = "GapPlanCritic")
    /\ ~ (gapPlannerActive /\ gapCriticActive)

(***************************************************************************)
(* TOP-LEVEL AGGREGATE.  Useful when configuring TLC with a single      *)
(* invariant name.                                                       *)
(***************************************************************************)
ProjectInvariants ==
    /\ TypeOK
    /\ HumanGateMatchesState
    /\ NeedInputReentryGuarded
    /\ InFlightKindMatchesStage
    /\ CleanupHasNoBlockers
    /\ NoAdvancePhaseWithBlockers
    /\ CompleteRequiresChallengeCoverage
    /\ CleanupDoneTerminal
    /\ StalePassClosurePreventsCleanupAdvance
    /\ ClosureCoverageTotal
    /\ ClosureStatusMutex
    /\ CrossTargetFormalIsolation
    /\ CoarseAnchorSafe
    /\ AnchorChangeForbiddenDuringGlobalRepair
    /\ LocalModeSoundnessCarveOut
    /\ AuthorizedNodesScopeContract
    /\ PendingTaskStaging
    /\ PhaseDormancyContract
    /\ QuiescentLiveEqualsCommitted
    /\ GlobalBlockersExhaustive
    /\ ReviewerScopeAuthorizationComplete
    /\ GlobalRepairLifecycle
    /\ SingleAuditAtATime
    /\ AuditStageConsistency
    /\ GapLoopTerminates
    /\ GapStageRejectConsistency
    \* PV mode-B prove-or-disprove invariants (forward design).
    /\ ModeBConfiguredTargetsEmpty
    /\ SoundnessFloor
    /\ NotBothSidesClosed
    /\ NoHumanInModeB
    /\ UnderModelAssumptionLifecycleContract
    /\ CoverageOnLiveOnly
    /\ DecidedResolves

(***************************************************************************)
(* AuditSoleFlipper (ACTION property).  `challengePolarity` changes only  *)
(* across an AcceptStuckMathAudit step — no worker / reviewer / verifier  *)
(* / human action mutates it.  This is a TWO-STATE property: it relates   *)
(* the pre- and post-state, so it is checked as `[][AuditSoleFlipper]_Vars *)
(* under PROPERTIES (not as a single-state INVARIANT).  Operationally it  *)
(* is the machine-checked form of the UNCHANGED-PolarityVars discipline   *)
(* every non-audit action carries.                                        *)
(*                                                                       *)
(* polarityFlips and challengeClosedSide may ALSO change on the           *)
(* challenge-close worker step (closing the live side) and on the audit   *)
(* flip; the load-bearing sole-flipper claim is specifically about        *)
(* `challengePolarity` (the live-side selector), so that is what this     *)
(* property pins.                                                        *)
(***************************************************************************)
AuditSoleFlipper ==
    (challengePolarity' # challengePolarity) =>
        /\ stage = "StuckMathAudit"
        /\ inFlightRequestKind = "stuck_math_audit"

(***************************************************************************)
(* DoneReachable (BAIT invariant).  Done IS reachable, but reachability   *)
(* is an existential/liveness claim, not a safety invariant.  Under       *)
(* simulation we check it the standard TLC way: assert the NEGATION as an *)
(* invariant (`phase # "complete"`); a sim that drives a run to           *)
(* completion produces a counterexample trace, which is the WITNESS that  *)
(* Done is reachable.  A clean (no-violation) sim run is therefore the    *)
(* NON-result here — see the sim report.  Keep this OUT of the green cfg; *)
(* it lives in a dedicated bait cfg.                                      *)
(***************************************************************************)
DoneNeverComplete == phase # "complete"

(***************************************************************************)
(* ---------------------- LIVENESS (sketch) ---------------------------- *)
(*                                                                       *)
(* The core spec's liveness goals are:                                  *)
(*   * `EventuallyCleanupReachable`: under fair execution, the protocol *)
(*     either reaches phase = cleanup or phase = complete eventually.   *)
(*   * `NoStuckProtocol`: at every non-terminal state, some Next       *)
(*     disjunct is enabled.                                              *)
(*                                                                       *)
(* These are stated below as `WF_Vars(Next)` plus the temporal goals.  *)
(* They are documented as sketches; the spec's Spec definition does    *)
(* not include them, because TLC sim mode does not verify temporal     *)
(* properties.  A future exhaustive run can lift these.                *)
(***************************************************************************)
EventuallyComplete == <>(phase = "complete")

EventuallyCleanupReachable == <>(phase \in {"cleanup", "complete"})

NoStuckProtocol == [](phase = "complete" \/ ENABLED Next)

=============================================================================
