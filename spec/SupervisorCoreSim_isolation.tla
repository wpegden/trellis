------------------------------- MODULE SupervisorCoreSim_isolation -------------------------------
EXTENDS Integers, FiniteSets, Sequences

(***************************************************************************)
(* Phase IV step 12 — CrossTargetFormalIsolation exercise harness.         *)
(*                                                                       *)
(* Standalone 2-node instance of SupervisorCore (the shared              *)
(* SupervisorCoreSim fixes SimNodes = {"n1"}, so it cannot exercise an   *)
(* import edge between two nodes).  Two backends are AVAILABLE            *)
(* (`SimBackends = {"lean","isabelle_hol"}`) and a real formal-import    *)
(* edge `n1 -> n2` is present, but `nodeTarget` is UNIFORM (`Core!Init`  *)
(* seeds every node to `TabletTarget = "lean"`), so                      *)
(* `CrossTargetFormalIsolation` must HOLD: this proves the uniform case  *)
(* stays safe even when a second backend is in the universe and an edge  *)
(* exists.  The non-uniform VIOLATION is the throwaway                   *)
(* SupervisorCoreSim_isolation_bugcheck (expected-fail, not in the green *)
(* suite).                                                               *)
(*                                                                       *)
(* Two nodes can blow up exhaustive BFS (see SupervisorCoreSim.tla §25), *)
(* so the .cfg drives `-simulate` (random) mode, not BFS.                *)
(***************************************************************************)
SimNodes == {"n1", "n2"}
SimTargets == {"t1"}
SimChallengeTargets == {"c1"}
SimNoNode == "NONE"
SimMaxCycle == 2
SimInitialConfiguredTargets == SimTargets
SimInitialPresentNodes == {"n1", "n2"}
\* Two backends available, but the tablet default is a single backend and
\* every node inherits it (uniform nodeTarget) ⇒ the invariant holds.
SimBackends == {"lean", "isabelle_hol"}
SimTabletTarget == "lean"
SimFormalImports == {<<"n1", "n2">>}
\* PV mode-B axis (forward design) — mode-A here (this harness targets the
\* CrossTargetFormalIsolation backend property, not the polarity axis).
SimGoalMode == "prose"
SimMaxPolarityFlips == 2
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
    challengeClosedSide

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
        challengeClosedSide <- challengeClosedSide

Spec == Core!Spec

TypeOK == Core!TypeOK
CrossTargetFormalIsolation == Core!CrossTargetFormalIsolation

=============================================================================
