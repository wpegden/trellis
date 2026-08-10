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
SimChallengeTargets == {"c1"}
SimNoNode == "NONE"
SimMaxCycle == 2
SimInitialConfiguredTargets == SimTargets
SimInitialPresentNodes == {"n1"}
\* Phase IV step 12: all-Lean backend axis. One backend ⇒ nodeTarget is the
\* constant `"lean"` map and CrossTargetFormalIsolation is vacuous; this run
\* is the regression check that the new immutable variable / invariant don't
\* disturb the existing Core invariants under exhaustive BFS.
SimBackends == {"lean"}
SimTabletTarget == "lean"
SimFormalImports == {}
\* PV mode-B / prove-or-disprove axis (forward design). This DEFAULT harness
\* runs mode-A (GoalMode = "prose") so it keeps the existing paper-faithfulness
\* behavior (non-empty SimInitialConfiguredTargets) and existing invariant
\* coverage unchanged; the mode-B prove-or-disprove behavior is exercised by
\* the dedicated SupervisorCoreSim_modeb harness. The polarity vars are still
\* present here (one Decide challenge target `c1`, abstract truth = Prove);
\* the audit lane may flip up to SimMaxPolarityFlips times.
SimGoalMode == "prose"
SimMaxPolarityFlips == 2
SimMaxSidecarCloses == 1
SimChallengeTruth == [t \in SimChallengeTargets |-> "prove"]

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
    challengeCoverage,
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
    stagedUnderModelAssumptionDraft,
    pendingUnderModelAssumptions,
    hasPendingTask,
    pendingTaskKind,
    pendingTaskCarriers,
    workerMode,
    cleanupAuditActive,
    stuckMathAuditActive,
    needInputAuditorActive,
    assumptionLaneActive,
    gapPlannerActive,
    gapCriticActive,
    gapRejectCount,
    postAdvanceRoutingPending,
    forceReviewAfterConeClean,
    globalRepairStep,
    cyclesSinceClean,
    hasEverBeenClean,
    inFlightRequestKind,
    nodeTarget,
    challengePolarity,
    polarityFlips,
    challengeClosedSide,
    sidecarCloses,
    sidecarQueue

Core == INSTANCE SupervisorCore
    WITH
        Nodes <- SimNodes,
        Targets <- SimTargets,
        ChallengeTargets <- SimChallengeTargets,
        NoNode <- SimNoNode,
        MaxCycle <- SimMaxCycle,
        InitialConfiguredTargets <- SimInitialConfiguredTargets,
        InitialPresentNodes <- SimInitialPresentNodes,
        Backends <- SimBackends,
        TabletTarget <- SimTabletTarget,
        FormalImports <- SimFormalImports,
        GoalMode <- SimGoalMode,
        MaxPolarityFlips <- SimMaxPolarityFlips,
        MaxSidecarCloses <- SimMaxSidecarCloses,
        ChallengeTruth <- SimChallengeTruth,
        AssumptionsNode <- "n1",
        phase <- phase,
        stage <- stage,
        cycle <- cycle,
        activeNode <- activeNode,
        activeCoarseNode <- activeCoarseNode,
        presentNodes <- presentNodes,
        openNodes <- openNodes,
        coverage <- coverage,
        approvedCoverage <- approvedCoverage,
        challengeCoverage <- challengeCoverage,
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
        stagedUnderModelAssumptionDraft <- stagedUnderModelAssumptionDraft,
        pendingUnderModelAssumptions <- pendingUnderModelAssumptions,
        hasPendingTask <- hasPendingTask,
        pendingTaskKind <- pendingTaskKind,
        pendingTaskCarriers <- pendingTaskCarriers,
        workerMode <- workerMode,
        cleanupAuditActive <- cleanupAuditActive,
        stuckMathAuditActive <- stuckMathAuditActive,
        needInputAuditorActive <- needInputAuditorActive,
        assumptionLaneActive <- assumptionLaneActive,
        gapPlannerActive <- gapPlannerActive,
        gapCriticActive <- gapCriticActive,
        gapRejectCount <- gapRejectCount,
        postAdvanceRoutingPending <- postAdvanceRoutingPending,
        forceReviewAfterConeClean <- forceReviewAfterConeClean,
        globalRepairStep <- globalRepairStep,
        cyclesSinceClean <- cyclesSinceClean,
        hasEverBeenClean <- hasEverBeenClean,
        inFlightRequestKind <- inFlightRequestKind,
        nodeTarget <- nodeTarget,
        challengePolarity <- challengePolarity,
        polarityFlips <- polarityFlips,
        challengeClosedSide <- challengeClosedSide,
        sidecarCloses <- sidecarCloses,
        sidecarQueue <- sidecarQueue

Spec == Core!Spec

TypeOK == Core!TypeOK
HumanGateMatchesState == Core!HumanGateMatchesState
NeedInputReentryGuarded == Core!NeedInputReentryGuarded
InFlightKindMatchesStage == Core!InFlightKindMatchesStage
CleanupHasNoBlockers == Core!CleanupHasNoBlockers
NoAdvancePhaseWithBlockers == Core!NoAdvancePhaseWithBlockers
CompleteRequiresChallengeCoverage == Core!CompleteRequiresChallengeCoverage
CleanupDoneTerminal == Core!CleanupDoneTerminal
StalePassClosurePreventsCleanupAdvance == Core!StalePassClosurePreventsCleanupAdvance
CrossTargetFormalIsolation == Core!CrossTargetFormalIsolation
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
GapLoopTerminates == Core!GapLoopTerminates
GapStageRejectConsistency == Core!GapStageRejectConsistency
SidecarQueueSubsetOpen == Core!SidecarQueueSubsetOpen
\* PV mode-B prove-or-disprove invariants (forward design).
ModeBConfiguredTargetsEmpty == Core!ModeBConfiguredTargetsEmpty
SoundnessFloor == Core!SoundnessFloor
NotBothSidesClosed == Core!NotBothSidesClosed
NoHumanInModeB == Core!NoHumanInModeB
UnderModelAssumptionLifecycleContract == Core!UnderModelAssumptionLifecycleContract
CoverageOnLiveOnly == Core!CoverageOnLiveOnly
DecidedResolves == Core!DecidedResolves
AuditSoleFlipper == Core!AuditSoleFlipper
AuditSoleFlipperProp == [][AuditSoleFlipper]_(Core!Vars)
DoneNeverComplete == Core!DoneNeverComplete
ProjectInvariants == Core!ProjectInvariants

=============================================================================
