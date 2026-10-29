--------------------- MODULE ConditionalAdvanceRegression ---------------------
EXTENDS SupervisorProtocolSim, TLC
\* Reuse the real protocol; override the bounded instance's target universe.
BatchTargets == {"c1", "c2"}
LeanGoalMode == "lean"
Originals == {"n1", "n2"}
Present == Originals \cup {"Preamble"}
Kinds == [n \in SimNodes |-> IF n \in Originals THEN "proof" ELSE SP!InitialNodeKinds[n]]
Passed == [n \in SimNodes |-> "pass"]
FailedOriginals == [n \in SimNodes |-> IF n \in Originals THEN "fail" ELSE "pass"]
Fingerprinted == [n \in SimNodes |-> "fp1"]
\* Keep ordinary initial defaults except the stated regression premises.
Seed == INSTANCE SupervisorProtocolSim WITH
    phase <- "source_adaptation", stage <- "Check",
    configuredTargets <- SimInitialConfiguredTargets,
    presentNodes <- SP!InitialPresentNodes, committedPresentNodes <- SP!InitialPresentNodes,
    currentProofNodes <- {}, committedProofNodes <- {},
    currentNodeKinds <- SP!InitialNodeKinds, committedNodeKinds <- SP!InitialNodeKinds,
    openNodes <- {}, committedOpenNodes <- {},
    challengeCoverage <- [t \in SimChallengeTargets |-> {}],
    corrStatus <- SP!InitialCorr, substantivenessStatus <- SP!DefaultSubstantiveness,
    soundStatus <- SP!DefaultSound,
    corrCurrentFp <- SP!DefaultFp, committedCorrCurrentFp <- SP!DefaultFp,
    corrApprovedFp <- SP!DefaultFp,
    substantivenessCurrentFp <- SP!DefaultFp, committedSubstantivenessCurrentFp <- SP!DefaultFp,
    substantivenessApprovedFp <- SP!DefaultFp,
    soundCurrentFp <- SP!DefaultFp, committedSoundCurrentFp <- SP!DefaultFp,
    soundApprovedFp <- SP!DefaultFp
RegressionInit ==
    /\ Seed!SP!Init
    /\ phase = "theorem_stating" /\ stage = "Reviewer" /\ configuredTargets = {}
    /\ presentNodes = Present /\ committedPresentNodes = Present
    /\ currentProofNodes = Originals /\ committedProofNodes = Originals
    /\ currentNodeKinds = Kinds /\ committedNodeKinds = Kinds
    /\ openNodes = Originals /\ committedOpenNodes = Originals
    /\ challengeCoverage = [t \in SimChallengeTargets |-> IF t = "c1" THEN {"n1"} ELSE {"n2"}]
    /\ corrStatus = Passed /\ substantivenessStatus = FailedOriginals /\ soundStatus = Passed
    /\ corrCurrentFp = Fingerprinted /\ committedCorrCurrentFp = Fingerprinted /\ corrApprovedFp = Fingerprinted
    /\ substantivenessCurrentFp = Fingerprinted /\ committedSubstantivenessCurrentFp = Fingerprinted
    /\ substantivenessApprovedFp = Fingerprinted
    /\ soundCurrentFp = Fingerprinted /\ committedSoundCurrentFp = Fingerprinted /\ soundApprovedFp = Fingerprinted
AllSealed == \A t \in SimChallengeTargets :
    /\ challengeClosedSide[t].conditionalStage = "correspondence_pass"
    /\ ~challengeClosedSide[t].conditionalRejected
Approved == \A t \in SimChallengeTargets : challengeClosedSide[t].conditionalStage = "human_approved"
Rejected == \A t \in SimChallengeTargets : challengeClosedSide[t].conditionalRejected
Terminal == Approved \/ Rejected
\* Environment writes only the response; all protocol transitions stay in SP.
ResponseFrame == <<
    phase, stage, cycle, attempt, requestSeq,
    invalidAttempt, retryOutcomeKind, gateKind, gateFromInvalidAttempt, activeNode,
    heldTarget, targetEditMode, proofEditMode, configuredTargets, approvedConfiguredTargets,
    currentProofNodes, committedProofNodes, currentNodeKinds, committedNodeKinds, currentDeps,
    committedDeps, currentTargetClaims, committedTargetClaims, presentNodes, committedPresentNodes,
    openNodes, committedOpenNodes, localClosureUnverified, committedLocalClosureUnverified, currentCoverage,
    committedCoverage, approvedCoverage, challengeCoverage, approvedChallengeCoverage, challengePolarity,
    polarityFlips, challengeClosedSide, paperStatus, paperCurrentFp, committedPaperCurrentFp,
    paperApprovedFp, substantivenessStatus, substantivenessCurrentFp, committedSubstantivenessCurrentFp, substantivenessApprovedFp,
    currentTargetFp, committedTargetFp, approvedTargetFp, coarseDagNodes, corrStatus,
    corrCurrentFp, committedCorrCurrentFp, corrApprovedFp, soundStatus, soundCurrentFp,
    committedSoundCurrentFp, soundApprovedFp, deviationFiles, committedDeviationFiles, deviationStatus,
    deviationCurrentFp, committedDeviationCurrentFp, deviationApprovedFp, nodeDeviationClaims, committedNodeDeviationClaims,
    lastCleanDeviationFiles, lastCleanDeviationStatus, lastCleanDeviationApprovedFp, lastCleanNodeDeviationClaims, latestDeviationReviewIds,
    latestDeviationEvidenceLanes, nodeDifficulty, easyAttempts, reviewerComments, latestPaperEvidenceLanes,
    latestCorrEvidenceLanes, latestSoundEvidenceLanes, latestPaperPanelSplit, latestCorrPanelSplit, latestSoundPanelSplit,
    latestPaperReviewTargets, latestCorrReviewNodes, latestSoundReviewNodes, previousPaperFindingLanes, previousCorrFindingLanes,
    previousSoundFindingLanes, latestSubstantivenessEvidenceLanes, latestSubstantivenessReviewNodes, latestSubstantivenessPanelSplit, previousSubstantivenessFindingLanes,
    humanInputOutstanding, nativeHistoryKinds, cyclesSinceClean, hasEverBeenClean, pendingTask,
    cleanupAuditTasks, cleanupAuditScratchpad, cleanupAuditBurstCount, cleanupAuditRound, cleanupConsecutiveInvalidWorkers,
    cleanupActiveTask, cleanupForceDone, forceReviewAfterConeClean, activeCoarseNode, cyclesInCoarseRepairMode,
    stuckMathAuditActive, stuckMathAuditNeedInputAudit, stuckMathAuditBurstRetryCount, lastStuckMathAuditDispatchedCycle, stuckMathAuditAssumptionsLaneActive,
    stagedUnderModelAssumptionDraft, pendingUnderModelAssumptions, pendingGlobalRepairRequest, pendingGlobalRepairGrant, latestGlobalRepairAuditDeclineReason,
    latestGlobalRepairAuditDeclineCycle, lastReviewerGlobalRepairRequestCycle, everShallowCoarseClosed, globalRepairModeEnabled, postAdvanceRoutingPending,
    pendingProtectedReapprovalNodes, pendingProtectedSemanticScopeConfirmation, auditPlan, supersededAuditPlan, soundAssessmentStatus,
    reviewerRequestedSoundVerifierNodes, soundReverificationContext, inFlightRequest, phase0State
>>
AdvanceResponse ==
    /\ stage = "Reviewer" /\ inFlightRequest.kind = "review"
    /\ response = SP!NoResponse
    /\ response' = [SP!NoResponse EXCEPT !.kind = "review", !.status = "ok",
          !.cycle = cycle, !.decision = "ADVANCE_PHASE", !.nextMode = "global",
          !.allowNewObligations = TRUE]
    /\ UNCHANGED ResponseFrame

RegressionCoreNext ==
    \/ SP!ProposeChallengeConditional
    \/ SP!PassChallengeConditionalCorrespondence
    \/ /\ AllSealed /\ SP!IssueReviewRequest
    \/ /\ AllSealed /\ AdvanceResponse
    \/ SP!ReviewAdvancePhase
    \/ SP!IssueHumanGateRequest
    \/ SP!EnvStageHumanApprove
    \/ SP!EnvStageHumanFeedback
    \/ SP!HumanApproveAdvance
    \/ SP!HumanFeedbackAfterAdvance
    \/ /\ Terminal /\ UNCHANGED <<ResponseFrame, response>>
RegressionNext ==
    /\ RegressionCoreNext
    /\ UNCHANGED phase0State
    /\ retryOutcomeKind' = SP!RetryOutcomeKindNext

OriginalFailuresPreserved ==
    /\ openNodes = Originals /\ substantivenessStatus = FailedOriginals
    /\ \A t \in SimChallengeTargets :
         /\ challengeClosedSide[t].positiveSide = "open"
         /\ challengeClosedSide[t].negativeSide = "open"
ApprovalReachableWithoutProof == AllSealed => SP!GlobalBlockers = {}
ApprovalIsNotCompletion == ~SP!FormalizationComplete
FeedbackRestoresBlockers == Rejected => SP!GlobalBlockers # {}
RegressionSpec == RegressionInit /\ [][RegressionNext]_SP!Vars /\ WF_SP!Vars(RegressionNext)
EventuallyDecided == <>Terminal
=============================================================================
