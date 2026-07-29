------------------------------- MODULE SupervisorCoreSim_modeb_min -------------------------------
EXTENDS Integers, FiniteSets, Sequences

(***************************************************************************)
(* Minimal mode-B reachability harness: ONE node, ONE Decide challenge     *)
(* target whose abstract truth is the DISPROVE side.  The single target's  *)
(* default Prove side is FALSE, so the ONLY way to decide it (and reach    *)
(* Done) is the audit polarity flip → re-claim → disprove-side close.      *)
(* Minimizing node/target count removes the multi-node Pass-alignment      *)
(* burden, so a SHORT random sim can witness the disprove path and Done.   *)
(* Larger MaxCycle gives the (worker→verifier→reviewer→audit) sequence     *)
(* room.  Expected-FAIL under the bait cfg; NOT in the green suite.        *)
(***************************************************************************)
SimNodes == {"n1"}
SimTargets == {"t1"}
SimChallengeTargets == {"c1"}
SimNoNode == "NONE"
SimMaxCycle == 16
SimInitialConfiguredTargets == {}
SimInitialPresentNodes == {"n1"}
SimBackends == {"lean"}
SimTabletTarget == "lean"
SimFormalImports == {}
SimGoalMode == "lean"
SimMaxPolarityFlips == 2
SimMaxSidecarCloses == 1
\* The single challenge target is TRUE on the DISPROVE side.
SimChallengeTruth == [t \in SimChallengeTargets |-> "disprove"]

VARIABLES
    phase, stage, cycle, activeNode, activeCoarseNode,
    presentNodes, openNodes, coverage, approvedCoverage, challengeCoverage,
    configuredTargets, approvedConfiguredTargets, coarseDagNodes,
    correspondenceStatus, substantivenessStatus, soundnessStatus,
    deviationStatus, faithfulnessStatus, localClosureStatus, authorizedNodes,
    gateKind, humanInputOutstanding, pendingProtectedReapproval,
    stagedUnderModelAssumptionDraft, pendingUnderModelAssumptions,
    hasPendingTask, pendingTaskKind, pendingTaskCarriers, workerMode,
    cleanupAuditActive, stuckMathAuditActive, needInputAuditorActive,
    assumptionLaneActive,
    gapPlannerActive, gapCriticActive, gapRejectCount,
    postAdvanceRoutingPending, forceReviewAfterConeClean, globalRepairStep,
    cyclesSinceClean, hasEverBeenClean, inFlightRequestKind, nodeTarget,
    challengePolarity, polarityFlips, challengeClosedSide,
    sidecarCloses,
    sidecarQueue

Core == INSTANCE SupervisorCore
    WITH
        Nodes <- SimNodes, Targets <- SimTargets,
        ChallengeTargets <- SimChallengeTargets, NoNode <- SimNoNode,
        MaxCycle <- SimMaxCycle,
        InitialConfiguredTargets <- SimInitialConfiguredTargets,
        InitialPresentNodes <- SimInitialPresentNodes,
        Backends <- SimBackends, TabletTarget <- SimTabletTarget,
        FormalImports <- SimFormalImports, GoalMode <- SimGoalMode,
        MaxPolarityFlips <- SimMaxPolarityFlips,
        MaxSidecarCloses <- SimMaxSidecarCloses,
        ChallengeTruth <- SimChallengeTruth,
        AssumptionsNode <- "n1",
        phase <- phase, stage <- stage, cycle <- cycle,
        activeNode <- activeNode, activeCoarseNode <- activeCoarseNode,
        presentNodes <- presentNodes, openNodes <- openNodes,
        coverage <- coverage, approvedCoverage <- approvedCoverage,
        challengeCoverage <- challengeCoverage,
        configuredTargets <- configuredTargets,
        approvedConfiguredTargets <- approvedConfiguredTargets,
        coarseDagNodes <- coarseDagNodes,
        correspondenceStatus <- correspondenceStatus,
        substantivenessStatus <- substantivenessStatus,
        soundnessStatus <- soundnessStatus, deviationStatus <- deviationStatus,
        faithfulnessStatus <- faithfulnessStatus,
        localClosureStatus <- localClosureStatus,
        authorizedNodes <- authorizedNodes, gateKind <- gateKind,
        humanInputOutstanding <- humanInputOutstanding,
        pendingProtectedReapproval <- pendingProtectedReapproval,
        stagedUnderModelAssumptionDraft <- stagedUnderModelAssumptionDraft,
        pendingUnderModelAssumptions <- pendingUnderModelAssumptions,
        hasPendingTask <- hasPendingTask, pendingTaskKind <- pendingTaskKind,
        pendingTaskCarriers <- pendingTaskCarriers, workerMode <- workerMode,
        cleanupAuditActive <- cleanupAuditActive,
        stuckMathAuditActive <- stuckMathAuditActive,
        needInputAuditorActive <- needInputAuditorActive,
        assumptionLaneActive <- assumptionLaneActive,
        gapPlannerActive <- gapPlannerActive, gapCriticActive <- gapCriticActive,
        gapRejectCount <- gapRejectCount,
        postAdvanceRoutingPending <- postAdvanceRoutingPending,
        forceReviewAfterConeClean <- forceReviewAfterConeClean,
        globalRepairStep <- globalRepairStep,
        cyclesSinceClean <- cyclesSinceClean,
        hasEverBeenClean <- hasEverBeenClean,
        inFlightRequestKind <- inFlightRequestKind, nodeTarget <- nodeTarget,
        challengePolarity <- challengePolarity, polarityFlips <- polarityFlips,
        challengeClosedSide <- challengeClosedSide,
        sidecarCloses <- sidecarCloses,
        sidecarQueue <- sidecarQueue

Spec == Core!Spec
TypeOK == Core!TypeOK
SoundnessFloor == Core!SoundnessFloor
NotBothSidesClosed == Core!NotBothSidesClosed
NoHumanInModeB == Core!NoHumanInModeB
DecidedResolves == Core!DecidedResolves

\* Reachability baits (violation = witness).
NeverProofPhase == phase # "proof_formalization"
C1NeverFlipped == challengePolarity["c1"] # "disprove"
C1NeverDisproveClosed ==
    ~ (challengePolarity["c1"] = "disprove" /\ challengeClosedSide["c1"] = "closed")
DoneNeverComplete == phase # "complete"

=============================================================================
