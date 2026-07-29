------------------------- MODULE SidecarCoreHarness -------------------------
EXTENDS Integers, FiniteSets, Sequences

(***************************************************************************)
(* Init-at-enabled harness for SupervisorCore!ApplySidecarClosure.        *)
(*                                                                        *)
(* Why this exists: the stock SupervisorCoreSim BFS is VACUOUS for the    *)
(* sidecar-close action — SimNodes = {"n1"} and the action excludes the   *)
(* active node, so with a single node the enabling conjunction is never   *)
(* satisfiable inside the stock bounds (the AddTargets harness precedent  *)
(* class).  This harness starts DIRECTLY in a synthetic quiescent         *)
(* ProofFormalization boundary state (stage "Start", nothing in flight,   *)
(* one OPEN node `n1` with corr+subst Pass, activeNode = NONE), with      *)
(* Next = ApplySidecarClosure plus a follow-on StartCycle, so TLC         *)
(* actually FIRES the action and re-checks the stock invariants across    *)
(* the close (openNodes shrink + record install + counter bump in one     *)
(* transition; the MaxSidecarCloses = 1 bound then disables re-firing).   *)
(*                                                                        *)
(* Run:  java -jar tla2tools.jar SidecarCoreHarness.tla \                 *)
(*           -config SidecarCoreHarness.cfg -coverage 1                   *)
(***************************************************************************)

HNodes == {"n1"}
HTargets == {"t1", "t2"}
HChallengeTargets == {}
HNoNode == "NONE"
HMaxCycle == 3
HGoalMode == "prose"

VARIABLES
    hEverQueued,  \* harness-only history: n1 has been in the queue
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
        MaxSidecarCloses <- 1,
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
        challengeClosedSide <- challengeClosedSide,
        sidecarCloses <- sidecarCloses,
        sidecarQueue <- sidecarQueue

(***************************************************************************)
(* Synthetic ProofFormalization boundary state: stage "Start", nothing   *)
(* in flight, no pending task, the single node `n1` OPEN with corr+subst *)
(* Pass (sound still unknown — the tier-2 shape), target `t1` covered.   *)
(***************************************************************************)
HarnessInit ==
    /\ phase = "proof_formalization"
    \* Two entry shapes: the quiescent boundary (stage "Start" — the
    \* close under test fires from here) and a live Reviewer turn
    \* (stage "Reviewer" — Core!ReviewContinue fires from here, giving
    \* LIVE coverage of the queue add/remove conjuncts, which the stock
    \* Sim never reaches: no reachable Sim Reviewer state has an open
    \* node with both statement lanes Pass).
    /\ \/ (stage = "Start"    /\ inFlightRequestKind = "none")
       \/ (stage = "Reviewer" /\ inFlightRequestKind = "reviewer")
    /\ cycle = 1
    /\ activeNode = HNoNode
    /\ activeCoarseNode = HNoNode
    /\ presentNodes = {"n1"}
    /\ openNodes = {"n1"}
    /\ coverage = [t \in HTargets |-> IF t = "t1" THEN {"n1"} ELSE {}]
    /\ approvedCoverage = [t \in HTargets |-> IF t = "t1" THEN {"n1"} ELSE {}]
    /\ challengeCoverage = [t \in HChallengeTargets |-> {}]
    /\ configuredTargets = {"t1"}
    /\ approvedConfiguredTargets = {"t1"}
    /\ coarseDagNodes = {"n1"}
    /\ correspondenceStatus = [n \in HNodes |-> "pass"]
    /\ substantivenessStatus = [n \in HNodes |-> "pass"]
    /\ soundnessStatus = [n \in HNodes |-> "unknown"]
       \* Sound not yet passed: the sidecar tier-2 close is the harder case
       \* (closing retires the sound frontier for the node).
    /\ deviationStatus = [n \in HNodes |-> "pass"]
    /\ faithfulnessStatus = [t \in HTargets |-> IF t = "t1" THEN "pass" ELSE "unknown"]
    /\ localClosureStatus = [n \in HNodes |-> "unverified"]
       \* The open node has no closure record yet; ApplySidecarClosure
       \* must install it ("verified") in the SAME transition (C-3).
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
    /\ nodeTarget = [n \in HNodes |-> "lean"]
    /\ challengePolarity = [t \in HChallengeTargets |-> "prove"]
    /\ polarityFlips = [t \in HChallengeTargets |-> 0]
    /\ challengeClosedSide = [t \in HChallengeTargets |-> "open"]
    /\ sidecarCloses = 0
    \* Queue redesign: three init branches — Start+queued (the close
    \* can fire), Start+unqueued (the close is DISABLED by the
    \* membership conjunct; only StartCycle can move), and
    \* Reviewer+unqueued (ReviewContinue can ADD live).  The history
    \* aux var (updated by the ReviewContinue frame below) lets the
    \* invariant prove the counterfactual: drop the membership conjunct
    \* from ApplySidecarClosure and TLC fires the close on a
    \* never-queued run, violating SidecarCloseOnlyAfterQueued.
    /\ sidecarQueue \in IF stage = "Start" THEN {{}, {"n1"}} ELSE {{}}
    /\ hEverQueued = (sidecarQueue = {"n1"})

(* The sidecar-close action (the disjunct under test) plus a follow-on    *)
(* StartCycle, pinning that the post-close boundary state can take the    *)
(* ordinary next step (incl. the formalization-complete auto-advance      *)
(* shape).  (The harness eventually deadlocks by design — CHECK_DEADLOCK  *)
(* is FALSE in the .cfg.)                                                 *)
HarnessNext ==
    \* The action under test carries its own sidecar frames (counter +
    \* queue consume); StartCycle (a legacy action) is framed here;
    \* ReviewContinue assigns sidecarQueue' itself (reviewer
    \* add/remove) and records the history bit.
    \/ (Core!SidecarClose /\ UNCHANGED hEverQueued)
    \* The spent-generation expiry: same boundary, same queue, but it
    \* must NOT close the node or install a record (the pin below).
    \/ (Core!SidecarExpire /\ UNCHANGED hEverQueued)
    \/ (Core!StartCycle /\ UNCHANGED <<sidecarCloses, sidecarQueue,
                                       hEverQueued>>)
    \/ (Core!ReviewContinue
        /\ UNCHANGED sidecarCloses
        /\ hEverQueued' = (hEverQueued \/ sidecarQueue' # {}))

HarnessSpec == HarnessInit /\ [][HarnessNext]_(<<Core!Vars, hEverQueued>>)

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

(* Feature pins.                                                         *)
(* C-3 tier coverage across the atomic close: firing the action closes   *)
(* the node AND installs its record in the same transition — no state    *)
(* where the node is closed but the closure tier still says unverified.  *)
(* Queue redesign: the close also CONSUMES the queue entry in the same   *)
(* transition.                                                           *)
SidecarCloseInstallsRecord ==
    (sidecarCloses = 1) =>
        /\ openNodes = {}
        /\ localClosureStatus["n1"] = "verified"
        /\ sidecarQueue = {}

(* The bound is live: the counter never exceeds MaxSidecarCloses (= 1    *)
(* here), i.e. the action disables itself after the bound.               *)
SidecarCounterBounded ==
    sidecarCloses \in 0..1

(* Queue authority (queue redesign): the close can ONLY fire on a run    *)
(* where n1 has been queued (at init, or by a live ReviewContinue add)   *)
(* — with the membership conjunct removed, a never-queued run fires the  *)
(* close and violates this.                                              *)
SidecarCloseOnlyAfterQueued ==
    (sidecarCloses = 1) => hEverQueued

(* Expiry retires the QUEUE ENTRY and nothing else.  In this harness the *)
(* only action that may close `n1` or install its record is              *)
(* `ApplySidecarClosure` (StartCycle and ReviewContinue cannot), and it   *)
(* bumps the counter — so "counter still 0" must imply "node still open,  *)
(* record still unverified" in every reachable state.  Give                *)
(* ExpireSidecarQueueEntry an `openNodes'` or `localClosureStatus'`       *)
(* conjunct by mistake and TLC lands here immediately: the queue can      *)
(* empty without a close, but the node cannot.                            *)
ExpireNeitherClosesNorInstalls ==
    (sidecarCloses = 0) =>
        /\ openNodes = {"n1"}
        /\ localClosureStatus["n1"] = "unverified"

SidecarQueueSubsetOpen == Core!SidecarQueueSubsetOpen

=============================================================================
