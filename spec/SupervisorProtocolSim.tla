------------------------------ MODULE SupervisorProtocolSim ------------------------------
SimNodes == {"Preamble", "n1", "n2", "n3", "n4", "n5", "n6"}
SimTargets == {"t1", "t2"}
SimVerifierLanes == {"v1", "v2"}
SimFingerprints == {"fp0", "fp1", "fp2", "fp3"}
SimPreambleItemIds == {"Preamble[1]"}
SimProofNodes == {"n1", "n2", "n3", "n4"}
SimNodeKinds ==
    [n \in SimNodes |->
        IF n = "Preamble" THEN
            "preamble"
        ELSE IF n \in {"n1", "n2", "n3", "n4"} THEN
            "proof"
        ELSE
            "definition"
    ]
SimDeps ==
    [n \in SimNodes |->
        IF n = "Preamble" THEN
            {}
        ELSE IF n = "n1" THEN
            {"Preamble"}
        ELSE IF n = "n2" THEN
            {"Preamble", "n1"}
        ELSE IF n = "n3" THEN
            {"Preamble", "n1"}
        ELSE IF n = "n4" THEN
            {"Preamble", "n2", "n3"}
        ELSE IF n = "n5" THEN
            {"Preamble", "n2"}
        ELSE
            {"Preamble", "n4", "n5"}
    ]
SimNodeRank ==
    [n \in SimNodes |->
        IF n = "Preamble" THEN
            0
        ELSE IF n = "n1" THEN
            1
        ELSE IF n \in {"n2", "n3"} THEN
            2
        ELSE IF n \in {"n4", "n5"} THEN
            3
        ELSE
            4
    ]
SimNodeOrder ==
    [n \in SimNodes |->
        IF n = "Preamble" THEN
            0
        ELSE IF n = "n1" THEN
            1
        ELSE IF n = "n2" THEN
            2
        ELSE IF n = "n3" THEN
            3
        ELSE IF n = "n4" THEN
            4
        ELSE IF n = "n5" THEN
            5
        ELSE
            6
    ]
SimVerifierBindings == {"corrA", "corrB", "soundA", "soundB"}
SimCorrVerifierBindingByLane ==
    [lane \in SimVerifierLanes |->
        IF lane = "v1" THEN "corrA" ELSE "corrB"
    ]
SimSoundVerifierBindingByLane ==
    [lane \in SimVerifierLanes |->
        IF lane = "v1" THEN "soundA" ELSE "soundB"
    ]
SimWorkerBindingByProfile ==
    [profile \in {"none", "theorem", "proof_easy", "proof_hard", "cleanup", "final_cleanup"} |->
        IF profile = "proof_easy" THEN
            "soundA"
        ELSE IF profile \in {"proof_hard", "cleanup", "final_cleanup"} THEN
            "soundB"
        ELSE
            "corrA"
    ]
SimReviewerBinding == "corrB"
SimInitialConfiguredTargets == SimTargets
\* Two abstract deviation ids — enough to exercise multi-deviation
\* interactions (claim cross-contamination, deletion of one while
\* another is in flight, etc.) while keeping the state space small.
SimDeviations == {"dev_a", "dev_b"}
SimNoDeviation == "NO_DEVIATION"
\* Match the kernel default (commit 2c59943): reviewer Pass override
\* is OFF. Flip to TRUE to model the opt-in trace.
SimAllowReviewerPassOverride == FALSE

VARIABLES
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
    \* Deviation lane state (2026-05-27/28).
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
    \* Cleanup-v2 (2026-05-14) — see SupervisorProtocol.tla.
    cleanupAuditTasks,
    cleanupAuditScratchpad,
    cleanupAuditBurstCount,
    cleanupAuditRound,
    cleanupConsecutiveInvalidWorkers,
    cleanupActiveTask,
    cleanupForceDone,
    \* Cone-clean (2026-05-19) — see SupervisorProtocol.tla.
    forceReviewAfterConeClean,
    \* Active-coarse-anchor (proposal v32) — see SupervisorProtocol.tla.
    activeCoarseNode,
    cyclesInCoarseRepairMode,
    \* StuckMathAudit producer (2026-05-31) — see SupervisorProtocol.tla.
    stuckMathAuditActive,
    stuckMathAuditNeedInputAudit,
    stuckMathAuditBurstRetryCount,
    lastStuckMathAuditDispatchedCycle,
    \* global_repair_mode cluster (2026-06-05) — see SupervisorProtocol.tla.
    pendingGlobalRepairRequest,
    pendingGlobalRepairGrant,
    latestGlobalRepairAuditDeclineReason,
    latestGlobalRepairAuditDeclineCycle,
    lastReviewerGlobalRepairRequestCycle,
    everShallowCoarseClosed,
    globalRepairModeEnabled,
    \* Post-advance routing latch + protected-target reapproval +
    \* audit-plan lane (2026-06-05) — see SupervisorProtocol.tla.
    postAdvanceRoutingPending,
    pendingProtectedReapprovalNodes,
    pendingProtectedSemanticScopeConfirmation,
    auditPlan,
    supersededAuditPlan,
    \* Sound assessment taxonomy + reverification context
    \* (2026-06-05) — see SupervisorProtocol.tla.
    soundAssessmentStatus,
    reviewerRequestedSoundVerifierNodes,
    soundReverificationContext,
    inFlightRequest,
    response

SP == INSTANCE SupervisorProtocol
    WITH
        Nodes <- SimNodes,
        Targets <- SimTargets,
        VerifierLanes <- SimVerifierLanes,
        Fingerprints <- SimFingerprints,
        PreambleItemIds <- SimPreambleItemIds,
        NodeKinds <- SimNodeKinds,
        ProofNodes <- SimProofNodes,
        Deps <- SimDeps,
        NodeRank <- SimNodeRank,
        NodeOrder <- SimNodeOrder,
        VerifierBindings <- SimVerifierBindings,
        CorrVerifierBindingByLane <- SimCorrVerifierBindingByLane,
        SoundVerifierBindingByLane <- SimSoundVerifierBindingByLane,
        WorkerBindingByProfile <- SimWorkerBindingByProfile,
        ReviewerBinding <- SimReviewerBinding,
        NoNode <- "NONE",
        NoFingerprint <- "fp0",
        NoCheckpoint <- "NO_CHECKPOINT",
        MaxCycle <- 6,
        MaxAttempt <- 3,
        ProofInvalidReviewThreshold <- 2,
        EasyMaxRetries <- 2,
        StuckCoarseRepairThreshold <- 3,
        InitialConfiguredTargets <- SimInitialConfiguredTargets,
        Deviations <- SimDeviations,
        NoDeviation <- SimNoDeviation,
        AllowReviewerPassOverride <- SimAllowReviewerPassOverride,
        phase <- phase,
        stage <- stage,
        cycle <- cycle,
        attempt <- attempt,
        requestSeq <- requestSeq,
        invalidAttempt <- invalidAttempt,
        gateKind <- gateKind,
        gateFromInvalidAttempt <- gateFromInvalidAttempt,
        activeNode <- activeNode,
        heldTarget <- heldTarget,
        targetEditMode <- targetEditMode,
        proofEditMode <- proofEditMode,
        configuredTargets <- configuredTargets,
        approvedConfiguredTargets <- approvedConfiguredTargets,
        currentProofNodes <- currentProofNodes,
        committedProofNodes <- committedProofNodes,
        currentNodeKinds <- currentNodeKinds,
        committedNodeKinds <- committedNodeKinds,
        currentDeps <- currentDeps,
        committedDeps <- committedDeps,
        currentTargetClaims <- currentTargetClaims,
        committedTargetClaims <- committedTargetClaims,
        presentNodes <- presentNodes,
        committedPresentNodes <- committedPresentNodes,
        openNodes <- openNodes,
        committedOpenNodes <- committedOpenNodes,
        localClosureUnverified <- localClosureUnverified,
        committedLocalClosureUnverified <- committedLocalClosureUnverified,
        currentCoverage <- currentCoverage,
        committedCoverage <- committedCoverage,
        approvedCoverage <- approvedCoverage,
        paperStatus <- paperStatus,
        paperCurrentFp <- paperCurrentFp,
        committedPaperCurrentFp <- committedPaperCurrentFp,
        paperApprovedFp <- paperApprovedFp,
        substantivenessStatus <- substantivenessStatus,
        substantivenessCurrentFp <- substantivenessCurrentFp,
        committedSubstantivenessCurrentFp <- committedSubstantivenessCurrentFp,
        substantivenessApprovedFp <- substantivenessApprovedFp,
        currentTargetFp <- currentTargetFp,
        committedTargetFp <- committedTargetFp,
        approvedTargetFp <- approvedTargetFp,
        coarseDagNodes <- coarseDagNodes,
        corrStatus <- corrStatus,
        corrCurrentFp <- corrCurrentFp,
        committedCorrCurrentFp <- committedCorrCurrentFp,
        corrApprovedFp <- corrApprovedFp,
        soundStatus <- soundStatus,
        soundCurrentFp <- soundCurrentFp,
        committedSoundCurrentFp <- committedSoundCurrentFp,
        soundApprovedFp <- soundApprovedFp,
        deviationFiles <- deviationFiles,
        committedDeviationFiles <- committedDeviationFiles,
        deviationStatus <- deviationStatus,
        deviationCurrentFp <- deviationCurrentFp,
        committedDeviationCurrentFp <- committedDeviationCurrentFp,
        deviationApprovedFp <- deviationApprovedFp,
        nodeDeviationClaims <- nodeDeviationClaims,
        committedNodeDeviationClaims <- committedNodeDeviationClaims,
        lastCleanDeviationFiles <- lastCleanDeviationFiles,
        lastCleanDeviationStatus <- lastCleanDeviationStatus,
        lastCleanDeviationApprovedFp <- lastCleanDeviationApprovedFp,
        lastCleanNodeDeviationClaims <- lastCleanNodeDeviationClaims,
        latestDeviationReviewIds <- latestDeviationReviewIds,
        latestDeviationEvidenceLanes <- latestDeviationEvidenceLanes,
        nodeDifficulty <- nodeDifficulty,
        easyAttempts <- easyAttempts,
        reviewerComments <- reviewerComments,
        latestPaperEvidenceLanes <- latestPaperEvidenceLanes,
        latestCorrEvidenceLanes <- latestCorrEvidenceLanes,
        latestSoundEvidenceLanes <- latestSoundEvidenceLanes,
        latestPaperPanelSplit <- latestPaperPanelSplit,
        latestCorrPanelSplit <- latestCorrPanelSplit,
        latestSoundPanelSplit <- latestSoundPanelSplit,
        latestPaperReviewTargets <- latestPaperReviewTargets,
        latestCorrReviewNodes <- latestCorrReviewNodes,
        latestSoundReviewNodes <- latestSoundReviewNodes,
        previousPaperFindingLanes <- previousPaperFindingLanes,
        previousCorrFindingLanes <- previousCorrFindingLanes,
        previousSoundFindingLanes <- previousSoundFindingLanes,
        latestSubstantivenessEvidenceLanes <- latestSubstantivenessEvidenceLanes,
        latestSubstantivenessReviewNodes <- latestSubstantivenessReviewNodes,
        latestSubstantivenessPanelSplit <- latestSubstantivenessPanelSplit,
        previousSubstantivenessFindingLanes <- previousSubstantivenessFindingLanes,
        humanInputOutstanding <- humanInputOutstanding,
        nativeHistoryKinds <- nativeHistoryKinds,
        cyclesSinceClean <- cyclesSinceClean,
        hasEverBeenClean <- hasEverBeenClean,
        pendingTask <- pendingTask,
        cleanupAuditTasks <- cleanupAuditTasks,
        cleanupAuditScratchpad <- cleanupAuditScratchpad,
        cleanupAuditBurstCount <- cleanupAuditBurstCount,
        cleanupAuditRound <- cleanupAuditRound,
        cleanupConsecutiveInvalidWorkers <- cleanupConsecutiveInvalidWorkers,
        cleanupActiveTask <- cleanupActiveTask,
        cleanupForceDone <- cleanupForceDone,
        forceReviewAfterConeClean <- forceReviewAfterConeClean,
        activeCoarseNode <- activeCoarseNode,
        cyclesInCoarseRepairMode <- cyclesInCoarseRepairMode,
        stuckMathAuditActive <- stuckMathAuditActive,
        stuckMathAuditNeedInputAudit <- stuckMathAuditNeedInputAudit,
        stuckMathAuditBurstRetryCount <- stuckMathAuditBurstRetryCount,
        lastStuckMathAuditDispatchedCycle <- lastStuckMathAuditDispatchedCycle,
        pendingGlobalRepairRequest <- pendingGlobalRepairRequest,
        pendingGlobalRepairGrant <- pendingGlobalRepairGrant,
        latestGlobalRepairAuditDeclineReason <- latestGlobalRepairAuditDeclineReason,
        latestGlobalRepairAuditDeclineCycle <- latestGlobalRepairAuditDeclineCycle,
        lastReviewerGlobalRepairRequestCycle <- lastReviewerGlobalRepairRequestCycle,
        everShallowCoarseClosed <- everShallowCoarseClosed,
        globalRepairModeEnabled <- globalRepairModeEnabled,
        postAdvanceRoutingPending <- postAdvanceRoutingPending,
        pendingProtectedReapprovalNodes <- pendingProtectedReapprovalNodes,
        pendingProtectedSemanticScopeConfirmation <- pendingProtectedSemanticScopeConfirmation,
        auditPlan <- auditPlan,
        supersededAuditPlan <- supersededAuditPlan,
        soundAssessmentStatus <- soundAssessmentStatus,
        reviewerRequestedSoundVerifierNodes <- reviewerRequestedSoundVerifierNodes,
        soundReverificationContext <- soundReverificationContext,
        inFlightRequest <- inFlightRequest,
        response <- response

Spec == SP!Spec
TypeOK == SP!TypeOK
PendingTaskConsistent == SP!PendingTaskConsistent
WrapperRequestConsistent == SP!WrapperRequestConsistent
HumanGateMatchesState == SP!HumanGateMatchesState
NoInvalidAdvanceFlow == SP!NoInvalidAdvanceFlow
HeldTargetSuspendedByCorrBlockers == SP!HeldTargetSuspendedByCorrBlockers
DifficultyStateConsistent == SP!DifficultyStateConsistent
WorkerCoverageMatchesUpdates == SP!WorkerCoverageMatchesUpdates
WorkerStructuralMapsMatchObserved == SP!WorkerStructuralMapsMatchObserved
WorkerAcceptedResponsesSatisfyContract == SP!WorkerAcceptedResponsesSatisfyContract
WorkerStuckPreservesState == SP!WorkerNoProgressPreservesState
CleanupHasNoBlockers == SP!CleanupHasNoBlockers
StalePassClosurePreventsCleanupTransition == SP!StalePassClosurePreventsCleanupTransition
\* Cleanup-v2 invariants (2026-05-14) — surfaced to the sim suite.
\* See SupervisorProtocol.tla §"Cleanup-v2 invariants" cluster.
CleanupTasksShrinkMonotonic == SP!CleanupTasksShrinkMonotonic
CleanupTaskStatusTransitions == SP!CleanupTaskStatusTransitions
CleanupAuditTargetsPresent == SP!CleanupAuditTargetsPresent
CleanupExitImpliesFormalized == SP!CleanupExitImpliesFormalized
\* Tier 1 invariants (state-relation coherence) — surfaced to the sim suite.
\* See SupervisorProtocol.tla §"Tier 1 invariants" cluster.
FingerprintPinnedOnDecisiveStatus == SP!FingerprintPinnedOnDecisiveStatus
GlobalBlockersExhaustive == SP!GlobalBlockersExhaustive
QuiescentLiveEqualsCommitted == SP!QuiescentLiveEqualsCommitted
\* Tier 4 invariants (wrapper envelope + restart safety) — surfaced to
\* the sim suite. See SupervisorProtocol.tla §"Tier 4 invariants" cluster.
RequestIdMonotonic == SP!RequestIdMonotonic
RequestPayloadDerivable == SP!RequestPayloadDerivable
InFlightCycleBounded == SP!InFlightCycleBounded
\* Tier 2 invariants (phase dormancy and lifecycle gates) — surfaced
\* to the sim suite. See SupervisorProtocol.tla §"Tier 2 invariants"
\* cluster.
PhaseDormancyContract == SP!PhaseDormancyContract
StageOwnership == SP!StageOwnership
StuckMathAuditLatchWellFormed == SP!StuckMathAuditLatchWellFormed
CoarseAnchorWellFormed == SP!CoarseAnchorWellFormed
CleanupAuditRoundBound == SP!CleanupAuditRoundBound
\* Tier 3 invariants (reviewer contract, 2026-05-21) — surfaced to the
\* sim suite. See SupervisorProtocol.tla §"Tier 3 invariants (reviewer
\* contract)" cluster.
ReviewBlockerActionsWellFormed == SP!ReviewBlockerActionsWellFormed
LocalModeSoundnessCarveOut == SP!LocalModeSoundnessCarveOut
AuthorizedNodesScopeContract == SP!AuthorizedNodesScopeContract
ReviewerScopeAuthorizationComplete == SP!ReviewerScopeAuthorizationComplete
CleanupDoneTerminal == SP!CleanupDoneTerminal
AdjudicationFrontierContained == SP!AdjudicationFrontierContained
\* Deviation lane invariants (2026-05-27/28).
DeviationStickyFailDiscipline == SP!DeviationStickyFailDiscipline
DeviationUnauthorizedClaimSuppressesSubstantivenessPass ==
    SP!DeviationUnauthorizedClaimSuppressesSubstantivenessPass
DeviationClaimsCarrierWellFormed == SP!DeviationClaimsCarrierWellFormed
ReviewerOverrideEmptyUnderDefault == SP!ReviewerOverrideEmptyUnderDefault
DeviationTaskBlockerRoutingScope == SP!DeviationTaskBlockerRoutingScope
DeviationDeletionLeavesNoClaim == SP!DeviationDeletionLeavesNoClaim

=============================================================================
