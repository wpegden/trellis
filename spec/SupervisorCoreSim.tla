------------------------------- MODULE SupervisorCoreSim -------------------------------
EXTENDS Integers, FiniteSets, Sequences

(***************************************************************************)
(* Concrete-bounds harness for SupervisorCore.tla.  Mirrors the           *)
(* SupervisorProtocolSim shape — the only TLC-driven harness for the     *)
(* core spec.                                                             *)
(*                                                                       *)
(* Bounds are intentionally small.  The small spec exists precisely so  *)
(* exhaustive search is tractable where the big spec's couldn't be; the *)
(* sim config file `SupervisorCoreSim.cfg` runs TLC in exhaustive       *)
(* (BFS) mode at these bounds.                                           *)
(*                                                                       *)
(* The carrier sets used here are smaller than the big sim's so the     *)
(* core spec's state-space search stays bounded.  Bumping these is      *)
(* safe in principle but TLC's per-state size scales with the           *)
(* product (|Nodes| × |Targets|) for the lane-status maps, so doubling *)
(* either constant multiplies the state space.                          *)
(***************************************************************************)

(***************************************************************************)
(* Exhaustive-tractable bounds: 1 node, 1 target, MaxCycle = 2.            *)
(* TLC's BFS completes in ~16s at ~270K distinct states / depth 19.      *)
(*                                                                       *)
(* Expanding to 2 nodes pushes the per-cycle successor count up by ~9x  *)
(* (single-node lane-status-flip × 5 lanes plus a 6-way next-stage     *)
(* choice per verifier accept).  TLC's BFS at 2 nodes runs out of      *)
(* memory or time before completing depth.  Sim mode at 2 nodes is the *)
(* alternative; see SupervisorCore.sim.cfg.                            *)
(***************************************************************************)
SimNodes == {"n1"}
SimTargets == {"t1"}
SimNoNode == "NONE"
SimMaxCycle == 2
SimInitialConfiguredTargets == SimTargets
SimInitialPresentNodes == {"n1"}

VARIABLES
    phase,
    stage,
    cycle,
    activeNode,
    activeCoarseNode,
    presentNodes,
    openNodes,
    coverage,
    approvedCoverage,
    configuredTargets,
    approvedConfiguredTargets,
    coarseDagNodes,
    correspondenceStatus,
    substantivenessStatus,
    soundnessStatus,
    deviationStatus,
    faithfulnessStatus,
    localClosureStatus,
    authorizedNodes,
    gateKind,
    humanInputOutstanding,
    pendingProtectedReapproval,
    hasPendingTask,
    pendingTaskKind,
    pendingTaskCarriers,
    workerMode,
    cleanupAuditActive,
    stuckMathAuditActive,
    needInputAuditorActive,
    postAdvanceRoutingPending,
    forceReviewAfterConeClean,
    globalRepairStep,
    cyclesSinceClean,
    hasEverBeenClean,
    inFlightRequestKind

Core == INSTANCE SupervisorCore
    WITH
        Nodes <- SimNodes,
        Targets <- SimTargets,
        NoNode <- SimNoNode,
        MaxCycle <- SimMaxCycle,
        InitialConfiguredTargets <- SimInitialConfiguredTargets,
        InitialPresentNodes <- SimInitialPresentNodes,
        phase <- phase,
        stage <- stage,
        cycle <- cycle,
        activeNode <- activeNode,
        activeCoarseNode <- activeCoarseNode,
        presentNodes <- presentNodes,
        openNodes <- openNodes,
        coverage <- coverage,
        approvedCoverage <- approvedCoverage,
        configuredTargets <- configuredTargets,
        approvedConfiguredTargets <- approvedConfiguredTargets,
        coarseDagNodes <- coarseDagNodes,
        correspondenceStatus <- correspondenceStatus,
        substantivenessStatus <- substantivenessStatus,
        soundnessStatus <- soundnessStatus,
        deviationStatus <- deviationStatus,
        faithfulnessStatus <- faithfulnessStatus,
        localClosureStatus <- localClosureStatus,
        authorizedNodes <- authorizedNodes,
        gateKind <- gateKind,
        humanInputOutstanding <- humanInputOutstanding,
        pendingProtectedReapproval <- pendingProtectedReapproval,
        hasPendingTask <- hasPendingTask,
        pendingTaskKind <- pendingTaskKind,
        pendingTaskCarriers <- pendingTaskCarriers,
        workerMode <- workerMode,
        cleanupAuditActive <- cleanupAuditActive,
        stuckMathAuditActive <- stuckMathAuditActive,
        needInputAuditorActive <- needInputAuditorActive,
        postAdvanceRoutingPending <- postAdvanceRoutingPending,
        forceReviewAfterConeClean <- forceReviewAfterConeClean,
        globalRepairStep <- globalRepairStep,
        cyclesSinceClean <- cyclesSinceClean,
        hasEverBeenClean <- hasEverBeenClean,
        inFlightRequestKind <- inFlightRequestKind

Spec == Core!Spec

TypeOK == Core!TypeOK
HumanGateMatchesState == Core!HumanGateMatchesState
InFlightKindMatchesStage == Core!InFlightKindMatchesStage
CleanupHasNoBlockers == Core!CleanupHasNoBlockers
NoAdvancePhaseWithBlockers == Core!NoAdvancePhaseWithBlockers
CleanupDoneTerminal == Core!CleanupDoneTerminal
StalePassClosurePreventsCleanupAdvance == Core!StalePassClosurePreventsCleanupAdvance
CoarseAnchorSafe == Core!CoarseAnchorSafe
AnchorChangeForbiddenDuringGlobalRepair == Core!AnchorChangeForbiddenDuringGlobalRepair
LocalModeSoundnessCarveOut == Core!LocalModeSoundnessCarveOut
AuthorizedNodesScopeContract == Core!AuthorizedNodesScopeContract
PendingTaskStaging == Core!PendingTaskStaging
PhaseDormancyContract == Core!PhaseDormancyContract
QuiescentLiveEqualsCommitted == Core!QuiescentLiveEqualsCommitted
GlobalBlockersExhaustive == Core!GlobalBlockersExhaustive
ReviewerScopeAuthorizationComplete == Core!ReviewerScopeAuthorizationComplete
GlobalRepairLifecycle == Core!GlobalRepairLifecycle
SingleAuditAtATime == Core!SingleAuditAtATime
AuditStageConsistency == Core!AuditStageConsistency
ProjectInvariants == Core!ProjectInvariants

=============================================================================
