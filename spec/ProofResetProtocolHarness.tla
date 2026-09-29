-------------------- MODULE ProofResetProtocolHarness --------------------
EXTENDS SupervisorProtocolSim

(***************************************************************************)
(* Init-at-reviewer harness for the proof-formalization substantiveness    *)
(* reset (SupervisorProtocol!RequestAllowedResetBlockers, proof arm).      *)
(*                                                                        *)
(* Why this exists: the stock random simulation does not reach            *)
(* phase = "proof_formalization" with a live substantiveness Fail inside  *)
(* the configured trace budget, so widening EnvStageReviewArtifact's      *)
(* reset-blocker generation is not enough on its own to exercise the      *)
(* transition — same situation as AddTargetsProtocolHarness. This harness *)
(* starts DIRECTLY at a proof-formalization Reviewer boundary carrying    *)
(* two substantiveness Fails of different provenance:                     *)
(*                                                                        *)
(*   n1 — fingerprint-backed: substantivenessStatus = "fail" with         *)
(*        current = approved. The reset moves it, so it IS offered.       *)
(*   n2 — derived: status "pass", but it claims deviation `dev_a` whose   *)
(*        own verdict is Fail, so CurrentSubstantivenessState             *)
(*        short-circuits to "fail" ahead of the status/fingerprint arm.   *)
(*        The reset cannot move it, so it is NOT offered.                 *)
(*                                                                        *)
(* Next issues the review, stages one pinned reviewer response naming n1's *)
(* blocker in `resetBlockers`, and applies it through ReviewContinueProof  *)
(* — whose `ReviewDecisionLegal` gate carries the load-bearing clause      *)
(* `resetBlockers \subseteq RequestAllowedResetBlockers`. TLC must FIRE    *)
(* both (check -coverage: HStageSubstantivenessReset and HApplyProof       *)
(* non-zero); a proof-phase offer that excluded the reset would leave      *)
(* HApplyProof at zero.                                                   *)
(*                                                                        *)
(* Run:  java -jar tla2tools.jar ProofResetProtocolHarness.tla \          *)
(*           -config ProofResetProtocolHarness.cfg -coverage 1            *)
(***************************************************************************)

HPresent == {"Preamble", "n1", "n2"}
HClaims == [n \in SimNodes |-> IF n = "n1" THEN {"t1"} ELSE {}]
HCoverage == SP!CoverageFromClaims(HClaims, HPresent, {"t1"})
HPassFp == "fp1"

\* Deviation `dev_a` exists and its own lane verdict is a definite Fail
\* (status "fail" with a non-default current = approved fingerprint); n2
\* claims it. `dev_b` is absent.
HDeviationFiles == [id \in SimDeviations |-> id = "dev_a"]
HDeviationStatus == [id \in SimDeviations |-> IF id = "dev_a" THEN "fail" ELSE "unknown"]
HDeviationFp == [id \in SimDeviations |-> IF id = "dev_a" THEN HPassFp ELSE "fp0"]
HClaimsMap == [n \in SimNodes |-> IF n = "n2" THEN {"dev_a"} ELSE {}]

HarnessInit ==
    /\ phase = "proof_formalization"
    /\ stage = "Reviewer"
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
    /\ openNodes = {"n1", "n2"}
    /\ committedOpenNodes = {"n1", "n2"}
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
    /\ substantivenessStatus = [n \in SimNodes |-> IF n = "n1" THEN "fail" ELSE "pass"]
    /\ substantivenessCurrentFp = [n \in SimNodes |-> HPassFp]
    /\ committedSubstantivenessCurrentFp = [n \in SimNodes |-> HPassFp]
    /\ substantivenessApprovedFp = [n \in SimNodes |-> HPassFp]
    /\ currentTargetFp = [n \in SimNodes |-> HPassFp]
    /\ committedTargetFp = [n \in SimNodes |-> HPassFp]
    /\ approvedTargetFp = [n \in SimNodes |-> HPassFp]
    /\ coarseDagNodes = {}
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
    /\ deviationFiles = HDeviationFiles
    /\ committedDeviationFiles = HDeviationFiles
    /\ deviationStatus = HDeviationStatus
    /\ deviationCurrentFp = HDeviationFp
    /\ committedDeviationCurrentFp = HDeviationFp
    /\ deviationApprovedFp = HDeviationFp
    /\ nodeDeviationClaims = HClaimsMap
    /\ committedNodeDeviationClaims = HClaimsMap
    /\ lastCleanDeviationFiles = HDeviationFiles
    /\ lastCleanDeviationStatus = HDeviationStatus
    /\ lastCleanDeviationApprovedFp = HDeviationFp
    /\ lastCleanNodeDeviationClaims = HClaimsMap
    /\ latestDeviationReviewIds = {"dev_a"}
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
    /\ latestSubstantivenessReviewNodes = {"n1", "n2"}
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
    /\ everShallowCoarseClosed = {}
    /\ globalRepairModeEnabled = TRUE
    /\ postAdvanceRoutingPending = FALSE
    /\ pendingProtectedReapprovalNodes = {}
    /\ pendingProtectedSemanticScopeConfirmation = SP!NoProtectedSemanticChangeConfirmation
    /\ auditPlan = SP!NoAuditPlan
    /\ supersededAuditPlan = SP!NoAuditPlan
    /\ inFlightRequest = SP!NoRequest
    /\ response = SP!NoResponse

HIssueReview ==
    /\ SP!IssueReviewRequest
    /\ retryOutcomeKind' = SP!RetryOutcomeKindNext

\* Every variable the staging action leaves alone.
HVarsWithoutResponse ==
    <<
        phase,
        stage,
        cycle,
        attempt,
        requestSeq,
        invalidAttempt,
        retryOutcomeKind,
        gateKind,
        gateFromInvalidAttempt,
        activeNode,
        heldTarget,
        targetEditMode,
        proofEditMode,
        configuredTargets,
        approvedConfiguredTargets,
        currentProofNodes,
        committedProofNodes,
        currentNodeKinds,
        committedNodeKinds,
        currentDeps,
        committedDeps,
        currentTargetClaims,
        committedTargetClaims,
        presentNodes,
        committedPresentNodes,
        openNodes,
        committedOpenNodes,
        localClosureUnverified,
        committedLocalClosureUnverified,
        currentCoverage,
        committedCoverage,
        approvedCoverage,
        challengeCoverage,
        approvedChallengeCoverage,
        challengePolarity,
        polarityFlips,
        challengeClosedSide,
        paperStatus,
        paperCurrentFp,
        committedPaperCurrentFp,
        paperApprovedFp,
        substantivenessStatus,
        substantivenessCurrentFp,
        committedSubstantivenessCurrentFp,
        substantivenessApprovedFp,
        currentTargetFp,
        committedTargetFp,
        approvedTargetFp,
        coarseDagNodes,
        corrStatus,
        corrCurrentFp,
        committedCorrCurrentFp,
        corrApprovedFp,
        soundStatus,
        soundCurrentFp,
        committedSoundCurrentFp,
        soundApprovedFp,
        deviationFiles,
        committedDeviationFiles,
        deviationStatus,
        deviationCurrentFp,
        committedDeviationCurrentFp,
        deviationApprovedFp,
        nodeDeviationClaims,
        committedNodeDeviationClaims,
        lastCleanDeviationFiles,
        lastCleanDeviationStatus,
        lastCleanDeviationApprovedFp,
        lastCleanNodeDeviationClaims,
        latestDeviationReviewIds,
        latestDeviationEvidenceLanes,
        nodeDifficulty,
        easyAttempts,
        reviewerComments,
        latestPaperEvidenceLanes,
        latestCorrEvidenceLanes,
        latestSoundEvidenceLanes,
        latestPaperPanelSplit,
        latestCorrPanelSplit,
        latestSoundPanelSplit,
        latestPaperReviewTargets,
        latestCorrReviewNodes,
        latestSoundReviewNodes,
        previousPaperFindingLanes,
        previousCorrFindingLanes,
        previousSoundFindingLanes,
        latestSubstantivenessEvidenceLanes,
        latestSubstantivenessReviewNodes,
        latestSubstantivenessPanelSplit,
        previousSubstantivenessFindingLanes,
        humanInputOutstanding,
        nativeHistoryKinds,
        cyclesSinceClean,
        hasEverBeenClean,
        pendingTask,
        cleanupAuditTasks,
        cleanupAuditScratchpad,
        cleanupAuditBurstCount,
        cleanupAuditRound,
        cleanupConsecutiveInvalidWorkers,
        cleanupActiveTask,
        cleanupForceDone,
        forceReviewAfterConeClean,
        activeCoarseNode,
        cyclesInCoarseRepairMode,
        stuckMathAuditActive,
        stuckMathAuditNeedInputAudit,
        stuckMathAuditBurstRetryCount,
        lastStuckMathAuditDispatchedCycle,
        stuckMathAuditAssumptionsLaneActive,
        stagedUnderModelAssumptionDraft,
        pendingUnderModelAssumptions,
        pendingGlobalRepairRequest,
        pendingGlobalRepairGrant,
        latestGlobalRepairAuditDeclineReason,
        latestGlobalRepairAuditDeclineCycle,
        lastReviewerGlobalRepairRequestCycle,
        everShallowCoarseClosed,
        globalRepairModeEnabled,
        postAdvanceRoutingPending,
        pendingProtectedReapprovalNodes,
        pendingProtectedSemanticScopeConfirmation,
        auditPlan,
        supersededAuditPlan,
        soundAssessmentStatus,
        reviewerRequestedSoundVerifierNodes,
        soundReverificationContext,
        inFlightRequest
    >>

\* The reviewer artifact under test: a proof-formalization Continue whose
\* only blocker action is a reset of n1's substantiveness blocker.
\*
\* Staged directly rather than through `EnvStageReviewArtifact`, whose
\* successor set is far too large for TLC to enumerate from a state with
\* live blockers. What the transition needs from the env is only that some
\* response of this shape be generable; what decides whether the reset is
\* admitted is `ReviewDecisionLegal`, which `ReviewContinueProof` applies
\* below — with `resetBlockers \subseteq RequestAllowedResetBlockers`
\* as the load-bearing clause.
HResetBlocker ==
    SP!Blocker("substantiveness", SP!NodeObject("n1"), HPassFp)

\* The two blockers the reset does not touch. Proof-phase legality requires
\* the reviewer's buckets to cover GlobalBlockers, so n2's derived
\* substantiveness Fail and the rejected deviation it claims are routed to
\* the worker — which is the whole point of D1: those are worker repairs,
\* not reset candidates.
HTaskBlockers ==
    {SP!Blocker("substantiveness", SP!NodeObject("n2"), HPassFp),
     SP!Blocker("deviation", SP!DeviationObject("dev_a"), HPassFp)}

HStageSubstantivenessReset ==
    /\ stage = "Reviewer"
    /\ inFlightRequest.kind = "review"
    /\ inFlightRequest.cycle = cycle
    /\ response = SP!NoResponse
    /\ response' =
        [
            SP!NoResponse EXCEPT
                !.status = "ok",
                !.kind = "review",
                !.cycle = cycle,
                !.decision = "CONTINUE",
                !.resetBlockers = {HResetBlocker},
                !.taskBlockers = HTaskBlockers,
                !.nextActive = "n2",
                !.nextMode = "restructure",
                !.authorizedNodes = {"n2"}
        ]
    /\ UNCHANGED HVarsWithoutResponse

HApplyProof ==
    /\ SP!ReviewContinueProof
    /\ retryOutcomeKind' = SP!RetryOutcomeKindNext

HarnessNext ==
    \/ HIssueReview
    \/ HStageSubstantivenessReset
    \/ HApplyProof

HarnessSpec == HarnessInit /\ [][HarnessNext]_(SP!Vars)

(* D1: the proof-phase offer names only substantiveness Fails a reset can *)
(* move. A Fail derived ahead of the status/fingerprint arm survives the  *)
(* reset, so offering it spends a reviewer action that cannot work.       *)
ProofResetOfferIsMovable ==
    (phase = "proof_formalization") =>
        \A b \in SP!RequestAllowedResetBlockers("review") :
            /\ b.kind = "substantiveness"
            /\ ~SP!IsDerivedSubstantivenessFail(b)
            /\ ~SP!NodeHasFailedDeviationClaim(b.object.node)

(* The reset re-queues rather than clears: post-apply, n1 is back on the  *)
(* substantiveness verifier frontier and still carries its blocker.       *)
ResetRequeuesRatherThanClears ==
    (substantivenessStatus["n1"] = "unknown") =>
        /\ "n1" \in SP!SubstantivenessVerifyNodes
        /\ ~SP!CurrentSubstantivenessPass("n1")

(* n2's derived Fail is untouched by anything the reviewer can do here.   *)
DerivedFailUnmoved ==
    SP!CurrentSubstantivenessState("n2") = "fail"

=============================================================================
