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
    NoNode,
    MaxCycle,
    InitialConfiguredTargets,
    InitialPresentNodes

ASSUME Nodes # {}
ASSUME Targets # {}
ASSUME NoNode \notin Nodes
ASSUME InitialConfiguredTargets \subseteq Targets
ASSUME InitialPresentNodes \subseteq Nodes
ASSUME MaxCycle \in Nat \ {0}

(***************************************************************************)
(* Enums.  Stated as plain string-set constants so TLC observes a closed   *)
(* type universe per variable.                                             *)
(***************************************************************************)

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
        "NeedInputAuditor"
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
        "need_input_auditor"
    }

GateKinds == {"none", "advance", "need_input", "protected_reapproval"}

(***************************************************************************)
(* Worker outcome taxonomy.  The kernel's four-value enum maps directly.   *)
(***************************************************************************)
WorkerOutcomes == {"valid", "invalid", "stuck", "needs_restructure"}

(***************************************************************************)
(* Worker mode.  Four values per partition decision #5.                    *)
(***************************************************************************)
WorkerModes == {"local", "restructure", "coarse_restructure", "cleanup"}

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
    pendingProtectedReapproval

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
    needInputAuditorActive

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

(***************************************************************************)
(* Cluster aliases.  Used for UNCHANGED in actions.                       *)
(***************************************************************************)

PhaseStageVars ==
    <<phase, stage, cycle, activeNode, activeCoarseNode>>

StructureVars ==
    <<presentNodes, openNodes, coverage, approvedCoverage,
      configuredTargets, approvedConfiguredTargets>>

CoarseDagVars == <<coarseDagNodes>>

LaneStatusVars ==
    <<correspondenceStatus, substantivenessStatus,
      soundnessStatus, deviationStatus, faithfulnessStatus>>

LocalClosureVars == <<localClosureStatus>>

AuthorizedScopeVars == <<authorizedNodes>>

GateVars ==
    <<gateKind, humanInputOutstanding, pendingProtectedReapproval>>

PendingTaskVars ==
    <<hasPendingTask, pendingTaskKind, pendingTaskCarriers, workerMode>>

AuditLaneVars ==
    <<cleanupAuditActive, stuckMathAuditActive, needInputAuditorActive>>

RoutingLatchVars ==
    <<postAdvanceRoutingPending, forceReviewAfterConeClean>>

GlobalRepairVars == <<globalRepairStep>>

ClosureHistoryVars == <<cyclesSinceClean, hasEverBeenClean>>

InFlightVars == <<inFlightRequestKind>>

Vars ==
    <<phase, stage, cycle, activeNode, activeCoarseNode,
      presentNodes, openNodes, coverage, approvedCoverage,
      configuredTargets, approvedConfiguredTargets,
      coarseDagNodes,
      correspondenceStatus, substantivenessStatus, soundnessStatus,
      deviationStatus, faithfulnessStatus,
      localClosureStatus, authorizedNodes,
      gateKind, humanInputOutstanding, pendingProtectedReapproval,
      hasPendingTask, pendingTaskKind, pendingTaskCarriers, workerMode,
      cleanupAuditActive, stuckMathAuditActive, needInputAuditorActive,
      postAdvanceRoutingPending, forceReviewAfterConeClean,
      globalRepairStep,
      cyclesSinceClean, hasEverBeenClean,
      inFlightRequestKind>>

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
    /\ presentNodes = InitialPresentNodes
    /\ openNodes = {}
    /\ coverage = [t \in Targets |-> {}]
    /\ approvedCoverage = [t \in Targets |-> {}]
    /\ configuredTargets = InitialConfiguredTargets
    /\ approvedConfiguredTargets = {}
    /\ coarseDagNodes = {}
    /\ correspondenceStatus  = NoStatus("unknown")
    /\ substantivenessStatus = NoStatus("unknown")
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
    /\ hasPendingTask = FALSE
    /\ pendingTaskKind = "none"
    /\ pendingTaskCarriers = {}
    /\ workerMode = "local"
    /\ cleanupAuditActive = FALSE
    /\ stuckMathAuditActive = FALSE
    /\ needInputAuditorActive = FALSE
    /\ postAdvanceRoutingPending = FALSE
    /\ forceReviewAfterConeClean = FALSE
    /\ globalRepairStep = "none"
    /\ cyclesSinceClean = 0
    /\ hasEverBeenClean = FALSE
    /\ inFlightRequestKind = "none"

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
                /\ UNCHANGED <<stuckMathAuditActive, needInputAuditorActive>>
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
(* abstract spec, we just transition to one of the verifier stages       *)
(* non-deterministically; the verifier sequencing invariant is a sub-    *)
(* refinement.                                                            *)
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
(* abstract spec, we transition to one of the verifier stages non-       *)
(* deterministically; the verifier sequencing invariant is a sub-        *)
(* refinement.                                                            *)
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
                /\ \E n \in presentNodes : DriftNodeStatusesToUnknown(n)
                /\ UNCHANGED localClosureStatus
             \/ \* No structural delta — single target faithfulness drift.
                /\ UNCHANGED StructureVars
                /\ \E t \in configuredTargets :
                       DriftTargetFaithfulnessToUnknown(t)
                /\ UNCHANGED localClosureStatus
             \/ \* Node closure: removes a single sorry from openNodes.
                /\ \E n \in openNodes :
                    /\ openNodes' = openNodes \ {n}
                    /\ presentNodes' = presentNodes
                    /\ coverage' = coverage
                /\ UNCHANGED <<approvedCoverage, configuredTargets,
                               approvedConfiguredTargets>>
                /\ UNCHANGED LaneStatusVars
                /\ UNCHANGED localClosureStatus
             \/ \* Worker invalidates a node's local-closure record
                \* (kernel: dep-edit fingerprint drift).
                /\ UNCHANGED StructureVars
                /\ UNCHANGED LaneStatusVars
                /\ \E n \in presentNodes :
                    /\ n \notin openNodes
                    /\ localClosureStatus' =
                            [localClosureStatus EXCEPT ![n] = "unverified"]
       \/ \* Valid-without-delta — routes directly to Reviewer.
          /\ stage' = "Reviewer"
          /\ inFlightRequestKind' = "reviewer"
          /\ UNCHANGED StructureVars
          /\ UNCHANGED LaneStatusVars
          /\ UNCHANGED LocalClosureVars
       \/ \* Invalid or Stuck — re-issue worker (retry) or escalate
          \* to Reviewer.  The spec collapses retry counters away.
          /\ \/ /\ stage' = "Worker"
                /\ inFlightRequestKind' = "worker"
             \/ /\ stage' = "Reviewer"
                /\ inFlightRequestKind' = "reviewer"
          /\ UNCHANGED StructureVars
          /\ UNCHANGED LaneStatusVars
          /\ UNCHANGED LocalClosureVars
       \/ \* NeedsRestructure — always to Reviewer.
          /\ stage' = "Reviewer"
          /\ inFlightRequestKind' = "reviewer"
          /\ UNCHANGED StructureVars
          /\ UNCHANGED LaneStatusVars
          /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED GateVars
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
                  correspondenceStatus' =
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
           /\ UNCHANGED <<stuckMathAuditActive, needInputAuditorActive>>
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

(***************************************************************************)
(* AcceptStuckMathAudit.  StuckMathAudit returns to Reviewer (and may    *)
(* fire a cone-clean reset, setting forceReviewAfterConeClean).          *)
(***************************************************************************)
AcceptStuckMathAudit ==
    /\ stage = "StuckMathAudit"
    /\ inFlightRequestKind = "stuck_math_audit"
    /\ phase = "proof_formalization"
    \* When the global-repair lane has a request pending, the
    \* AcceptGlobalRepairGrant action consumes the audit response
    \* instead (Step B).
    /\ globalRepairStep # "request_pending"
    /\ stage' = "Reviewer"
    /\ inFlightRequestKind' = "reviewer"
    /\ stuckMathAuditActive' = FALSE
    /\ \E coneClean \in BOOLEAN :
            forceReviewAfterConeClean' = coneClean
    /\ UNCHANGED <<cleanupAuditActive, needInputAuditorActive>>
    /\ UNCHANGED postAdvanceRoutingPending
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED GateVars
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars

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
           /\ stage' = "HumanGate"
           /\ inFlightRequestKind' = "human_gate"
           /\ gateKind' = "need_input"
        \/ /\ ~ confirm
           /\ stage' = "Reviewer"
           /\ inFlightRequestKind' = "reviewer"
           /\ gateKind' = "none"
    /\ needInputAuditorActive' = FALSE
    /\ UNCHANGED <<cleanupAuditActive, stuckMathAuditActive>>
    /\ UNCHANGED <<humanInputOutstanding, pendingProtectedReapproval>>
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

(* ReviewNeedInput sets up a NeedInputAuditor follow-up. *)
ReviewNeedInput ==
    /\ stage = "Reviewer"
    /\ inFlightRequestKind = "reviewer"
    /\ stage' = "NeedInputAuditor"
    /\ inFlightRequestKind' = "need_input_auditor"
    /\ needInputAuditorActive' = TRUE
    /\ UNCHANGED <<cleanupAuditActive, stuckMathAuditActive>>
    /\ humanInputOutstanding' = FALSE  \* NeedInputAuditor may later confirm
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED <<gateKind, pendingProtectedReapproval>>
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars

(***************************************************************************)
(* ReviewAdvancePhase.  The reviewer signals phase advance.  Legal only  *)
(* when globalBlockers is empty.  Routes to HumanGate with               *)
(* gateKind = advance.                                                    *)
(***************************************************************************)
ReviewAdvancePhase ==
    /\ stage = "Reviewer"
    /\ inFlightRequestKind = "reviewer"
    /\ phase \in {"theorem_stating", "proof_formalization"}
    /\ GlobalBlockers = {}
    /\ stage' = "HumanGate"
    /\ inFlightRequestKind' = "human_gate"
    /\ gateKind' = "advance"
    /\ hasPendingTask' = FALSE
    /\ pendingTaskKind' = "none"
    /\ pendingTaskCarriers' = {}
    /\ UNCHANGED workerMode
    /\ UNCHANGED <<phase, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED StructureVars
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED <<humanInputOutstanding, pendingProtectedReapproval>>
    /\ UNCHANGED AuditLaneVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars

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
    /\ UNCHANGED <<presentNodes, openNodes, coverage, configuredTargets>>
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED <<humanInputOutstanding, pendingProtectedReapproval>>
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
    /\ UNCHANGED <<presentNodes, openNodes, coverage, configuredTargets>>
    /\ UNCHANGED CoarseDagVars
    /\ UNCHANGED LaneStatusVars
    /\ UNCHANGED LocalClosureVars
    /\ UNCHANGED AuthorizedScopeVars
    /\ UNCHANGED humanInputOutstanding
    /\ UNCHANGED PendingTaskVars
    /\ UNCHANGED AuditLaneVars
    /\ UNCHANGED RoutingLatchVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ClosureHistoryVars

HumanFeedback ==
    /\ stage = "HumanGate"
    /\ inFlightRequestKind = "human_gate"
    /\ gateKind \in {"advance", "need_input", "protected_reapproval"}
    /\ stage' = "Reviewer"
    /\ inFlightRequestKind' = "reviewer"
    /\ gateKind' = "none"
    /\ humanInputOutstanding' = TRUE
    /\ UNCHANGED pendingProtectedReapproval
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
    /\ UNCHANGED humanInputOutstanding
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
    /\ \E newConfigured \in SUBSET Targets :
        /\ newConfigured # configuredTargets
        /\ configuredTargets' = newConfigured
        /\ \* Pending-task target carriers are pruned to the new
           \* configured set (kernel: relegalize step drops carriers
           \* that no longer correspond to live blockers).
           pendingTaskCarriers' =
                {c \in pendingTaskCarriers :
                    c \in (presentNodes \cup newConfigured)}
    /\ UNCHANGED approvedConfiguredTargets
    /\ UNCHANGED <<phase, stage, cycle, activeNode, activeCoarseNode>>
    /\ UNCHANGED <<presentNodes, openNodes, coverage, approvedCoverage>>
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
    /\ UNCHANGED InFlightVars

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
    /\ UNCHANGED InFlightVars

(***************************************************************************)
(* Next: disjunction of all actions.                                     *)
(***************************************************************************)
Next ==
    \/ StartCycle
    \/ AcceptWorker
    \/ AcceptVerifier
    \/ AcceptCleanupAudit
    \/ AcceptStuckMathAudit
    \/ AcceptNeedInputAuditor
    \/ ReviewContinue
    \/ ReviewNeedInput
    \/ ReviewAdvancePhase
    \/ ReviewDone
    \/ HumanApproveAdvance
    \/ HumanApproveProtectedReapproval
    \/ HumanFeedback
    \/ MaybeIssueProtectedReapprovalGate
    \/ EditConfiguredTargets
    \/ RequestGlobalRepairAudit
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
    /\ hasPendingTask \in BOOLEAN
    /\ pendingTaskKind \in PendingTaskKinds
    /\ pendingTaskCarriers \subseteq (Nodes \cup Targets)
    /\ workerMode \in WorkerModes
    /\ cleanupAuditActive \in BOOLEAN
    /\ stuckMathAuditActive \in BOOLEAN
    /\ needInputAuditorActive \in BOOLEAN
    /\ postAdvanceRoutingPending \in BOOLEAN
    /\ forceReviewAfterConeClean \in BOOLEAN
    /\ globalRepairStep \in GlobalRepairSteps
    /\ cyclesSinceClean \in 0..MaxCycle
    /\ hasEverBeenClean \in BOOLEAN
    /\ inFlightRequestKind \in RequestKinds

(***************************************************************************)
(* HumanGateMatchesState.  gateKind is non-`none` iff we are at a       *)
(* HumanGate stage.                                                      *)
(***************************************************************************)
HumanGateMatchesState ==
    (gateKind # "none") <=> (stage = "HumanGate")

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
           /\ stuckMathAuditActive = FALSE
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
    ) <= 1

(***************************************************************************)
(* AuditStageConsistency.  Each audit-lane stage is reached iff that   *)
(* lane's active flag is set.                                           *)
(***************************************************************************)
AuditStageConsistency ==
    /\ stage = "CleanupAudit"     => cleanupAuditActive
    /\ stage = "StuckMathAudit"   => stuckMathAuditActive
    /\ stage = "NeedInputAuditor" => needInputAuditorActive

(***************************************************************************)
(* TOP-LEVEL AGGREGATE.  Useful when configuring TLC with a single      *)
(* invariant name.                                                       *)
(***************************************************************************)
ProjectInvariants ==
    /\ TypeOK
    /\ HumanGateMatchesState
    /\ InFlightKindMatchesStage
    /\ CleanupHasNoBlockers
    /\ NoAdvancePhaseWithBlockers
    /\ CleanupDoneTerminal
    /\ StalePassClosurePreventsCleanupAdvance
    /\ ClosureCoverageTotal
    /\ ClosureStatusMutex
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
