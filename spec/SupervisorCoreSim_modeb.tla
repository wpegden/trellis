------------------------------- MODULE SupervisorCoreSim_modeb -------------------------------
EXTENDS Integers, FiniteSets, Sequences

(***************************************************************************)
(* PV mode-B (Lean-goals / prove-or-disprove) concrete-bounds harness for *)
(* SupervisorCore.tla.  Mirrors SupervisorCoreSim but configures mode-B:  *)
(*                                                                       *)
(*   * GoalMode = "lean"  ⇒ the paper-faithfulness target bucket is empty *)
(*     (SimInitialConfiguredTargets = {}); byte-pinned challenge specs    *)
(*     are the sole target type.                                         *)
(*                                                                       *)
(*   * TWO Decide challenge targets with OPPOSITE abstract truths so one  *)
(*     sim run exercises BOTH polarities of the soundness floor: `c1` is  *)
(*     true on the Prove side, `c2` is true on the Disprove side.  A run  *)
(*     reaching Done must close `c2` via the DISPROVE side (after an      *)
(*     audit polarity flip), which is the symmetric-axioms witness.       *)
(*                                                                       *)
(* Two nodes (one covering node per challenge target) + MaxCycle = 4 to   *)
(* give the audit lane room to flip `c2` to Disprove and still reach      *)
(* completion.  Bounds are intentionally small; run under -simulate.      *)
(***************************************************************************)
SimNodes == {"n1", "n2"}
SimTargets == {"t1"}
SimChallengeTargets == {"c1", "c2"}
SimNoNode == "NONE"
SimMaxCycle == 4
\* Mode-B contract: the paper-faithfulness bucket is empty.
SimInitialConfiguredTargets == {}
SimInitialPresentNodes == {"n1", "n2"}
SimBackends == {"lean"}
SimTabletTarget == "lean"
SimFormalImports == {}
\* Mode-B / prove-or-disprove axis (forward design).
SimGoalMode == "lean"
SimMaxPolarityFlips == 2
SimMaxSidecarCloses == 1
\* Opposite abstract truths: c1 true on Prove, c2 true on Disprove.  c2's
\* live side starts at the default "prove" (a FALSE side), so the only way
\* to decide c2 is for the audit lane to flip it to "disprove" first — the
\* SymmetricAxioms / disprove-side-is-sound exercise.
SimChallengeTruth ==
    [t \in SimChallengeTargets |-> IF t = "c1" THEN "prove" ELSE "disprove"]

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

\* Existing Core safety invariants (must still hold in mode-B).
TypeOK == Core!TypeOK
HumanGateMatchesState == Core!HumanGateMatchesState
InFlightKindMatchesStage == Core!InFlightKindMatchesStage
CleanupHasNoBlockers == Core!CleanupHasNoBlockers
NoAdvancePhaseWithBlockers == Core!NoAdvancePhaseWithBlockers
CompleteRequiresChallengeCoverage == Core!CompleteRequiresChallengeCoverage
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

\* PV mode-B prove-or-disprove invariants (forward design) — the deliverable.
ModeBConfiguredTargetsEmpty == Core!ModeBConfiguredTargetsEmpty
SoundnessFloor == Core!SoundnessFloor
NotBothSidesClosed == Core!NotBothSidesClosed
NoHumanInModeB == Core!NoHumanInModeB
UnderModelAssumptionLifecycleContract == Core!UnderModelAssumptionLifecycleContract
CoverageOnLiveOnly == Core!CoverageOnLiveOnly
DecidedResolves == Core!DecidedResolves
AuditSoleFlipper == Core!AuditSoleFlipper
\* Action property: only an audit step may change challengePolarity.  Checked
\* under PROPERTIES (two-state formula); `Core!Vars` is the full tuple.
AuditSoleFlipperProp == [][AuditSoleFlipper]_(Core!Vars)
\* Bait (negation of reachability) — used in the bait cfg, not the green cfg.
DoneNeverComplete == Core!DoneNeverComplete

=============================================================================
