------------------------------- MODULE SupervisorCoreSim_modeb_reach -------------------------------
EXTENDS Integers, FiniteSets, Sequences

(***************************************************************************)
(* PV mode-B REACHABILITY-witness harness (expected-FAIL under the bait    *)
(* cfg, NOT in the green suite).  Same Core spec as SupervisorCoreSim_modeb *)
(* but with a larger MaxCycle so a random sim can actually drive a run all  *)
(* the way to phase = "complete" (Done).  Two Decide targets with OPPOSITE  *)
(* abstract truths: `c1` true on Prove, `c2` true on Disprove — so a        *)
(* completing run MUST decide `c2` via the DISPROVE side (after the audit   *)
(* lane flips c2's live polarity), witnessing DecidedResolves on the        *)
(* disprove side and the symmetric soundness floor.                        *)
(***************************************************************************)
SimNodes == {"n1", "n2"}
SimTargets == {"t1"}
SimChallengeTargets == {"c1", "c2"}
SimNoNode == "NONE"
SimMaxCycle == 14
SimInitialConfiguredTargets == {}
SimInitialPresentNodes == {"n1", "n2"}
SimBackends == {"lean"}
SimTabletTarget == "lean"
SimFormalImports == {}
SimGoalMode == "lean"
SimMaxPolarityFlips == 2
SimChallengeTruth ==
    [t \in SimChallengeTargets |-> IF t = "c1" THEN "prove" ELSE "disprove"]

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
    challengePolarity, polarityFlips, challengeClosedSide

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
        challengeClosedSide <- challengeClosedSide

Spec == Core!Spec
TypeOK == Core!TypeOK
SoundnessFloor == Core!SoundnessFloor
NotBothSidesClosed == Core!NotBothSidesClosed
NoHumanInModeB == Core!NoHumanInModeB
DecidedResolves == Core!DecidedResolves
\* Bait: negation of reachability of Done.  A violation (counterexample) is
\* the WITNESS that Done is reachable in mode-B.
DoneNeverComplete == Core!DoneNeverComplete
\* Sharper bait: Done is never reached with c2 decided on the DISPROVE side.
\* A violation witnesses a completing run that resolved c2 by disproof.
DoneNeverViaDisprove ==
    ~ (/\ phase = "complete"
       /\ challengePolarity["c2"] = "disprove"
       /\ challengeClosedSide["c2"] = "closed")

\* Shallower milestone bait: c2 closed on the DISPROVE side (after an audit
\* flip), without requiring full Done.  A violation is the disprove-side
\* soundness-symmetry witness.
C2NeverDisproveClosed ==
    ~ (/\ challengePolarity["c2"] = "disprove"
       /\ challengeClosedSide["c2"] = "closed")

\* Shallowest bait: c2's live side is never flipped to "disprove".  A
\* violation witnesses that the polarity-flip lane (AcceptStuckMathAudit via
\* RequestPolarityAudit) is REACHABLE — the precondition for any disprove-side
\* resolution.  Shallower than C2NeverDisproveClosed (no close step needed).
C2NeverFlipped ==
    challengePolarity["c2"] # "disprove"

\* Reachability probes (baits): each violation witnesses that the named
\* milestone is reachable in mode-B.
NeverProofPhase == phase # "proof_formalization"
NeverReviewerInProof ==
    ~ (phase = "proof_formalization" /\ stage = "Reviewer")
NeverStuckAuditInProof == stage # "StuckMathAudit"

=============================================================================
