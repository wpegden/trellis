---------------------- MODULE AddTargetsProtocolHarness ----------------------
EXTENDS SupervisorProtocolSim

(***************************************************************************)
(* Init-at-complete harness for SupervisorProtocol!EnvAddTargetsAtComplete *)
(* (the add-targets disjunct of EnvEditConfiguredTargets).                *)
(*                                                                        *)
(* Why this exists: the stock random simulation rarely (within the       *)
(* configured depth, never) reaches phase = "complete", so the           *)
(* add-targets disjunct is not exercised there — a                        *)
(* successor-not-completely-specified bug in the action would go         *)
(* undetected.  This harness starts DIRECTLY in a synthetic quiescent    *)
(* complete-state (target `t1` configured/covered/passed by node `n1`,   *)
(* `t2` an available unconfigured label, challenge target `c1` decided   *)
(* on its true Disprove side), with Next = the add-targets action plus a *)
(* follow-on StartCycle.  TLC must FIRE the action (check the -coverage  *)
(* output: EnvAddTargetsAtComplete non-zero) and hold the targeted       *)
(* invariants across the atomic complete -> theorem_stating flip.        *)
(*                                                                        *)
(* Reuses the SupervisorProtocolSim instance (EXTENDS) — same constants,  *)
(* same variables; only Init/Next differ.  InitialConfiguredTargets is   *)
(* irrelevant here (HarnessInit assigns configuredTargets directly).     *)
(*                                                                        *)
(* Run:  java -jar tla2tools.jar AddTargetsProtocolHarness.tla \          *)
(*           -config AddTargetsProtocolHarness.cfg -coverage 1            *)
(***************************************************************************)

HPresent == {"Preamble", "n1"}
HClaims == [n \in SimNodes |-> IF n = "n1" THEN {"t1"} ELSE {}]
HCoverage == SP!CoverageFromClaims(HClaims, HPresent, {"t1"})
HPassFp == "fp1"

HarnessInit ==
    /\ phase = "complete"
    /\ stage = "Complete"
    /\ cycle = 1
    /\ attempt = 0
    /\ requestSeq = 0
    /\ invalidAttempt = FALSE
    /\ retryOutcomeKind = "none"
    /\ gateKind = "none"
    /\ gateFromInvalidAttempt = FALSE
    /\ activeNode = "NONE"
    /\ heldTarget = "NONE"
    /\ targetEditMode = "global"
    /\ proofEditMode = "local"
    /\ configuredTargets = {"t1"}
    /\ approvedConfiguredTargets = {"t1"}
    /\ currentNodeKinds = SP!InitialNodeKinds
    /\ committedNodeKinds = SP!InitialNodeKinds
    /\ currentProofNodes = SP!ProofNodesFromKinds(SP!InitialNodeKinds, HPresent)
    /\ committedProofNodes = SP!ProofNodesFromKinds(SP!InitialNodeKinds, HPresent)
    /\ currentDeps = SP!DefaultNodeSetMap
    /\ committedDeps = SP!DefaultNodeSetMap
    /\ currentTargetClaims = HClaims
    /\ committedTargetClaims = HClaims
    /\ presentNodes = HPresent
    /\ committedPresentNodes = HPresent
    /\ openNodes = {}
    /\ committedOpenNodes = {}
    /\ localClosureUnverified = {}
    /\ committedLocalClosureUnverified = {}
    /\ currentCoverage = HCoverage
    /\ committedCoverage = HCoverage
    /\ approvedCoverage = HCoverage
    \* Challenge target c1 decided on its TRUE side (truth = "disprove"):
    \* covered + closed, one audit flip from the default Prove polarity.
    /\ challengeCoverage = [t \in SimChallengeTargets |-> {"n1"}]
    /\ approvedChallengeCoverage = [t \in SimChallengeTargets |-> {"n1"}]
    /\ challengePolarity = [t \in SimChallengeTargets |-> "disprove"]
    /\ polarityFlips = [t \in SimChallengeTargets |-> 1]
    /\ challengeClosedSide =
        [t \in SimChallengeTargets |->
            [SP!InitialChallengeOutcome(t) EXCEPT !.negativeSide = "closed"]]
    /\ paperStatus = [t \in SimTargets |-> IF t = "t1" THEN "pass" ELSE "unknown"]
    /\ paperCurrentFp = [t \in SimTargets |-> IF t = "t1" THEN HPassFp ELSE "fp0"]
    /\ committedPaperCurrentFp = [t \in SimTargets |-> IF t = "t1" THEN HPassFp ELSE "fp0"]
    /\ paperApprovedFp = [t \in SimTargets |-> IF t = "t1" THEN HPassFp ELSE "fp0"]
    /\ substantivenessStatus = [n \in SimNodes |-> "pass"]
    /\ substantivenessCurrentFp = [n \in SimNodes |-> HPassFp]
    /\ committedSubstantivenessCurrentFp = [n \in SimNodes |-> HPassFp]
    /\ substantivenessApprovedFp = [n \in SimNodes |-> HPassFp]
    /\ currentTargetFp = [n \in SimNodes |-> HPassFp]
    /\ committedTargetFp = [n \in SimNodes |-> HPassFp]
    /\ approvedTargetFp = [n \in SimNodes |-> HPassFp]
    /\ coarseDagNodes = {"n1"}
    /\ corrStatus = [n \in SimNodes |-> "pass"]
    /\ corrCurrentFp = [n \in SimNodes |-> HPassFp]
    /\ committedCorrCurrentFp = [n \in SimNodes |-> HPassFp]
    /\ corrApprovedFp = [n \in SimNodes |-> HPassFp]
    /\ soundStatus = [n \in SimNodes |-> "pass"]
    /\ soundCurrentFp = [n \in SimNodes |-> HPassFp]
    /\ committedSoundCurrentFp = [n \in SimNodes |-> HPassFp]
    /\ soundApprovedFp = [n \in SimNodes |-> HPassFp]
    /\ soundAssessmentStatus = SP!DefaultSoundAssessmentStatus
    /\ reviewerRequestedSoundVerifierNodes = {}
    /\ soundReverificationContext = SP!NoSoundReverificationContext
    /\ deviationFiles = SP!DefaultDeviationFiles
    /\ committedDeviationFiles = SP!DefaultDeviationFiles
    /\ deviationStatus = SP!DefaultDeviationStatus
    /\ deviationCurrentFp = SP!DefaultDeviationFp
    /\ committedDeviationCurrentFp = SP!DefaultDeviationFp
    /\ deviationApprovedFp = SP!DefaultDeviationFp
    /\ nodeDeviationClaims = SP!DefaultNodeDeviationClaims
    /\ committedNodeDeviationClaims = SP!DefaultNodeDeviationClaims
    /\ lastCleanDeviationFiles = SP!DefaultDeviationFiles
    /\ lastCleanDeviationStatus = SP!DefaultDeviationStatus
    /\ lastCleanDeviationApprovedFp = SP!DefaultDeviationFp
    /\ lastCleanNodeDeviationClaims = SP!DefaultNodeDeviationClaims
    /\ latestDeviationReviewIds = {}
    /\ latestDeviationEvidenceLanes = {}
    /\ nodeDifficulty = SP!DefaultDifficulty
    /\ easyAttempts = SP!DefaultEasyAttempts
    /\ reviewerComments = ""
    /\ latestPaperEvidenceLanes = {}
    /\ latestCorrEvidenceLanes = {}
    /\ latestSoundEvidenceLanes = {}
    /\ latestPaperReviewTargets = {}
    /\ latestCorrReviewNodes = {}
    /\ latestSoundReviewNodes = {}
    /\ latestPaperPanelSplit = FALSE
    /\ latestCorrPanelSplit = FALSE
    /\ latestSoundPanelSplit = FALSE
    /\ previousPaperFindingLanes = {}
    /\ previousCorrFindingLanes = {}
    /\ previousSoundFindingLanes = {}
    /\ latestSubstantivenessEvidenceLanes = {}
    /\ latestSubstantivenessReviewNodes = {}
    /\ latestSubstantivenessPanelSplit = FALSE
    /\ previousSubstantivenessFindingLanes = {}
    /\ humanInputOutstanding = FALSE
    /\ nativeHistoryKinds = {}
    /\ cyclesSinceClean = 0
    /\ hasEverBeenClean = TRUE
    /\ pendingTask = SP!NoPendingTask
    /\ cleanupAuditTasks = <<>>
    /\ cleanupAuditScratchpad = ""
    /\ cleanupAuditBurstCount = 0
    /\ cleanupAuditRound = 1
    /\ cleanupConsecutiveInvalidWorkers = 0
    /\ cleanupActiveTask = SP!NoTask
    /\ cleanupForceDone = FALSE
    /\ forceReviewAfterConeClean = FALSE
    /\ activeCoarseNode = "NONE"
    /\ cyclesInCoarseRepairMode = 0
    /\ stuckMathAuditActive = FALSE
    /\ stuckMathAuditNeedInputAudit = SP!NoNeedInputAuditContext
    /\ stuckMathAuditBurstRetryCount = 0
    /\ lastStuckMathAuditDispatchedCycle = SP!NoCycle
    /\ stuckMathAuditAssumptionsLaneActive = FALSE
    /\ stagedUnderModelAssumptionDraft = FALSE
    /\ pendingUnderModelAssumptions = 0
    /\ pendingGlobalRepairRequest = SP!NoGlobalRepairRequest
    /\ pendingGlobalRepairGrant = SP!NoGlobalRepairGrant
    /\ latestGlobalRepairAuditDeclineReason = ""
    /\ latestGlobalRepairAuditDeclineCycle = SP!NoCycle
    /\ lastReviewerGlobalRepairRequestCycle = SP!NoCycle
    /\ everShallowCoarseClosed = {"n1"}
    /\ globalRepairModeEnabled = TRUE
    /\ postAdvanceRoutingPending = FALSE
    /\ pendingProtectedReapprovalNodes = {}
    /\ pendingProtectedSemanticScopeConfirmation = SP!NoProtectedSemanticChangeConfirmation
    /\ auditPlan = SP!NoAuditPlan
    /\ supersededAuditPlan = SP!NoAuditPlan
    /\ inFlightRequest = SP!NoRequest
    /\ response = SP!NoResponse

(* The add-targets action plus a follow-on StartCycle.  retryOutcomeKind  *)
(* is assigned the same way the real `Next` assigns it (the global        *)
(* RetryOutcomeKindNext conjunct); EnvAddTargetsAtComplete additionally   *)
(* holds it UNCHANGED internally — consistent, since the complete-stage   *)
(* branch of RetryOutcomeKindNext is the identity.                        *)
HAddTargets ==
    /\ SP!EnvAddTargetsAtComplete
    /\ retryOutcomeKind' = SP!RetryOutcomeKindNext

HStartCycle ==
    /\ SP!StartCycle
    /\ retryOutcomeKind' = SP!RetryOutcomeKindNext

HarnessNext ==
    \/ HAddTargets
    \/ HStartCycle

HarnessSpec == HarnessInit /\ [][HarnessNext]_(SP!Vars)

(* Feature pin: approvedConfiguredTargets never moves in this harness —   *)
(* only the AdvancePhase freeze may move it, and that action is not in    *)
(* HarnessNext.                                                           *)
ApprovedConfiguredTargetsPinned ==
    approvedConfiguredTargets = {"t1"}

(* Firing evidence for the uncovered-target orphan window: the complete   *)
(* init state is fully covered (window CLOSED), and the add-targets       *)
(* revival configures `t2` with no covering node, so every post-revival   *)
(* state must evaluate the predicate OPEN.  Pins the predicate's          *)
(* variables and polarity on exactly the state class the window feature   *)
(* serves (the kernel waives the same-burst orphan clauses there).        *)
WindowMatchesRevivalShape ==
    /\ (phase = "complete") => ~SP!OrphanConstructionWindowOpen
    /\ (phase = "theorem_stating" /\ "t2" \in configuredTargets)
           => SP!OrphanConstructionWindowOpen

=============================================================================
