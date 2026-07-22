------------------------- MODULE AddTargetsCoreHarness -------------------------
EXTENDS Integers, FiniteSets, Sequences

(***************************************************************************)
(* Init-at-complete harness for the add-targets disjunct of               *)
(* SupervisorCore!EditConfiguredTargets.                                  *)
(*                                                                        *)
(* Why this exists: the stock SupervisorCoreSim BFS is VACUOUS for the    *)
(* add-targets action — SimTargets == SimInitialConfiguredTargets leaves  *)
(* no unconfigured label to add, and phase = "complete" is not reached    *)
(* inside the stock bounds, so the disjunct never fires there.  This      *)
(* harness starts DIRECTLY in a synthetic quiescent complete-state (one   *)
(* configured+passed target `t1`, one unconfigured label `t2` available,  *)
(* one covering present node `n1`), with Next = the add-targets action    *)
(* plus a follow-on StartCycle, so TLC actually FIRES the action (check   *)
(* the -coverage output: EditConfiguredTargets must be non-zero) and      *)
(* re-checks the stock invariants across the atomic                       *)
(* complete -> theorem_stating flip.  This harness class is what catches  *)
(* successor-not-completely-specified bugs in operator-action disjuncts   *)
(* that the reachable-space harnesses never exercise.                     *)
(*                                                                        *)
(* Run:  java -jar tla2tools.jar AddTargetsCoreHarness.tla \              *)
(*           -config AddTargetsCoreHarness.cfg -coverage 1                *)
(***************************************************************************)

HNodes == {"n1"}
HTargets == {"t1", "t2"}
HChallengeTargets == {}
HNoNode == "NONE"
HMaxCycle == 3
HGoalMode == "prose"

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
        Nodes <- HNodes,
        Targets <- HTargets,
        ChallengeTargets <- HChallengeTargets,
        NoNode <- HNoNode,
        MaxCycle <- HMaxCycle,
        InitialConfiguredTargets <- {"t1"},
        InitialPresentNodes <- {"n1"},
        Backends <- {"lean"},
        TabletTarget <- "lean",
        FormalImports <- {},
        GoalMode <- HGoalMode,
        MaxPolarityFlips <- 2,
        ChallengeTruth <- [t \in HChallengeTargets |-> "prove"],
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

(***************************************************************************)
(* Synthetic quiescent complete-state: the terminal shape ReviewDone      *)
(* leaves behind (stage "Start", nothing in flight, no pending task,      *)
(* every lane Pass, GlobalBlockers = {}), with `t1` configured+covered    *)
(* and `t2` an available unconfigured label.                              *)
(***************************************************************************)
HarnessInit ==
    /\ phase = "complete"
    /\ stage = "Start"
    /\ cycle = 1
    /\ activeNode = HNoNode
    /\ activeCoarseNode = HNoNode
    /\ presentNodes = {"n1"}
    /\ openNodes = {}
    /\ coverage = [t \in HTargets |-> IF t = "t1" THEN {"n1"} ELSE {}]
    /\ approvedCoverage = [t \in HTargets |-> IF t = "t1" THEN {"n1"} ELSE {}]
    /\ challengeCoverage = [t \in HChallengeTargets |-> {}]
    /\ configuredTargets = {"t1"}
    /\ approvedConfiguredTargets = {"t1"}
    /\ coarseDagNodes = {"n1"}
    /\ correspondenceStatus = [n \in HNodes |-> "pass"]
    /\ substantivenessStatus = [n \in HNodes |-> "pass"]
    /\ soundnessStatus = [n \in HNodes |-> "pass"]
    /\ deviationStatus = [n \in HNodes |-> "pass"]
    /\ faithfulnessStatus = [t \in HTargets |-> IF t = "t1" THEN "pass" ELSE "unknown"]
    /\ localClosureStatus = [n \in HNodes |-> "verified"]
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
    /\ hasEverBeenClean = TRUE
    /\ inFlightRequestKind = "none"
    /\ nodeTarget = [n \in HNodes |-> "lean"]
    /\ challengePolarity = [t \in HChallengeTargets |-> "prove"]
    /\ polarityFlips = [t \in HChallengeTargets |-> 0]
    /\ challengeClosedSide = [t \in HChallengeTargets |-> "open"]

(* The add-targets action (the disjunct under test) plus a follow-on      *)
(* StartCycle, pinning that the revived theorem_stating state can take    *)
(* the ordinary next step.  (After the follow-on step the harness         *)
(* deadlocks by design — CHECK_DEADLOCK is FALSE in the .cfg.)            *)
HarnessNext ==
    \/ Core!EditConfiguredTargets
    \/ Core!StartCycle

HarnessSpec == HarnessInit /\ [][HarnessNext]_(Core!Vars)

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
ModeBConfiguredTargetsEmpty == Core!ModeBConfiguredTargetsEmpty
SoundnessFloor == Core!SoundnessFloor
NotBothSidesClosed == Core!NotBothSidesClosed
NoHumanInModeB == Core!NoHumanInModeB
UnderModelAssumptionLifecycleContract == Core!UnderModelAssumptionLifecycleContract
CoverageOnLiveOnly == Core!CoverageOnLiveOnly
DecidedResolves == Core!DecidedResolves

(* Feature pin: approvedConfiguredTargets never moves in this harness —   *)
(* only the AdvancePhase freeze may move it, and that action is not in    *)
(* HarnessNext.  (The in-place theorem_stating edit disjunct can reshape  *)
(* configuredTargets arbitrarily after the revival, so the live set is    *)
(* not pinned here; superset-only growth at complete is enforced by the   *)
(* action's own `added \subseteq (Targets \ configuredTargets)` guard.)   *)
ApprovedConfiguredTargetsPinned ==
    approvedConfiguredTargets = {"t1"}

(* Firing evidence for the uncovered-target orphan window: the complete   *)
(* init state is fully covered (window CLOSED), and the add-targets       *)
(* revival configures `t2` with no covering node, so every post-revival   *)
(* state that still carries `t2` must evaluate the predicate OPEN.  Pins  *)
(* the predicate's variables and polarity on exactly the state class the  *)
(* window feature serves.                                                 *)
WindowMatchesRevivalShape ==
    /\ (phase = "complete") => ~Core!OrphanConstructionWindowOpen
    /\ (phase = "theorem_stating" /\ "t2" \in configuredTargets)
           => Core!OrphanConstructionWindowOpen

=============================================================================
