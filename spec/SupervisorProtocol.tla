------------------------------ MODULE SupervisorProtocol ------------------------------
EXTENDS Integers, FiniteSets, Sequences

\* Intended theorem-stating supervisor protocol.
\*
\* This spec is intentionally simpler and more semantic than the current Python
\* implementation. It models the authoritative protocol we want:
\*
\* - one global verification ledger
\* - reviewer task blockers live only on the current pending task assignment
\* - invalid attempts cannot produce success-shaped reviewer outcomes
\* - theorem-stating ADVANCE requests are gated by a final full verification pass
\* - the theorem-stating -> proof_formalization boundary requires expert approval
\* - the authoritative expert gate is target-based (Option A)
\*
\* PAPER-TARGET PRESERVATION:
\*   Enforced in the kernel at commit time by a multi-axis
\*   correspondence-fingerprint reopen guard over `approved_target_nodes`
\*   (see `CorrespondenceFingerprint` in `runtime_cli_observations.rs` and
\*   the "Protected correspondence" section of `FILESPEC.md`). This spec
\*   models the abstract precondition (`current_*_pass` requires
\*   `approved == current` per-node) but not the kernel's per-axis
\*   refinement. See `spec/SPEC_TODO.md` items 5 and 9 for the
\*   structured-fingerprint extension that would let TLC verify
\*   properties like "preamble-only changes don't reopen correspondence."

CONSTANTS
    Nodes,
    Targets,
    VerifierLanes,
    Fingerprints,
    NodeKinds,
    ProofNodes,
    Deps,
    NodeRank,
    NodeOrder,
    VerifierBindings,
    CorrVerifierBindingByLane,
    SoundVerifierBindingByLane,
    WorkerBindingByProfile,
    ReviewerBinding,
    PreambleItemIds,
    NoNode,
    NoFingerprint,
    NoCheckpoint,
    MaxCycle,
    MaxAttempt,
    EasyMaxRetries,
    ProofInvalidReviewThreshold,
    StuckCoarseRepairThreshold,
    InitialConfiguredTargets,
    \* Authorized deviation reference-file ids (kernel: DeviationId). Each
    \* element is an abstract opaque id. Spec-side modelling lifted from
    \* kernel 7aad7cb ("Add authorized deviation tracking") and follow-up
    \* commits 4e83783 / efaafa7 (sticky-Fail, lastclean mirrors,
    \* short-circuits) / 4abe9dd (worker retire). Configs that don't want
    \* to model deviations may set Deviations <- {}; the deviation
    \* surface then degrades to no-ops.
    Deviations,
    NoDeviation,
    \* Option C (2026-06-04): retired. Retained as a constant declaration
    \* for sim.cfg back-compat; assumed to be FALSE in all model checks.
    \* The reviewer Pass-override authority has been removed entirely;
    \* see REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md.
    AllowReviewerPassOverride

ASSUME Nodes # {}
ASSUME Targets # {}
ASSUME InitialConfiguredTargets \subseteq Targets
ASSUME VerifierLanes # {}
ASSUME NoDeviation \notin Deviations
ASSUME AllowReviewerPassOverride \in BOOLEAN
ASSUME NodeKinds \in [Nodes -> {"preamble", "definition", "proof"}]
ASSUME ProofNodes \subseteq Nodes
ASSUME \A n \in Nodes: (n \in ProofNodes) <=> (NodeKinds[n] = "proof")
ASSUME Deps \in [Nodes -> SUBSET Nodes]
ASSUME NodeRank \in [Nodes -> Nat]
ASSUME NodeOrder \in [Nodes -> Nat]
ASSUME VerifierBindings # {}
ASSUME CorrVerifierBindingByLane \in [VerifierLanes -> VerifierBindings]
ASSUME SoundVerifierBindingByLane \in [VerifierLanes -> VerifierBindings]
ASSUME WorkerBindingByProfile \in [{"none", "theorem", "proof_easy", "proof_hard", "cleanup", "final_cleanup"} -> VerifierBindings]
ASSUME ReviewerBinding \in VerifierBindings
ASSUME PreambleItemIds \subseteq STRING
ASSUME \A n \in Nodes:
    /\ Deps[n] \subseteq (Nodes \ {n})
    /\ \A m \in Deps[n]: NodeRank[m] < NodeRank[n]
ASSUME \A n1, n2 \in Nodes: n1 # n2 => NodeOrder[n1] # NodeOrder[n2]
ASSUME NoNode \notin Nodes
ASSUME NoFingerprint \in Fingerprints
ASSUME MaxCycle \in Nat
ASSUME MaxAttempt \in Nat \ {0}
ASSUME EasyMaxRetries \in Nat \ {0}
ASSUME ProofInvalidReviewThreshold \in Nat \ {0}
ASSUME StuckCoarseRepairThreshold \in Nat \ {0}

StageValues ==
    {
        "Start",
        "Worker",
        "VerifyPaper",
        "VerifyCorr",
        "VerifySound",
        "Reviewer",
        "HumanGate",
        "Complete",
        \* Cleanup-v2 (2026-05-14) audit sub-phase; carries `audit`-kind
        \* in-flight requests. Mirror of kernel `Stage::CleanupAudit`
        \* (model.rs). Reached only inside `Phase::Cleanup` via
        \* `enter_cleanup_phase` -> StartCycle.
        "CleanupAudit",
        \* Proof-phase StuckMathAudit sub-phase. Carries
        \* `stuck_math_audit`-kind in-flight requests. Mirror of kernel
        \* `Stage::StuckMathAudit`. Reached from StartCycle when the
        \* TheoremStating Sound-stagnation gate preempts a Worker
        \* dispatch (engine.rs `start_cycle`), and from
        \* `issue_review_or_stuck_math_audit` in ProofFormalization
        \* when the audit gate fires.
        "StuckMathAudit"
    }

PhaseValues == {"theorem_stating", "proof_formalization", "cleanup", "complete"}

CorrStates == {"unknown", "pass", "fail"}
CorrUpdates == CorrStates \cup {"same"}

SoundStates == {"unknown", "pass", "fail", "structural"}
SoundUpdates == SoundStates \cup {"same"}

\* Mirror of kernel `SoundAssessmentStatus` (model.rs). The legacy
\* `SoundStates` four-value map (`soundStatus`) is the engagement view
\* used by reviewer / verifier validators; this 11-value taxonomy is the
\* `sound_assessments`-store status the kernel computes via
\* `current_sound_assessment()`. The two diverge for the
\* dep-edit-only-stale lane: `soundStatus` reads `"pass"` while
\* `soundAssessmentStatus` reads `"dep_edit_only_stale_fail"` or
\* `"dep_edit_only_stale_pass_deferred"` and the kernel deliberately
\* DOES NOT auto-cascade into a reopen wave (see feedback memory
\* `feedback_dont_kill_on_data_structure_drift.md`).
\*
\* Values explicitly stored by VerifierPanel / Reviewer write paths:
\*   "verifier_pass", "verifier_fail", "verifier_structural",
\*   "split_unknown", "reviewer_pinned_fail",
\*   "reviewer_accepted_pass" (retired, kept for serde back-compat).
\* Values returned by the lazy `current_sound_assessment()` predicate
\* when the stored entry's fingerprint drifts:
\*   "fresh_unknown" (no entry stored at all),
\*   "self_edit_unknown" (own_tex hash differs),
\*   "dep_edit_only_stale_pass_deferred" (dep hashes differ; stored
\*       was pass),
\*   "dep_edit_only_stale_fail" (dep hashes differ; stored was fail),
\*   "sketch_auto_fail" (node \in sketchProofNodes).
SoundAssessmentStatuses ==
    {
        "fresh_unknown",
        "verifier_pass",
        "verifier_fail",
        "verifier_structural",
        "reviewer_pinned_fail",
        "reviewer_accepted_pass",
        "sketch_auto_fail",
        "self_edit_unknown",
        "dep_edit_only_stale_fail",
        "dep_edit_only_stale_pass_deferred",
        "split_unknown"
    }

\* Sentinel for `soundReverificationContext` when the kernel's
\* `Option<SoundReverificationContext>` is `None`. The kernel
\* materializes this on a per-request basis in
\* `request_sound_reverification_context()` — `Some(...)` only when
\* the current assessment status is one of
\* `dep_edit_only_stale_pass_deferred` / `self_edit_unknown`. The spec
\* abstracts it as a top-level variable so the "next Sound request
\* will carry these facts" semantics are observable.
NoSoundReverificationContext == "NoSoundReverificationContext"

\* Mirror of `SoundReverificationContext` (model.rs). The kernel record
\* also carries `deps_changed: Vec<SoundDepHashDriftEntry>` and
\* `prior_lane_evidence: BTreeMap<LaneId, ...>` — both presentational
\* facts the verifier prompt reads but no decision in the kernel
\* branches on. TODO: model `deps_changed` if a downstream invariant
\* needs it; for now the load-bearing fields are
\* `target` / `priorStatus` / `currentStatus` / `ownTexChanged`.
SoundReverificationContextValues ==
    {NoSoundReverificationContext}
    \cup
    [
        target: Nodes,
        priorStatus: SoundAssessmentStatuses,
        currentStatus:
            {"dep_edit_only_stale_pass_deferred", "self_edit_unknown"},
        ownTexChanged: BOOLEAN
    ]

ReviewDecisions == {"none", "CONTINUE", "ADVANCE_PHASE", "NEED_INPUT", "DONE"}

WorkerOutcomes == {"none", "valid", "invalid", "stuck", "needs_restructure"}
\* Mirror of kernel `RetryOutcomeKind`. `transport` (Bug X principled
\* fix) is the infrastructure-failure bucket counted separately from
\* `invalid` against `transport_invalid_review_threshold` rather than
\* `proof_invalid_review_threshold`.
RetryOutcomeKinds == {"none", "invalid", "stuck", "needs_restructure", "transport"}
ResponseStatuses == {"none", "ok", "malformed"}
HumanSignals == {"none", "approve", "feedback"}
ReviewerCommentValues == {"", "set"}
DeterministicWorkerRejectionReasonSeqValues == {<< >>, <<"set">>}
WorkerContextModes == {"resume", "fresh"}
WorkerWorkStyleHints == {"none", "restructure"}
PaperFocusLineValues == 1..3
PaperFocusRangeValues ==
    UNION {
        UNION {
            IF s <= e THEN
                { [startLine |-> s, endLine |-> e, reason |-> r] : r \in ReviewerCommentValues }
            ELSE
                {}
            : e \in PaperFocusLineValues
        }
        : s \in PaperFocusLineValues
    }
PaperFocusRangeSeqValues ==
    {<< >>} \cup UNION { [1..n -> PaperFocusRangeValues] : n \in 1..2 }
\* Mirror of kernel `GateKind` (model.rs). `protected_reapproval` was
\* added in the protected-target reapproval pathway (see kernel
\* `maybe_issue_protected_reapproval` in engine.rs).
GateKinds == {"none", "advance", "needinput", "protected_reapproval"}
\* Mirror of kernel `RequestKind` (model.rs). Cleanup-v2 added `audit`;
\* StuckMathAudit role added `stuck_math_audit`.
RequestKinds == {"none", "worker", "paper", "corr", "sound", "review", "human_gate", "audit", "stuck_math_audit"}
TargetEditModes == {"global", "targeted"}
ProofEditModes == {"local", "restructure", "coarse_restructure"}
TaskModes == TargetEditModes \cup ProofEditModes \cup {"cleanup"}
DifficultyValues == {"easy", "hard"}
DifficultyUpdates == DifficultyValues \cup {"same"}
WorkerProfiles == {"none", "theorem", "proof_easy", "proof_hard", "cleanup", "final_cleanup"}
WorkerValidationKinds == {
    "none",
    "theorem_global",
    "theorem_targeted",
    \* Mirror of kernel `WorkerValidationKind::ProofEasy` (model.rs).
    \* Used by the proof-easy worker dispatch path (one fixed active
    \* node, no new obligations, no must-close — distinct from
    \* `proof_local` and from cleanup).
    "proof_easy",
    "proof_local",
    "proof_restructure",
    "proof_coarse_restructure",
    "cleanup",
    "final_cleanup"
}
WorkerNewNodesAllowedKinds == {
    "theorem_global",
    "theorem_targeted",
    "proof_local",
    "proof_restructure",
    "proof_coarse_restructure"
}
WorkerBaselineScopes == {"none", "authorized_nodes", "all_present"}
WorkerProofDeltaModes == {"none", "easy", "local", "restructure", "coarse_restructure"}
ScopedTabletAllowedNodesModes == {"explicit", "all_present", "previous_or_explicit"}
NodeKindValues == {"preamble", "definition", "proof"}
NodeKindUpdates == NodeKindValues \cup {"same"}
WorkerValidationExecutionPlanStepValues ==
    { [kind |-> "theorem_target_edit_scope", target |-> n, initialScope |-> s] :
        n \in (Nodes \cup {NoNode}), s \in SUBSET Nodes }
    \cup
    { [kind |-> "scoped_tablet", allowedNodesMode |-> mode, explicitNodes |-> s] :
        mode \in ScopedTabletAllowedNodesModes, s \in SUBSET Nodes }
    \cup
    { [kind |-> "proof_easy_scope", active |-> n] :
        n \in (Nodes \cup {NoNode}) }
    \cup
    { [kind |-> "proof_worker_delta", active |-> n, mode |-> mode, authorizedNodes |-> s,
         allowNewObligations |-> allow, mustCloseActive |-> close] :
        n \in (Nodes \cup {NoNode}),
        mode \in WorkerProofDeltaModes,
        s \in SUBSET Nodes,
        allow \in BOOLEAN,
        close \in BOOLEAN }
    \cup
    { [kind |-> "cleanup_preserving"] }
    \cup
    { [kind |-> "final_cleanup_preserving"] }
WorkerValidationStepKinds ==
    {
        "invalid",
        "theorem_target_edit_scope",
        "scoped_tablet",
        "proof_easy_scope",
        "proof_worker_delta",
        "cleanup_preserving",
        "final_cleanup_preserving"
    }
WorkerValidationStepResultValues ==
    {
        [
            kind |-> kind,
            ok |-> ok,
            detail |-> detail,
            errors |-> errors,
            buildOutput |-> buildOutput,
            allowedNodes |-> allowedNodes
        ] :
            kind \in WorkerValidationStepKinds,
            ok \in BOOLEAN,
            detail \in STRING,
            errors \in Seq(STRING),
            buildOutput \in STRING,
            allowedNodes \in SUBSET Nodes
    }
SuccessfulValidationStepResult(step) ==
    [
        kind |-> step.kind,
        ok |-> TRUE,
        detail |-> "",
        errors |-> << >>,
        buildOutput |-> "",
        allowedNodes |-> {}
    ]

SuccessfulValidationStepResults(plan) ==
    [i \in 1..Len(plan) |-> SuccessfulValidationStepResult(plan[i])]

WorkerValidationStepResultsSatisfyContract(plan, results) ==
    /\ Len(results) = Len(plan)
    /\ \A i \in 1..Len(plan):
        /\ results[i].kind = plan[i].kind
        /\ results[i].ok
        /\ Len(results[i].errors) = 0
KernelOwnedValidationKinds ==
    {
        "theorem_global",
        "theorem_targeted",
        "proof_local",
        "proof_restructure",
        "proof_coarse_restructure",
        "cleanup"
    }
\* The trellis checker interface keeps raw external observations and support
\* writes below the protocol boundary. Python is restricted to atomic
\* external-tool, raw Lean semantic-payload, and support-snapshot materialize /
\* write actions; kernel-local file reads, source parsing, snapshot derivation,
\* fingerprint composition, scope interpretation, and accept/reject reduction
\* are all Rust-owned.
\*
\* The deployed runtime intentionally separates:
\* - worker-local advisory checking in the worker repo with worker-writable cache
\* - authoritative supervisor checking in a separate supervisor-owned workspace
\*   produced by syncing the worker repo's semantic source snapshot
\*
\* The protocol does not model concrete file-copy mechanics, HOME, or cache
\* directories. It only models the semantic rule: the worker may consult the
\* same checker command locally, but only the supervisor-owned rerun is
\* authoritative for accept/reject.
NoWorkerContext == [
    enabled |-> FALSE,
    activeDifficulty |-> "hard",
    activeEasyAttempts |-> 0,
    workerProfile |-> "none",
    validationKind |-> "none",
    authorizedNodes |-> {},
    allowNewObligations |-> TRUE,
    mustCloseActive |-> FALSE,
    nextContextMode |-> "resume",
    paperFocusRanges |-> << >>,
    workStyleHint |-> "none",
    consumedGlobalRepairGrant |-> FALSE
]

LaneBindingUniverse ==
    {
        [laneId |-> lane, binding |-> binding] :
            lane \in VerifierLanes,
            binding \in VerifierBindings
    }

DefaultVerifierBinding ==
    CHOOSE binding \in VerifierBindings: TRUE

LaneBindings(bindingByLane, lanes) ==
    {
        [laneId |-> lane, binding |-> bindingByLane[lane]] :
            lane \in lanes
    }
NoWorkerAcceptance == [
    enabled |-> FALSE,
    validationKind |-> "none",
    authorizedNodes |-> {},
    validationExecutionPlan |-> << >>,
    requireExplicitTargetClaimsForNewNodes |-> TRUE,
    forbidTabletChangesWhenStuck |-> FALSE,
    observationPlan |->
        [
            captureBeforeSnapshot |-> FALSE,
            captureBeforeTabletContents |-> FALSE,
            captureScopedTabletBaselineErrors |-> FALSE,
            scopedTabletBaselineScope |-> "none",
            captureImportsBefore |-> FALSE,
            captureExpectedActiveHash |-> FALSE,
            captureBaselineDeclarationHashes |-> FALSE,
            captureBaselineCorrespondenceHashes |-> FALSE
        ]
]

NoCorrContract == [
    promptFragments |-> << >>,
    requestSummary |->
        [
            phase |-> "",
            targets |-> {},
            nodes |-> {},
            blockedTargets |-> {}
        ],
    previousOwnFindingsByLane |-> {},
    issueReportingPolicy |-> "none",
    fixedItemReportingPolicy |-> "none",
    nodeIssueScope |-> {},
    rubric |->
        [
            statementAlignmentChecks |-> << >>,
            projectDefinitionPolicy |-> "none",
            definitionHygiene |-> << >>,
            duplicateMathlibDefinitionPolicy |-> "none",
            preambleItemIssuePolicy |-> "none"
        ],
    artifactContract |->
        [
            resultType |-> "correspondence_result_v1",
            overallRule |-> "approve_iff_pass",
            promptSchemaExample |->
                [
                    correspondence |->
                        [
                            decision |-> "PASS or FAIL",
                            issues |-> << >>
                        ],
                    overall |-> "APPROVE or REJECT",
                    summary |-> "",
                    comments |-> ""
                ],
            phaseBlocks |->
                [
                    correspondence |->
                        [
                            decisionValues |-> << >>,
                            issueSubjectKind |-> "none"
                        ]
                ]
        ],
    artifactPromptView |->
        [
            rawOutputFormat |-> "json_only",
            escapeJsonBackslashes |-> TRUE,
            doneMarkerContract |-> "write_done_after_json_check_passes",
            checkerAuthority |-> "exact_command_is_authoritative",
            jsonCheckCommandTemplate |-> << >>,
            acceptanceCheckCommandTemplate |-> << >>,
            failureRecovery |-> "json_check_required_acceptance_check_best_effort",
            stdoutPolicy |-> "do_not_print_json_to_stdout"
        ],
    preambleContract |->
        [
            mode |-> "none",
            itemIds |-> {},
            emptyItemsVacuouslySupported |-> TRUE
        ]
]

NoPaperContract == [
    promptFragments |-> << >>,
    requestSummary |->
        [
            phase |-> "",
            targets |-> {},
            blockedTargets |-> {}
        ],
    previousOwnFindingsByLane |-> {},
    issueReportingPolicy |-> "none",
    fixedItemReportingPolicy |-> "none",
    targetIssueScope |-> {},
    rubric |->
        [
            paperStatementAuthority |-> "none",
            coveringSetAuthority |-> "none",
            definitionDependencyAuthority |-> "none",
            faithfulnessStandard |-> "none"
        ],
    artifactContract |->
        [
            resultType |-> "paper_faithfulness_result_v1",
            overallRule |-> "approve_iff_pass",
            promptSchemaExample |->
                [
                    paperFaithfulness |->
                        [
                            decision |-> "PASS or FAIL",
                            issues |-> << >>
                        ],
                    overall |-> "APPROVE or REJECT",
                    summary |-> "",
                    comments |-> ""
                ],
            phaseBlocks |->
                [
                    paperFaithfulness |->
                        [
                            decisionValues |-> << >>,
                            issueSubjectKind |-> "target"
                        ]
                ]
        ],
    artifactPromptView |->
        [
            rawOutputFormat |-> "json_only",
            escapeJsonBackslashes |-> TRUE,
            doneMarkerContract |-> "write_done_after_json_check_passes",
            checkerAuthority |-> "exact_command_is_authoritative",
            jsonCheckCommandTemplate |-> << >>,
            acceptanceCheckCommandTemplate |-> << >>,
            failureRecovery |-> "json_check_required_acceptance_check_best_effort",
            stdoutPolicy |-> "do_not_print_json_to_stdout"
        ]
]

NoSoundContract == [
    promptFragments |-> << >>,
    requestSummary |->
        [
            phase |-> "",
            node |-> "",
            activeNode |-> "",
            heldTarget |-> ""
        ],
    previousOwnFindingsByLane |-> {},
    targetNodes |-> {},
    evaluationBasis |-> "none",
    detailFloor |-> "none",
    rubric |->
        [
            proofStandard |-> "none",
            rejectSketches |-> FALSE,
            detailFloor |-> "none",
            leanCodeRelevance |-> "none"
        ],
    artifactContract |->
        [
            resultType |-> "soundness_result_v1",
            decisionValues |-> << >>,
            overallRule |-> "approve_iff_sound",
            promptSchemaExample |->
                [
                    node |-> "",
                    soundness |->
                        [
                            decision |-> "",
                            explanation |-> ""
                        ],
                    overall |-> "APPROVE or REJECT",
                    summary |-> "",
                    feedback |-> ""
                ]
        ],
    artifactPromptView |->
        [
            rawOutputFormat |-> "json_only",
            escapeJsonBackslashes |-> TRUE,
            doneMarkerContract |-> "write_done_after_json_check_passes",
            checkerAuthority |-> "exact_command_is_authoritative",
            jsonCheckCommandTemplate |-> << >>,
            acceptanceCheckCommandTemplate |-> << >>,
            failureRecovery |-> "json_check_required_acceptance_check_best_effort",
            stdoutPolicy |-> "do_not_print_json_to_stdout"
        ]
]

NoWorkerContract == [
    promptFragments |-> << >>,
    requestSummary |->
        [
            phase |-> "",
            mode |-> "",
            activeNode |-> "",
            heldTarget |-> "",
            freshContext |-> FALSE,
            workerContext |-> NoWorkerContext,
            blockers |-> {},
            currentPresentNodes |-> {},
            currentProofNodes |-> {},
            currentDeps |-> [n \in Nodes |-> {}],
            currentTargetClaims |-> [n \in Nodes |-> {}]
        ],
    reviewerComments |-> "",
    resultType |-> "worker_result_v1",
    kernelDerivesStructuralSnapshot |-> TRUE,
    allowedOutcomes |-> << >>,
    reportedDeltaFields |-> << >>,
    forbiddenLegacyFields |-> << >>,
    promptSchemaExample |->
        [
            outcome |-> "",
            summary |-> "",
            comments |-> "",
            semanticDepUpdates |-> [nodeId |-> << >>],
            targetClaimUpdates |-> [nodeId |-> << >>],
            difficultyUpdates |-> [nodeId |-> ""]
        ],
    scopeContract |->
        [
            existingNodeScopeMode |-> "none",
            authorizedExistingNodes |-> {},
            configuredTargets |-> {},
            pendingTargets |-> {},
            pendingTargetsMeaning |-> "none",
            newNodesAllowed |-> FALSE
        ],
    stuckContract |->
        [
            allowed |-> FALSE,
            forbidTabletChangesWhenStuck |-> FALSE,
            meaning |-> "none"
        ],
    needsRestructureContract |->
        [
            allowed |-> FALSE,
            forbidTabletChangesWhenNeedsRestructure |-> FALSE,
            meaning |-> "none"
        ],
    artifactPromptView |->
        [
            rawOutputFormat |-> "json_only",
            escapeJsonBackslashes |-> TRUE,
            doneMarkerContract |-> "write_done_after_json_check_passes",
            checkerAuthority |-> "exact_command_is_authoritative",
            jsonCheckCommandTemplate |-> << >>,
            acceptanceCheckCommandTemplate |-> << >>,
            failureRecovery |-> "json_check_required_acceptance_check_best_effort",
            stdoutPolicy |-> "do_not_print_json_to_stdout"
        ]
]

NoReviewContract == [
    promptFragments |-> << >>,
    requestSummary |->
        [
            phase |-> "",
            mode |-> "",
            activeNode |-> "",
            heldTarget |-> "",
            invalidAttempt |-> FALSE,
            retryOutcomeKind |-> "none",
            retryAttempt |-> 0,
            humanInputOutstanding |-> FALSE,
            blockedTargets |-> {}
        ],
    artifactContract |->
        [
            resultType |-> "review_result_v1",
            requiredFields |-> << >>,
            optionalFields |-> << >>,
            promptSchemaExample |->
                [
                    decision |-> << >>,
                    reason |-> "",
                    comments |-> "",
                    taskBlockerIds |-> << >>,
                    overrideBlockerIds |-> << >>,
                    resetBlockerIds |-> << >>,
                    nextActive |-> "",
                    nextMode |-> << >>,
                    reset |-> << >>,
                    difficultyUpdates |-> [nodeId |-> ""],
                    clearHumanInput |-> "",
                    nextWorkerContextMode |-> "",
                    paperFocusRanges |-> << >>,
                    workStyleHint |-> ""
                ]
        ],
    verifierEvidence |-> [paper |-> {}, corr |-> {}, sound |-> {}],
    \* Each list is optional; omitted blockers remain live. The kernel
    \* checks subset + pairwise disjoint, NOT completeness.
    blockerActions |->
        [
            required |-> FALSE,
            actionFields |-> << >>,
            choices |-> << >>,
            allowedOverrideIds |-> {},
            allowedResetIds |-> {},
            resetSemantics |-> "none"
        ],
    nextActiveContract |->
        [
            allowedNodes |-> {},
            targetedAllowedNodes |-> {},
            allowTargetedWithoutNextActive |-> FALSE
        ],
    difficultyUpdateContract |->
        [
            allowedNodes |-> {}
        ],
    clearHumanInputContract |->
        [
            allowedWhenOutstanding |-> FALSE,
            omitWhenNotAllowed |-> TRUE
        ],
    commentsContract |->
        [
            field |-> "comments",
            semantics |-> "non_authoritative_guidance_forwarded_to_future_workers",
            emptyStringMeansNoComments |-> TRUE
        ],
    routingHintsContract |->
        [
            nextWorkerContextModeValues |-> << >>,
            paperFocusRangesShape |->
                [
                    startLine |-> "",
                    endLine |-> "",
                    reason |-> ""
                ],
            workStyleHintValues |-> << >>,
            continueOnly |-> FALSE,
            advisoryOnly |-> FALSE,
            semantics |-> "none"
        ],
    resetContract |->
        [
            allowedResets |-> {},
            lastCommitSemantics |-> "discard_unaccepted_live_changes_and_resume_from_last_accepted_checkpoint"
        ],
    artifactPromptView |->
        [
            rawOutputFormat |-> "json_only",
            escapeJsonBackslashes |-> TRUE,
            doneMarkerContract |-> "write_done_after_json_check_passes",
            checkerAuthority |-> "exact_command_is_authoritative",
            jsonCheckCommandTemplate |-> << >>,
            acceptanceCheckCommandTemplate |-> << >>,
            failureRecovery |-> "json_check_required_acceptance_check_best_effort",
            stdoutPolicy |-> "do_not_print_json_to_stdout"
        ]
]

DefaultCoverage == [t \in Targets |-> {}]
DefaultFp == [n \in Nodes |-> NoFingerprint]
DefaultTargetFp == [t \in Targets |-> NoFingerprint]
DefaultPaper == [t \in Targets |-> "unknown"]
DefaultSubstantiveness == [n \in Nodes |-> "unknown"]
DefaultSound == [n \in Nodes |-> "unknown"]
\* Default for `soundAssessmentStatus`: nodes whose stored assessment
\* entry is absent surface as `fresh_unknown` via the kernel's
\* `current_sound_assessment()` fallback (model.rs).
DefaultSoundAssessmentStatus == [n \in Nodes |-> "fresh_unknown"]
\* Deviation-lane defaults. Initial state: no deviations are alive.
\* `DefaultDeviationFiles` is the kernel's `BTreeMap<DeviationId, String>`
\* materialized presence-only: TRUE iff alive. The TLC functions need a
\* total domain on `Deviations`, so the maps are total but most ids carry
\* the sentinel "absent" value (FALSE / "unknown" / NoFingerprint / {}).
DefaultDeviationFiles == [id \in Deviations |-> FALSE]
DefaultDeviationStatus == [id \in Deviations |-> "unknown"]
DefaultDeviationFp == [id \in Deviations |-> NoFingerprint]
DefaultNodeDeviationClaims == [n \in Nodes |-> {}]
DefaultCorrUpdate == [n \in Nodes |-> "same"]
DefaultPaperUpdate == [t \in Targets |-> "same"]
DefaultSubstantivenessUpdate == [n \in Nodes |-> "same"]
DefaultSoundUpdate == [n \in Nodes |-> "same"]
DefaultCorrLaneMaps == [l \in VerifierLanes |-> DefaultCorrUpdate]
DefaultPaperLaneMaps == [l \in VerifierLanes |-> DefaultPaperUpdate]
DefaultSubstantivenessLaneMaps == [l \in VerifierLanes |-> DefaultSubstantivenessUpdate]
DefaultSoundLaneMaps == [l \in VerifierLanes |-> DefaultSoundUpdate]
DefaultDifficulty == [n \in Nodes |-> "hard"]
DefaultDifficultyUpdate == [n \in Nodes |-> "same"]
DefaultEasyAttempts == [n \in Nodes |-> 0]
InitialPresentNodes == {n \in Nodes : NodeKinds[n] = "preamble"}
InitialNodeKinds ==
    [n \in Nodes |-> IF n \in InitialPresentNodes THEN "preamble" ELSE "definition"]
ProjectInvariants ==
    [
        nodePairContract |-> "every_present_node_has_lean_and_nl_statement",
        proofBearingContract |-> "proof_nodes_need_closed_lean_or_rigorous_nl",
        nodeFileContract |-> "tablet_node_files_must_follow_filespec",
        filespecReference |-> "FILESPEC.md",
        progressModes |-> <<"close_proof", "paper_faithful_dag_improvement">>,
        roleAuthority |->
            [
                worker |-> "writes_repository_content_only",
                reviewer |-> "chooses_next_step_and_guidance",
                verifier |-> "checks_invariants_without_choosing_work"
            ]
    ]
DefaultNodeSetMap == [n \in Nodes |-> {}]
DefaultTargetClaimMap == [n \in Nodes |-> {}]
InitialCorr ==
    [n \in Nodes |-> IF n \in InitialPresentNodes THEN "pass" ELSE "unknown"]
SameSetUpdate == [kind |-> "same", value |-> {}]
SetUpdate(value) == [kind |-> "set", value |-> value]
NodeSetUpdateValues ==
    {SameSetUpdate}
    \cup
    {[kind |-> "set", value |-> s] : s \in SUBSET Nodes}
TargetClaimUpdateValues ==
    {SameSetUpdate}
    \cup
    {[kind |-> "set", value |-> s] : s \in SUBSET Targets}
DefaultProofNodeUpdate == [n \in Nodes |-> "same"]
DefaultNodeKindUpdate == [n \in Nodes |-> "same"]
DefaultNodeSetUpdate == [n \in Nodes |-> SameSetUpdate]
DefaultTargetClaimUpdate == [n \in Nodes |-> SameSetUpdate]

\* global_repair_mode sentinels (Step 1 kernel model.rs). Mirror
\* `Option::None` for the two persisted carriers. The actual record
\* shape is checked in TypeOK; these constants are the no-carrier case.
\* Hoisted above NoResponse because the response defaults reference
\* NoGlobalRepairRequest.
NoGlobalRepairRequest == "NoGlobalRepairRequest"
NoGlobalRepairGrant == "NoGlobalRepairGrant"

\* Sentinel for `pendingProtectedSemanticScopeConfirmation` when the
\* kernel's `pending_protected_semantic_scope_confirmation` is
\* `Option::None`. The shape mirrors kernel
\* `ProtectedSemanticChangeConfirmation` (model.rs): node set, next-
\* active hint, next-mode, allow_new_obligations, must_close_active.
NoProtectedSemanticChangeConfirmation == "NoProtectedSemanticChangeConfirmation"
ProtectedSemanticChangeConfirmationValues ==
    {NoProtectedSemanticChangeConfirmation}
    \cup
    [
        nodes: SUBSET Nodes,
        nextActive: Nodes \cup {NoNode},
        nextMode: TaskModes,
        allowNewObligations: BOOLEAN,
        mustCloseActive: BOOLEAN
    ]

\* Sentinel for `auditPlan` / `supersededAuditPlan` when the kernel's
\* `Option<AuditPlan>` is `None`. Mirror of kernel `AuditPlan`
\* (model.rs): a list of `AuditTask` records and an optional
\* `coneClean` node. The spec abstracts `tasks` as a function from a
\* finite set of task ids (the bounded universe is `AuditTaskIds`,
\* introduced below) to `{"pending","dismissed"}`, and `coneClean` as
\* `Nodes \cup {NoNode}`.
NoAuditPlan == "NoAuditPlan"
\* Bounded universe of audit-task identifiers. The kernel allocates
\* fresh strings per audit dispatch; the spec models this abstractly
\* as a fixed finite set so plans are TLC-representable. Plans
\* materialize as `tasks` maps over a subset of `AuditTaskIds`.
AuditTaskIds == {"audit_task_1", "audit_task_2"}
AuditTaskStatusValues == {"pending", "dismissed"}
AuditPlanValues ==
    {NoAuditPlan}
    \cup
    [
        tasks: [AuditTaskIds -> AuditTaskStatusValues],
        coneClean: Nodes \cup {NoNode}
    ]

NoResponse ==
    [
        status |-> "none",
        kind |-> "none",
        cycle |-> 0,
        workerOutcome |-> "none",
        validationStepResults |-> << >>,
        present |-> {},
        open |-> {},
        coverage |-> DefaultCoverage,
        corrCurrent |-> DefaultFp,
        paperCurrent |-> DefaultTargetFp,
        soundCurrent |-> DefaultFp,
        targetFp |-> DefaultFp,
        corrMap |-> DefaultCorrUpdate,
        paperMap |-> DefaultPaperUpdate,
        substantivenessMap |-> DefaultSubstantivenessUpdate,
        soundMap |-> DefaultSoundUpdate,
        corrLaneMaps |-> DefaultCorrLaneMaps,
        paperLaneMaps |-> DefaultPaperLaneMaps,
        substantivenessLaneMaps |-> DefaultSubstantivenessLaneMaps,
        soundLaneMaps |-> DefaultSoundLaneMaps,
        paperPanelSplit |-> FALSE,
        substantivenessPanelSplit |-> FALSE,
        corrPanelSplit |-> FALSE,
        soundPanelSplit |-> FALSE,
        proofNodeMap |-> DefaultProofNodeUpdate,
        nodeKindMap |-> DefaultNodeKindUpdate,
        depMap |-> DefaultNodeSetUpdate,
        targetClaimMap |-> DefaultTargetClaimUpdate,
        difficultyMap |-> DefaultDifficultyUpdate,
        decision |-> "none",
        comments |-> "",
        taskBlockers |-> {},
        overrideBlockers |-> {},
        resetBlockers |-> {},
        nextActive |-> NoNode,
        \* Proposal v32: reviewer-chosen next active coarse anchor.
        \* Must be NoNode outside proof_formalization, on retry-review,
        \* or when ActiveCoarseChangeAllowed is FALSE. When set, must be
        \* a member of KernelHintedNextActiveCoarseNodes.
        nextActiveCoarse |-> NoNode,
        reset |-> NoCheckpoint,
        nextMode |-> "global",
        humanChoice |-> "none",
        clearHumanInput |-> FALSE,
        nextWorkerContextMode |-> "resume",
        paperFocusRanges |-> << >>,
        workStyleHint |-> "none",
        allowNewObligations |-> TRUE,
        mustCloseActive |-> FALSE,
        authorizedNodes |-> {},
        \* global_repair_mode (2026-06-05). Defaults model the absence
        \* of a Step A / Step C signal on the reviewer side, and
        \* Step B decline-with-no-grant on the auditor side. The TLA
        \* response shape is union-typed across review and
        \* stuck_math_audit kinds; only the actions that filter on
        \* `response.kind` actually read the relevant subset.
        globalRepairRequest |-> NoGlobalRepairRequest,
        consumeGlobalRepairGrant |-> FALSE,
        globalRepairApprove |-> FALSE,
        globalRepairApprovedExtensionNodes |-> {},
        \* StuckMathAudit-response carriers (kernel
        \* `StuckMathAuditResponse` in model.rs). `confirmNeedInput`
        \* drives the dispatch-to-HumanGate arm; `coneClean` carries
        \* the optional `ResetChoice::TheoremStatingNode` reset; the
        \* full `auditPlan` records the auditor-published plan that
        \* lands in `state.audit_plan`.
        confirmNeedInput |-> FALSE,
        coneClean |-> NoNode,
        auditPlan |-> NoAuditPlan,
        \* Reviewer-response audit-plan mutations (kernel
        \* `apply_review_audit_plan_actions`). `dismissAuditPlan` moves
        \* the live plan into `superseded_audit_plan`; `dismissedTasks`
        \* names individual task ids to flag as dismissed on the live
        \* plan.
        dismissAuditPlan |-> FALSE,
        dismissedTasks |-> {},
        \* Worker-response protected-target signal (kernel
        \* `WorkerResponse::protected_semantic_change_nodes`). Worker
        \* lifts changes from its sandbox into the reviewer-tracked
        \* set, populated on Valid acceptance.
        protectedSemanticChangeNodes |-> {},
        \* Reviewer-response: nodes the reviewer is asking the kernel
        \* to dispatch a Sound verifier on (mirror of
        \* `ReviewResponse::request_sound_verifier_nodes`, engine.rs
        \* `queue_reviewer_requested_sound_verifiers`). Filtered to
        \* `sound_verifier_eligible` && not-already-VerifierPass before
        \* admission into `reviewerRequestedSoundVerifierNodes`.
        requestSoundVerifierNodes |-> {}
    ]

NoRequest ==
    [
        id |-> 0,
        kind |-> "none",
        cycle |-> 0,
        phase |-> "complete",
        active |-> NoNode,
        held |-> NoNode,
        mode |-> "global",
        blockers |-> {},
        overrides |-> {},
        blockedTargets |-> {},
        configuredTargets |-> {},
        verifyNodes |-> {},
        verifyTargets |-> {},
        verifyLanes |-> {},
        paperVerifyLaneBindings |-> {},
        corrVerifyLaneBindings |-> {},
        soundVerifyLaneBindings |-> {},
        paperVerifyTargets |-> {},
        substantivenessVerifyNodes |-> {},
        corrVerifyNodes |-> {},
        corrVerifyTargets |-> {},
        soundVerifyNodes |-> {},
        soundVerifyNode |-> NoNode,
        runtimeSupportRequired |-> FALSE,
        allowedDecisions |-> {},
        allowedNextModes |-> {},
        allowedNextActiveNodes |-> {},
        targetedNextActiveNodes |-> {},
        allowTargetedWithoutNextActive |-> FALSE,
        allowedResets |-> {},
        allowedResetBlockers |-> {},
        allowedOverrideBlockers |-> {},
        allowedOverrideBlockerIds |-> {},
        allowedResetBlockerIds |-> {},
        reviewBlockerChoices |-> {},
        allowedDifficultyUpdateNodes |-> {},
        currentPresentNodes |-> {},
        currentProofNodes |-> {},
        currentNodeKinds |-> [n \in {} |-> "definition"],
        currentDeps |-> DefaultNodeSetMap,
        currentTargetClaims |-> DefaultTargetClaimMap,
        reviewerComments |-> "",
        deterministicWorkerRejectionReasons |-> << >>,
        reviewVerifierEvidence |-> [paper |-> {}, corr |-> {}, sound |-> {}],
        projectInvariants |-> ProjectInvariants,
        freshContext |-> FALSE,
        promptContractVersion |-> 0,
        paperContract |-> NoPaperContract,
        corrContract |-> NoCorrContract,
        soundContract |-> NoSoundContract,
        workerContract |-> NoWorkerContract,
        reviewContract |-> NoReviewContract,
        workerBinding |-> DefaultVerifierBinding,
        reviewerBinding |-> DefaultVerifierBinding,
        workerContext |-> NoWorkerContext,
        workerAcceptance |-> NoWorkerAcceptance,
        invalidAttempt |-> FALSE,
        retryOutcomeKind |-> "none",
        retryAttempt |-> 0,
        humanInputOutstanding |-> FALSE,
        gateKind |-> "none",
        approvedTargetNodes |-> {},
        approvedCorrFingerprints |-> [n \in {} |-> NoFingerprint],
        currentPaperApprovedFp |-> DefaultTargetFp,
        coarseDagNodes |-> {}
    ]

NoPendingTask ==
    [
        taskBlockers |-> {},
        node |-> NoNode,
        mode |-> "global",
        orphanCleanupNodes |-> {},
        nextWorkerContextMode |-> "resume",
        paperFocusRanges |-> << >>,
        workStyleHint |-> "none",
        allowNewObligations |-> TRUE,
        mustCloseActive |-> FALSE,
        authorizedNodes |-> {},
        consumedGlobalRepairGrant |-> FALSE
    ]

\* Cleanup-v2 (2026-05-14): sentinel for `cleanupActiveTask` when no
\* cleanup worker burst is in flight. The kernel-side type is
\* `Option<u32>`. Modeled as a string sentinel because every use site
\* compares `cleanupActiveTask` only against `NoTask` itself (no
\* sentinel-vs-int union appears in TypeOK), so the string-vs-int TLC
\* trap that motivated migrating `NoCycle` to `-1` does not apply here.
NoTask == "NoTask"

\* StuckMathAudit (2026-05-31): sentinel for `lastStuckMathAuditDispatchedCycle`
\* (and the reused-since global_repair_mode siblings
\* `latestGlobalRepairAuditDeclineCycle` /
\* `lastReviewerGlobalRepairRequestCycle`) when the underlying kernel-side
\* `Option<u32>` is `None`. Modeled as the integer `-1` rather than a
\* string sentinel so the TypeOK clause `\in ({NoCycle} \cup 0..MaxCycle)`
\* yields a homogeneous integer set. TLC otherwise pairwise-equality-checks
\* the string `"NoCycle"` against each integer in `0..MaxCycle` and aborts
\* with `Attempted to check equality of string "NoCycle" with non-string: 0`.
\* (Mirrors the records-set-constructor fix on `NeedInputAuditContextValues`
\* for the same TLC trap on a sentinel-vs-record union.) The module
\* EXTENDS `Integers` (rather than `Naturals`) precisely to make `-1`
\* well-formed; do not regress to a string here.
NoCycle == -1

\* StuckMathAudit (2026-05-31): sentinel for `stuckMathAuditNeedInputAudit`
\* when no NeedInput-routed audit is queued (kernel
\* `stuck_math_audit.need_input_audit = None`). The kernel-side type is
\* `Option<NeedInputAuditContext>` carrying phase / active_node /
\* held_target / mode / reviewer_reason / reviewer_comments /
\* review_request_id / review_cycle / gate_from_invalid_attempt; the
\* spec abstracts to the single load-bearing field
\* `gateFromInvalidAttempt` (which propagates into the HumanGate's
\* `gateFromInvalidAttempt` on the dispatch-to-HumanGate arm of
\* `apply_stuck_math_audit_response` AND on the retry-exhausted arm of
\* `retry_or_transition_stuck_math_audit_to_reviewer`). All other fields
\* are pure reviewer-context bookkeeping the audit role consumes but
\* never returns through to the protocol state machine.
NoNeedInputAuditContext == "NoNeedInputAuditContext"
\* Use the records-set constructor `[field: Set, ...]` rather than a
\* set-comprehension `{[field |-> b] : b \in S}`. Both denote the same
\* TLA+ value, but TLC handles `\in` on the records-set form
\* structurally (DOMAIN match + per-field membership) and short-
\* circuits on non-records like the string sentinel, whereas the
\* comprehension form forces enumeration and TLC then tries to
\* compare the string sentinel against each record, raising
\* `Attempted to check equality of string ... with non-string`. The
\* sibling sentinels in this module (`SoundReverificationContextValues`,
\* `ProtectedSemanticChangeConfirmationValues`, `AuditPlanValues`)
\* already use the records-set form.
NeedInputAuditContextValues ==
    {NoNeedInputAuditContext}
    \cup
    [gateFromInvalidAttempt: BOOLEAN]

NodeObject(n) == [otype |-> "node", node |-> n]
TargetObject(t) == [otype |-> "target", target |-> t]
\* Per kernel `BlockerObject::Deviation { deviation }` (model.rs:478).
\* The third object variant; otype "deviation" with a deviation-id key.
DeviationObject(id) == [otype |-> "deviation", deviation |-> id]

Blocker(kind, obj, fp) == [kind |-> kind, object |-> obj, fp |-> fp]

BlockerUniverse ==
    {Blocker("node_corr", NodeObject(n), fp) : n \in Nodes, fp \in Fingerprints}
    \cup
    {Blocker("paper_faithfulness", TargetObject(t), fp) : t \in Targets, fp \in Fingerprints}
    \cup
    \* Substantiveness blockers (theorem-stating + proof-formalization).
    \* Distinct from the target-bound `paper_faithfulness` variant above:
    \* this one is node-bound. Bumped 2026-04-29 alongside contract v23;
    \* widened to proof-formalization 2026-04-29 so Hard-restructure helpers
    \* are checked.
    {Blocker("substantiveness", NodeObject(n), fp) : n \in Nodes, fp \in Fingerprints}
    \cup
    {Blocker("soundness", NodeObject(n), fp) : n \in Nodes, fp \in Fingerprints}
    \cup
    \* Deviation-authorization blockers (kernel
    \* `BlockerKind::Deviation`, model.rs:458-461). One per deviation id,
    \* fingerprinted by the live deviation file content. Carrier is
    \* `DeviationObject(id)`. See `GlobalBlockers` / `DeviationBlockersFor`
    \* for the membership rule (any deviation in `deviationFiles` whose
    \* `CurrentDeviationState` is not Pass).
    {Blocker("deviation", DeviationObject(id), fp) : id \in Deviations, fp \in Fingerprints}

BlockerChoiceId(b) ==
    b.kind \o ":" \o b.object.otype \o ":" \o
        (IF b.object.otype = "node" THEN b.object.node
         ELSE IF b.object.otype = "target" THEN b.object.target
         ELSE b.object.deviation)
        \o ":" \o b.fp

BlockerChoice(b) ==
    [id |-> BlockerChoiceId(b), blocker |-> b]

BlockerChoiceUniverse ==
    {BlockerChoice(b) : b \in BlockerUniverse}

BlockerChoiceIdUniverse ==
    {BlockerChoiceId(b) : b \in BlockerUniverse}

TargetSetNeighbors(ts) ==
    {ts}
    \cup
    {ts \cup {t} : t \in (Targets \ ts)}
    \cup
    {ts \ {t} : t \in ts}

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
    \* Deviation lane state surface (2026-05-27/28, kernel commits
    \* 7aad7cb / 4e83783 / efaafa7 / 4abe9dd). Each per-deviation map
    \* is keyed by an id from `deviationFiles`'s domain. The kernel
    \* maintains these as `BTreeMap<DeviationId, ..>` and prunes on
    \* deletion; the spec carries the same shape.
    \*   deviationFiles[id]: TRUE iff the deviation is alive (the
    \*                       kernel's BTreeMap has an entry). Absence
    \*                       (FALSE) means deleted or never existed.
    \*   deviationStatus[id]: lane verdict (pass/fail/unknown). Only
    \*                       meaningful when deviationFiles[id]; sticky
    \*                       Fail semantics are encoded in the helper
    \*                       `CurrentDeviationState`.
    \*   deviationCurrentFp[id]: TeX content fingerprint observed
    \*                       in the live snapshot. Empty (NoFingerprint
    \*                       analogue) means file missing/unreadable —
    \*                       treated as Unknown by the kernel.
    \*   deviationApprovedFp[id]: fingerprint pinned at the last
    \*                       verifier verdict; sticky-Fail keys on
    \*                       equality with the live current fp.
    \*   nodeDeviationClaims[n]: subset of Deviations the node declares
    \*                       it relies on. Drives the substantiveness
    \*                       short-circuit and the substantiveness
    \*                       fingerprint (kernel
    \*                       runtime_cli_observations.rs:2128-2141).
    deviationFiles,
    committedDeviationFiles,
    deviationStatus,
    deviationCurrentFp,
    committedDeviationCurrentFp,
    deviationApprovedFp,
    nodeDeviationClaims,
    committedNodeDeviationClaims,
    \* LastClean mirrors (2026-05-27/28, kernel commit 4e83783 +
    \* efaafa7). The `lastCleanDeviationStatus` migration shim (empty
    \* mirror but non-empty files maps to Unknown) is not modeled — it
    \* is a one-time migration concern.
    lastCleanDeviationFiles,
    lastCleanDeviationStatus,
    lastCleanDeviationApprovedFp,
    lastCleanNodeDeviationClaims,
    \* Reviewer-context scope for the deviation lane (kernel
    \* 4e83783 +
    \* model.rs:4943). `latestDeviationReviewIds` gates
    \* `ReviewBlockerAdjudicable` for Deviation blockers: an id is
    \* adjudicable only if its most recent verifier panel covered it.
    \* `latestDeviationReviewerEvidence` is keyed by lane id and is a
    \* stale-but-harmless cache after deletion (kernel comment at
    \* model.rs:5657); modeled as a flag pair rather than a per-lane
    \* map since the spec abstracts lane evidence as set membership.
    latestDeviationReviewIds,
    latestDeviationEvidenceLanes,
    nodeDifficulty,
    easyAttempts,
    reviewerComments,
    latestPaperEvidenceLanes,
    latestCorrEvidenceLanes,
    latestSoundEvidenceLanes,
    latestPaperReviewTargets,
    latestCorrReviewNodes,
    latestSoundReviewNodes,
    latestPaperPanelSplit,
    latestCorrPanelSplit,
    latestSoundPanelSplit,
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
    \* Cleanup-v2 audit lane (2026-05-14, kernel:
    \* CLAUDES_NOTES_cleanup_v2_impl_plan.md step 5).
    \* Audit + reviewer task-list state used during Phase::Cleanup.
    \* All default to empty/0/round-1/none in Init; reset on Cleanup
    \* phase re-entry via the kernel `enter_cleanup_phase` helper.
    cleanupAuditTasks,
    cleanupAuditScratchpad,
    cleanupAuditBurstCount,
    cleanupAuditRound,
    cleanupConsecutiveInvalidWorkers,
    cleanupActiveTask,
    cleanupForceDone,
    \* Cone-clean (2026-05-19, kernel d6e46e6): set TRUE by the kernel's
    \* `apply_audit_authorized_theorem_stating_node_reset`
    \* (engine.rs:433) when a StuckMathAudit response carries a
    \* `cone_clean_node`. Consumed in `StartCycle` to route the post-clean
    \* cycle through Review (kernel engine.rs:810). The StuckMathAudit
    \* role itself is unmodeled (pre-existing gap, see
    \* CLAUDES_NOTES_tla_drift_audit_2026-05-09.md), so no spec action
    \* currently sets this flag to TRUE; the variable is declared so the
    \* consumer-side semantics can be expressed and future syncs can wire
    \* in a producer.
    forceReviewAfterConeClean,
    \* Active coarse-DAG anchor (proposal v32). Set only while
    \* phase = "proof_formalization" AND coarseDagNodes # {}. Locked
    \* against change until ShallowlyClosedFromCoarse(activeCoarseNode)
    \* AND GlobalBlockers = {}, with a starvation-guard escape after
    \* cyclesInCoarseRepairMode >= StuckCoarseRepairThreshold. Cleared
    \* on phase exit and when an audit cone-clean reset targets the
    \* active anchor.
    activeCoarseNode,
    \* Starvation guard counter for CoarseRepairMode (proposal v32).
    \* Increments on every cycle where CoarseRepairMode holds; reset to
    \* 0 when activeCoarseNode changes or when CoarseRepairMode flips
    \* false. Bound: when >= StuckCoarseRepairThreshold, the kernel
    \* opens ActiveCoarseChangeAllowed even without strict shallow
    \* closure, to prevent indefinite blocker-chain drift.
    cyclesInCoarseRepairMode,
    \* StuckMathAudit dispatch state (kernel `StuckMathAuditState` in
    \* model.rs + `NeedInputAuditContext`). Latched TRUE by
    \* `route_need_input_to_auditor` (engine.rs) when a Reviewer
    \* `NEED_INPUT` decision is seen, and by the TheoremStating
    \* Sound-stagnation preempt in `start_cycle`. Cleared by
    \* `apply_stuck_math_audit_response` (success/retry-exhaust paths)
    \* and by `retry_or_transition_stuck_math_audit_to_reviewer`.
    \*
    \*   stuckMathAuditActive: TRUE iff the latch is on. Mirrors
    \*       `StuckMathAuditState::active`.
    \*   stuckMathAuditNeedInputAudit: NoNeedInputAuditContext when no
    \*       NeedInput escalation is queued; otherwise a record carrying
    \*       the fields needed when the audit routes back to HumanGate.
    \*       The kernel's `NeedInputAuditContext` carries phase/active/
    \*       held/mode/review-id/cycle/reviewer-reason/reviewer-comments;
    \*       the spec abstracts to the load-bearing `gateFromInvalidAttempt`
    \*       since that is what `apply_stuck_math_audit_response` and
    \*       `retry_or_transition_stuck_math_audit_to_reviewer` actually
    \*       consume.
    \*   stuckMathAuditBurstRetryCount: 0..STUCK_MATH_AUDIT_BURST_RETRY_LIMIT.
    \*       Kernel constant is 1; the spec carries 0..1 explicitly so
    \*       the retry / retry-exhausted branch is observable.
    \*   lastStuckMathAuditDispatchedCycle: cycle of the last
    \*       StuckMathAudit dispatch (NoCycle sentinel when never
    \*       dispatched). Mirrors `last_stuck_math_audit_dispatched_cycle`.
    \*       Used by the dispatch-cooldown gate (not modeled by traces
    \*       today but the variable is declared for symmetry).
    stuckMathAuditActive,
    stuckMathAuditNeedInputAudit,
    stuckMathAuditBurstRetryCount,
    lastStuckMathAuditDispatchedCycle,
    \* global_repair_mode state additions (kernel model.rs Step 1+3).
    \* pendingGlobalRepairRequest: NoGlobalRepairRequest sentinel when
    \*     no Step A request is pending; otherwise a record with the
    \*     reviewer-proposed extension nodes + bookkeeping cycles.
    \* pendingGlobalRepairGrant: NoGlobalRepairGrant sentinel when no
    \*     Step B grant is live; otherwise a record carrying the
    \*     auditor-approved extension nodes.
    \* latestGlobalRepairAuditDeclineReason: empty string when no
    \*     decline is in scope; else the auditor's most recent reason.
    \* latestGlobalRepairAuditDeclineCycle: NoCycle sentinel or the
    \*     cycle the decline was recorded.
    \* lastReviewerGlobalRepairRequestCycle: NoCycle sentinel or the
    \*     cycle of the most recent Step A acceptance (S10 cooldown).
    \* everShallowCoarseClosed: subset of Nodes; monotone history of
    \*     coarse nodes observed shallow-closed against committed.
    \* globalRepairModeEnabled: BOOLEAN kill-switch.
    \*
    \* New actions to mirror in a follow-up TLA pass:
    \*     RequestGlobalRepairAudit (Step A reviewer Continue),
    \*     ConsumeGlobalRepairGrant (Step C reviewer Continue),
    \*     ApplyStuckMathAuditGlobalRepairResponse (Step B audit).
    \* The kernel implementation is the source of truth; the spec
    \* declares the variables now so TypeOK references them, and a
    \* subsequent TLC sync (tracked in SPEC_TODO.md) wires the
    \* actions. The current run mirror via tla_replay covers the
    \* pre-existing actions.
    pendingGlobalRepairRequest,
    pendingGlobalRepairGrant,
    latestGlobalRepairAuditDeclineReason,
    latestGlobalRepairAuditDeclineCycle,
    lastReviewerGlobalRepairRequestCycle,
    everShallowCoarseClosed,
    globalRepairModeEnabled,
    \* Post-advance routing latch (kernel model.rs
    \* `ProtocolState::post_advance_routing_pending`). Set TRUE by
    \* `apply_human_gate_response` GateKind::Advance::Approve; cleared by
    \* the next `start_cycle` after dispatching the routing Reviewer (and
    \* by `apply_proof_review_response` on any review response that
    \* reaches it). Forces the first burst after a human-approved phase
    \* advance to be a routing Reviewer rather than a worker dispatch.
    postAdvanceRoutingPending,
    \* Protected-target reapproval (kernel model.rs
    \* `ProtocolState::pending_protected_reapproval_nodes` and
    \* `pending_protected_semantic_scope_confirmation`). Producer is
    \* `maybe_issue_protected_reapproval` (engine.rs) which sets the node
    \* set and emits a HumanGate of kind `protected_reapproval`; consumer
    \* is `apply_human_gate_response` GateKind::ProtectedReapproval which
    \* clears both fields on approve and routes Approve back into either
    \* ProofFormalization (with structural / target snapshot freeze) or
    \* Cleanup (when formalization is complete).
    \*
    \*   pendingProtectedReapprovalNodes: subset of Nodes whose target
    \*       protected closures require re-approval after a structural
    \*       change. Empty means no reapproval gate is pending.
    \*   pendingProtectedSemanticScopeConfirmation:
    \*       NoProtectedSemanticChangeConfirmation when no scope-
    \*       confirmation context is queued; else a record carrying the
    \*       fields `apply_proof_review_response`'s reissue path needs
    \*       (`maybe_reissue_protected_semantic_scope_confirmation`).
    pendingProtectedReapprovalNodes,
    pendingProtectedSemanticScopeConfirmation,
    \* StuckMathAudit `audit_plan` lane (kernel model.rs
    \* `ProtocolState::audit_plan` and `superseded_audit_plan`).
    \* Produced by `apply_stuck_math_audit_response` on a Valid response
    \* (carries the auditor's report / tasks / probe_paths / optional
    \* cone-clean node). Consumed by `apply_review_audit_plan_actions`
    \* in the reviewer cycle: individual task dismissals stamp
    \* `dismissed=true`; whole-plan dismissal moves the plan into
    \* `superseded_audit_plan` and clears the StuckMathAudit latch.
    \*
    \*   auditPlan: NoAuditPlan when no plan is on the table; else a
    \*       record with `tasks` (sequence of audit-task records, each
    \*       with id, dismissed flag, dismissed_reason) and `coneClean`
    \*       (Option<NodeId>, NoNode when absent).
    \*   supersededAuditPlan: NoAuditPlan or a prior plan retained for
    \*       audit-trail purposes after `dismiss_audit_plan = true`.
    auditPlan,
    supersededAuditPlan,
    \* Sound assessment taxonomy (kernel model.rs
    \* `ProtocolState::sound_assessments`,
    \* `reviewer_requested_sound_verifier_nodes`, plus the
    \* per-request `WrapperRequest::sound_reverification_context`
    \* surfaced here as standing state). The legacy `soundStatus` map
    \* (four-value SoundStates) is the engagement view; this rich
    \* taxonomy is the kernel store that distinguishes verifier
    \* verdicts from reviewer pins from drift-induced staleness.
    \*
    \*   soundAssessmentStatus: total map Nodes ->
    \*       SoundAssessmentStatuses. Default "fresh_unknown" for
    \*       nodes whose stored assessment is absent; the rich values
    \*       are produced by VerifierPanel writes, reviewer Sound pin,
    \*       and lazy `current_sound_assessment()` drift detection.
    \*   reviewerRequestedSoundVerifierNodes: subset of Nodes the
    \*       reviewer named via `request_sound_verifier_nodes` and
    \*       which the kernel held pending dispatch. Cleared on Sound
    \*       acceptance for the dispatched node.
    \*   soundReverificationContext: the per-request facts surfaced to
    \*       the Sound verifier when the request target's assessment
    \*       is `dep_edit_only_stale_pass_deferred` or
    \*       `self_edit_unknown`. Sentinel
    \*       `NoSoundReverificationContext` otherwise.
    soundAssessmentStatus,
    reviewerRequestedSoundVerifierNodes,
    soundReverificationContext,
    inFlightRequest,
    response

Vars ==
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
        latestPaperReviewTargets,
        latestCorrReviewNodes,
        latestSoundReviewNodes,
        latestPaperPanelSplit,
        latestCorrPanelSplit,
        latestSoundPanelSplit,
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
        inFlightRequest,
        response
    >>

\* NOTE: deviation lane variables were intentionally excluded from
\* `VarsWithoutRequest` so that the deviation env-driven mutator
\* actions can fire concurrently with Issue/EnvStage/Accept actions
\* that also `UNCHANGED VarsWithoutRequest`. Callers that explicitly
\* want deviation state preserved (i.e., every non-deviation action)
\* should add `UNCHANGED DeviationVars` separately.
VarsWithoutRequest ==
    <<
        phase,
        stage,
        cycle,
        attempt,
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
        nodeDifficulty,
        easyAttempts,
        reviewerComments,
        latestPaperEvidenceLanes,
        latestCorrEvidenceLanes,
        latestSoundEvidenceLanes,
        latestPaperReviewTargets,
        latestCorrReviewNodes,
        latestSoundReviewNodes,
        latestPaperPanelSplit,
        latestCorrPanelSplit,
        latestSoundPanelSplit,
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
        response
    >>

CurrentStructureVars ==
    <<
        currentProofNodes,
        currentNodeKinds,
        currentDeps,
        currentTargetClaims
    >>

\* Cleanup-v2 (2026-05-14): tuple alias for the new audit/task-list
\* state added in step 5 of CLAUDES_NOTES_cleanup_v2_impl_plan.md.
\* Every existing action that doesn't mutate cleanup-v2 state should
\* include `/\ UNCHANGED CleanupV2Vars`. Actions that mutate any
\* cleanup-v2 variable must enumerate the OTHERS in UNCHANGED.
\* Until step 7+ adds the audit / reviewer-cleanup actions, all
\* existing actions hold these UNCHANGED.
CleanupV2Vars ==
    <<
        cleanupAuditTasks,
        cleanupAuditScratchpad,
        cleanupAuditBurstCount,
        cleanupAuditRound,
        cleanupConsecutiveInvalidWorkers,
        cleanupActiveTask,
        cleanupForceDone
    >>

\* Active-coarse-anchor cluster (proposal v32). Same pattern as
\* CleanupV2Vars: every existing action that doesn't mutate the
\* coarse-anchor state should include `/\ UNCHANGED CoarseAnchorVars`.
\* Only ReviewContinueProof, the proof-formalization phase-entry
\* action, the cleanup-phase-entry action, and the cone-clean reset
\* action mutate these vars.
CoarseAnchorVars ==
    <<
        activeCoarseNode,
        cyclesInCoarseRepairMode
    >>

\* StuckMathAudit producer cluster (2026-05-31). Mirrors kernel
\* `StuckMathAuditState` (model.rs) + `NeedInputAuditContext` +
\* `stuck_math_audit_burst_retry_count` +
\* `last_stuck_math_audit_dispatched_cycle`. Same pattern as
\* CleanupV2Vars / CoarseAnchorVars: every existing action that
\* doesn't mutate the latch should include
\* `/\ UNCHANGED StuckMathAuditVars`. Actions that mutate any one of
\* these vars must enumerate the others in UNCHANGED. Latch mutators
\* are `ReviewNeedInputAfterInvalid` /
\* `ReviewNeedInputAfterValid` / `ReviewNeedInputProof` /
\* `ReviewNeedInputCleanup` (set TRUE via `route_need_input_to_auditor`),
\* `IssueStuckMathAuditRequest` (dispatch retry),
\* `AcceptStuckMathAuditDispatchHumanGate` /
\* `AcceptStuckMathAuditBackToReviewer` /
\* `AcceptStuckMathAuditRetry` /
\* `AcceptStuckMathAuditRetryExhausted*` (clear/decrement).
StuckMathAuditVars ==
    <<
        stuckMathAuditActive,
        stuckMathAuditNeedInputAudit,
        stuckMathAuditBurstRetryCount,
        lastStuckMathAuditDispatchedCycle
    >>

\* global_repair_mode cluster (2026-06-05). Mirrors the new kernel
\* fields added in `model.rs` Steps 1+3 of the global_repair_mode
\* implementation plan. Lifecycle of `pendingGlobalRepairRequest`:
\* set by Step A (`RequestGlobalRepairAudit`), cleared by Step B
\* (`ApplyStuckMathAuditGlobalRepairResponse`). Lifecycle of
\* `pendingGlobalRepairGrant`: set by Step B approve, cleared on
\* Step C burst acceptance OR on Step A re-dispatch OR by TTL.
\* `everShallowCoarseClosed` is the monotone refresh tracked in
\* `commit_live` against the COMMITTED baseline (S8 invariant).
\* `globalRepairModeEnabled` is the kill-switch.
\*
\* Every existing action that doesn't mutate these vars should
\* include `/\ UNCHANGED GlobalRepairVars`. The mutators are exactly
\* `RequestGlobalRepairAudit` (Step A — sets request, dispatches),
\* `ApplyStuckMathAuditGlobalRepairResponse` (Step B — clears
\* request, optionally sets grant + decline reason), and
\* `ConsumeGlobalRepairGrant` (Step C — clears grant). All other
\* StuckMathAudit-mutator actions (`Issue*`, `AcceptStuckMathAudit*`,
\* `ReviewNeedInput*`) UNCHANGE the global_repair cluster explicitly.
GlobalRepairVars ==
    <<
        pendingGlobalRepairRequest,
        pendingGlobalRepairGrant,
        latestGlobalRepairAuditDeclineReason,
        latestGlobalRepairAuditDeclineCycle,
        lastReviewerGlobalRepairRequestCycle,
        everShallowCoarseClosed,
        globalRepairModeEnabled
    >>

\* Post-advance routing latch cluster. Singleton, but presented as a
\* cluster alias for symmetry with the other recently-added singletons.
\* Mutators: `HumanApproveAdvance` (set TRUE), `StartCycle` (cleared on
\* the routing-Review dispatch), `ReviewContinueProof` /
\* `ReviewNeedInputProof` / `ReviewContinueCleanup` (cleared on any
\* review response that reaches `apply_proof_review_response`). All
\* other actions UNCHANGED PostAdvanceRoutingVars.
PostAdvanceRoutingVars ==
    <<
        postAdvanceRoutingPending
    >>

\* Protected-target reapproval cluster. Mutators: producer
\* `MaybeIssueProtectedReapproval` (sets the node set; under certain
\* paths sets `pendingProtectedSemanticScopeConfirmation`), consumers
\* `HumanApproveProtectedReapproval` / `HumanFeedbackProtectedReapproval`
\* (clear both fields). All other actions UNCHANGED
\* ProtectedReapprovalVars.
ProtectedReapprovalVars ==
    <<
        pendingProtectedReapprovalNodes,
        pendingProtectedSemanticScopeConfirmation
    >>

\* StuckMathAudit `audit_plan` lane. Mutators: producer
\* `AcceptStuckMathAuditBackToReviewer` /
\* `AcceptStuckMathAuditDispatchHumanGate` (set `auditPlan` and move
\* prior plan to `supersededAuditPlan`),
\* `AcceptAuthorizedConeCleanReset` (cone-clean reset clears auditPlan
\* via the reset path's `clear_pending_task` / superseded-shuffle),
\* reviewer cycle `RecordAuditPlan` and `DismissAuditPlanTask` and the
\* whole-plan-dismissal arm of `ReviewContinueProof` (record dismissal,
\* move plan to `supersededAuditPlan`). All other actions UNCHANGED
\* AuditPlanVars.
AuditPlanVars ==
    <<
        auditPlan,
        supersededAuditPlan
    >>

\* Sound assessment cluster. Mirrors the kernel `sound_assessments`
\* map (model.rs) and the request-time `sound_reverification_context`
\* / `reviewer_requested_sound_verifier_nodes` fields whose lifecycle
\* the spec must observe.
\*
\* Mutators:
\*   - VerifierPanel acceptance (Sound verifier verdict):
\*     `verifier_pass` / `verifier_fail` / `verifier_structural` /
\*     `split_unknown`. Sites are the four sound-accept actions
\*     (`AcceptSoundPass` / `AcceptSoundFail` / `AcceptSoundStructural`
\*     / `AcceptSoundSplit`).
\*   - Reviewer pinning a Sound blocker via reset_blockers:
\*     `reviewer_pinned_fail`. Site is the Sound branch of
\*     `ApplyReviewSoundStatusResets` callers
\*     (`ReviewContinueProof` / `ReviewNeedInputProof`).
\*   - `RequestSoundVerifier` action: extends
\*     `reviewerRequestedSoundVerifierNodes` with eligible nodes the
\*     reviewer named in the response.
\*   - Sound dispatch acceptance: removes the dispatched node from
\*     `reviewerRequestedSoundVerifierNodes`.
\*   - Structural reset (lastClean / cone-clean):
\*     `soundAssessmentStatus` map reset to "fresh_unknown",
\*     `reviewerRequestedSoundVerifierNodes` cleared,
\*     `soundReverificationContext` cleared.
\*
\* `soundReverificationContext` is the abstract carry of the
\* per-request `WrapperRequest::sound_reverification_context` field;
\* the spec treats it as standing state (set when a Sound request is
\* about to be issued, cleared on acceptance / structural reset).
\*
\* All other actions UNCHANGED SoundAssessmentVars.
SoundAssessmentVars ==
    <<
        soundAssessmentStatus,
        reviewerRequestedSoundVerifierNodes,
        soundReverificationContext
    >>

\* Coarse-anchor S8 safety state. The kernel maintains
\* `ever_shallow_coarse_closed` (monotone history) as PROTOCOL state
\* but exposes `ever_shallow_coarse_closed_regressed()` as a derived
\* predicate computed against the COMMITTED snapshot. The spec follows
\* the same shape: `everShallowCoarseClosed` is the variable (already
\* part of GlobalRepairVars), `EverShallowCoarseClosedRegressed` is a
\* derived operator (defined below). No new state variable is needed
\* — the S8 anchor-change safety invariant
\* (`AnchorChangeForbiddenDuringGlobalRepair`) is checked from the
\* derived predicate.

\* Step A → Step B dispatch cooldown (model.rs
\* `stuck_math_audit_dispatch_cooldown_cycles`). The kernel uses 2
\* cycles by default; expressed here as an operator (not a CONSTANT)
\* to keep the sim.cfg surface stable. A tighter bound makes Step A
\* fire more often in TLC traces, which is desirable for coverage.
StuckMathAuditDispatchCooldownCycles == 2

\* Grant TTL in cycles (model.rs
\* `global_repair_grant_ttl_cycles`). Mirrored at the spec level
\* only for completeness — grant TTL expiry is handled by a kernel
\* checkpoint hook (commit_live) that the spec does not yet model.
\* Surfaced here as a named operator so the rationale is discoverable.
GlobalRepairGrantTtlCycles == 3

\* Deviation lane cluster (2026-05-27/28). Same pattern as the two
\* clusters above. Every existing action in CoreNext holds
\* `UNCHANGED DeviationVars` — deviation state evolves ONLY through
\* the new env-driven mutator actions (`WorkerEmitDeviationRequest`,
\* `EnvDeviationVerifierVerdict`, `EnvDeviationFingerprintDrift`,
\* `WorkerRetireDeviation`, `WorkerUpdateDeviationClaims`) at the
\* tail of CoreNext.
\*
\* This is a deliberate abstraction: the deviation lane has no
\* committed/restore semantics in the spec (those happen
\* atomically with kernel `commit_live` / `restore_committed`).
\* The invariants in the closing section still constrain the
\* lifecycle (sticky-Fail, claim hygiene, blocker membership,
\* override-empty under default).
\*
\* Sub-clusters (CommittedDeviationVars / LastCleanDeviationVars /
\* LatestDeviationReviewVars) are referenced by the new mutator actions
\* for partial-UNCHANGED clauses.
CommittedDeviationVars ==
    <<
        committedDeviationFiles,
        committedDeviationCurrentFp,
        committedNodeDeviationClaims
    >>

LastCleanDeviationVars ==
    <<
        lastCleanDeviationFiles,
        lastCleanDeviationStatus,
        lastCleanDeviationApprovedFp,
        lastCleanNodeDeviationClaims
    >>

LatestDeviationReviewVars ==
    <<
        latestDeviationReviewIds,
        latestDeviationEvidenceLanes
    >>

DeviationVars ==
    <<
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
        latestDeviationEvidenceLanes
    >>

\* Cleanup-v2 (2026-05-14): tuple alias of every spec variable EXCEPT
\* the cleanup-v2 audit/task-list cluster, `inFlightRequest`, `requestSeq`,
\* and `response`. The cleanup-v2 actions
\* (`AcceptCleanupAuditNeedToContinue`, `AcceptCleanupAuditDone`,
\* `ReviewerCleanupDismissAndDispatch`, `ReviewerCleanupReAudit`) mutate
\* some subset of {request vars, cleanup-v2 vars} and use this alias plus
\* explicit per-cleanup-v2-var UNCHANGED clauses to keep prime-assignment
\* contradictions out of the conjunction.
VarsExceptCleanupV2AndRequest ==
    <<
        phase,
        stage,
        cycle,
        attempt,
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
        latestPaperReviewTargets,
        latestCorrReviewNodes,
        latestSoundReviewNodes,
        latestPaperPanelSplit,
        latestCorrPanelSplit,
        latestSoundPanelSplit,
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
        forceReviewAfterConeClean,
        activeCoarseNode,
        cyclesInCoarseRepairMode,
        stuckMathAuditActive,
        stuckMathAuditNeedInputAudit,
        stuckMathAuditBurstRetryCount,
        lastStuckMathAuditDispatchedCycle,
        postAdvanceRoutingPending,
        pendingProtectedReapprovalNodes,
        pendingProtectedSemanticScopeConfirmation,
        auditPlan,
        supersededAuditPlan,
        soundAssessmentStatus,
        reviewerRequestedSoundVerifierNodes,
        soundReverificationContext
    >>

PromptCarryVars ==
    <<
        reviewerComments,
        latestPaperEvidenceLanes,
        latestCorrEvidenceLanes,
        latestSoundEvidenceLanes,
        latestPaperReviewTargets,
        latestCorrReviewNodes,
        latestSoundReviewNodes,
        latestPaperPanelSplit,
        latestCorrPanelSplit,
        latestSoundPanelSplit,
        previousPaperFindingLanes,
        previousCorrFindingLanes,
        previousSoundFindingLanes,
        latestSubstantivenessEvidenceLanes,
        latestSubstantivenessReviewNodes,
        latestSubstantivenessPanelSplit,
        previousSubstantivenessFindingLanes
    >>

\* The protocol models support abstractly as a current generated support
\* surface derived from the current repo structure before any support-requiring
\* request or checker result is exposed. In the deployed runtime this includes
\* the generated `Tablet.lean` import surface, the repo-local support docs
\* (`Tablet/INDEX.md`, `Tablet/README.md`, `Tablet/header.tex`), the compiled
\* external dependency/package artifacts needed to resolve imports such as
\* `Mathlib`, and the local compiled Tablet module artifacts needed by
\* compilation-based checks. The
\* protocol therefore models only post-sync states: whenever support is
\* required, it is already current for the present node set.
SupportFilesAvailable == TRUE

SupportRequiredRequestKinds ==
    \* Mirror of kernel `RequestKind::requires_runtime_support`.
    \* Cleanup-v2 audit + StuckMathAudit roles both require runtime
    \* support (they read the live tablet and depend on the runtime's
    \* prepared support surface).
    {"worker", "paper", "corr", "sound", "review", "audit", "stuck_math_audit"}

\* `RequestSupportReady` models externally dispatchable requests/checks. The
\* deployed runtime may allocate an internal request object first and then
\* re-establish support in the same runtime step before exposing that request
\* to the wrapper.
RequestSupportReady(kind) ==
    IF kind \in SupportRequiredRequestKinds THEN
        SupportFilesAvailable
    ELSE
        TRUE

CommittedStructureVars ==
    <<
        committedProofNodes,
        committedNodeKinds,
        committedDeps,
        committedTargetClaims
    >>

AllStructureVars ==
    <<
        currentProofNodes,
        committedProofNodes,
        currentNodeKinds,
        committedNodeKinds,
        currentDeps,
        committedDeps,
        currentTargetClaims,
        committedTargetClaims
    >>

\* Patch C (LOCAL_CLOSURE_IMPL_PLAN.md §7.2): closure tier — live and
\* committed. Used as an UNCHANGED shorthand in actions that don't
\* mutate closure state. Worker-acceptance actions and Reviewer/Reset
\* actions explicitly bind these primes; everything else
\* (Issue*, EnvStage*, Accept*verifier* artifact, etc.) UNCHANGES them.
LocalClosureVars ==
    <<
        localClosureUnverified,
        committedLocalClosureUnverified
    >>

CoverageFromClaims(claimMap, livePresent, liveTargets) ==
    [t \in Targets |->
        IF t \in liveTargets THEN
            {n \in livePresent : t \in claimMap[n]}
        ELSE
            {}
    ]

ProofNodesFromKinds(kindMap, livePresent) ==
    {n \in livePresent : kindMap[n] = "proof"}

DepClosure(seed, livePresent) ==
    {
        n \in livePresent :
            \A s \in SUBSET livePresent:
                (seed \subseteq s /\ \A m \in s: currentDeps[m] \subseteq s) => n \in s
    }

ReverseDepClosure(seed, livePresent) ==
    {
        n \in livePresent :
            \A s \in SUBSET livePresent:
                (seed \subseteq s /\ \A m \in s: {k \in livePresent : m \in currentDeps[k]} \subseteq s) => n \in s
    }

ImpactRegion(node, livePresent) ==
    IF node \in livePresent THEN
        DepClosure({node}, livePresent) \cup ReverseDepClosure({node}, livePresent)
    ELSE
        {}

TargetSupportCone(t, liveCoverage, livePresent) ==
    DepClosure(liveCoverage[t], livePresent)

\* Down-cone of a single coarse-DAG node: the node plus its transitive
\* import dependencies. Used by the active-coarse-anchor mechanism
\* (proposal v32). NOTE: do NOT use `TargetSupportCone` here even
\* though both spec types are aliased to STRING -- TargetSupportCone
\* keys into coverage by TargetId and would silently misbehave on a
\* NodeId argument (see kernel audit, model.rs:6080 regression note).
CoarseNodeSupportCone(node, livePresent) ==
    IF node \in livePresent THEN DepClosure({node}, livePresent) ELSE {}

\* Mirrors kernel model.rs:1752 shallowly_closed_from_coarse. A coarse
\* node is shallowly closed iff every transitive non-coarse dep is
\* present and not open; other coarse-DAG members are skipped
\* regardless of their own closure state.
ShallowlyClosedFromCoarse(node, livePresent, liveOpen, coarse) ==
    /\ node \in livePresent
    /\ node \notin liveOpen
    /\ \A m \in DepClosure({node}, livePresent) \ coarse :
           m \in livePresent /\ m \notin liveOpen

ShallowlyClosedCoarseNodes(livePresent, liveOpen, coarse) ==
    {n \in coarse :
        ShallowlyClosedFromCoarse(n, livePresent, liveOpen, coarse)}

TargetCoveringNodes(liveCoverage) ==
    UNION {liveCoverage[t] : t \in configuredTargets}

OrphanNodes(liveCoverage, livePresent) ==
    {n \in livePresent :
        /\ n # "Preamble"
        /\ n \notin DepClosure(TargetCoveringNodes(liveCoverage), livePresent)}

OrphanCleanupNeeded ==
    OrphanNodes(currentCoverage, presentNodes) # {}

OrphanCleanupActive ==
    pendingTask.orphanCleanupNodes # {}

OrphanCleanupPendingTask(node, mode, liveCoverage, livePresent) ==
    [
        taskBlockers |-> {},
        node |-> node,
        mode |-> mode,
        orphanCleanupNodes |-> OrphanNodes(liveCoverage, livePresent),
        nextWorkerContextMode |-> "resume",
        paperFocusRanges |-> << >>,
        workStyleHint |-> "restructure",
        allowNewObligations |-> TRUE,
        mustCloseActive |-> FALSE,
        authorizedNodes |-> {},
        consumedGlobalRepairGrant |-> FALSE
    ]

CurrentMode ==
    IF phase = "theorem_stating" THEN
        targetEditMode
    ELSE IF phase = "proof_formalization" THEN
        proofEditMode
    ELSE
        "cleanup"

CurrentActiveDifficulty ==
    IF OrphanCleanupActive THEN
        "hard"
    ELSE IF activeNode = NoNode THEN
        "hard"
    ELSE
        nodeDifficulty[activeNode]

CurrentActiveEasyAttempts ==
    IF activeNode = NoNode THEN
        0
    ELSE
        easyAttempts[activeNode]

CurrentWorkerProfile ==
    IF OrphanCleanupActive THEN
        "cleanup"
    ELSE IF phase = "theorem_stating" THEN
        "theorem"
    ELSE IF phase = "proof_formalization" THEN
        IF CurrentActiveDifficulty = "easy" THEN "proof_easy" ELSE "proof_hard"
    ELSE IF phase = "cleanup" THEN
        "final_cleanup"
    ELSE
        "none"

CurrentWorkerValidationKind ==
    IF OrphanCleanupActive THEN
        IF phase \in {"theorem_stating", "proof_formalization", "cleanup"} THEN
            "cleanup"
        ELSE
            "none"
    ELSE IF phase = "theorem_stating" THEN
        IF targetEditMode = "targeted" THEN
            "theorem_targeted"
        ELSE
            "theorem_global"
    ELSE IF phase = "proof_formalization" THEN
        IF proofEditMode = "restructure" THEN
            "proof_restructure"
        ELSE IF proofEditMode = "coarse_restructure" THEN
            "proof_coarse_restructure"
        ELSE
            "proof_local"
    ELSE IF phase = "cleanup" THEN
        "final_cleanup"
    ELSE
        "none"

CurrentWorkerAllowNewObligations ==
    IF phase = "proof_formalization" THEN
        pendingTask.allowNewObligations
    ELSE
        TRUE

CurrentWorkerMustCloseActive ==
    IF phase = "proof_formalization" THEN
        pendingTask.mustCloseActive
    ELSE
        FALSE

\* `ActiveNodeLegal` is defined further down (after `GlobalBlockers`),
\* because its proof-formalization branch references
\* `ProofNodeRepairsBlocker` and `ProofNodeRepairsAggregateNodeBlockers`,
\* both of which depend on the blocker-derivation operators introduced
\* by the `GlobalBlockers` block.

HeldTargetLegal(node, livePresent, liveOpen) ==
    node = NoNode
    \/
    /\ node \in livePresent
    /\ node \in liveOpen
    /\ node \in currentProofNodes

PresentClosed(livePresent) ==
    \A n \in livePresent: currentDeps[n] \subseteq livePresent

LeafPresentNodes(livePresent) ==
    {n \in livePresent : \A m \in livePresent: n \notin currentDeps[m]}

PresentNeighbors(livePresent) ==
    {livePresent}
    \cup
    {livePresent \cup {n} : n \in {m \in (Nodes \ livePresent) : currentDeps[m] \subseteq livePresent}}
    \cup
    {livePresent \ {n} : n \in LeafPresentNodes(livePresent)}

OpenNeighbors(livePresent, liveOpen) ==
    {liveOpen}
    \cup
    {liveOpen \cup {n} : n \in (livePresent \ liveOpen)}
    \cup
    {liveOpen \ {n} : n \in liveOpen}

\* Patch C (LOCAL_CLOSURE_IMPL_PLAN.md §7.3): closure-unverified state
\* update neighborhood. `priorUnverified` is the pre-action live tier.
\* Post-action set must satisfy the §7.2 invariant: subset of
\* present∩proof, disjoint from openNodes.
\*
\* The neighborhood is bounded to {prior ∩ eligible} ∪ {add-one-node}
\* ∪ {remove-one-node} so TLC's state space stays tractable. This
\* mirrors `OpenNeighbors`'s shape and is a faithful abstraction of
\* the kernel's per-burst invalidation behaviour: typical burst
\* changes one helper, invalidating ≤1 consumer at a time; probes
\* refresh ≤1 node per pass (§7.5 chunking, MAX_PRE_REVIEW=10).
\* Modeling unbounded delta would balloon TLC without strengthening
\* the structural property `StalePassClosurePreventsCleanupTransition`.
LocalClosureUnverifiedNeighbors(livePresent, liveOpen, liveProofNodes) ==
    LET eligible == (livePresent \cap liveProofNodes) \ liveOpen
        prior == localClosureUnverified \cap eligible
    IN
    {prior}
    \cup
    {prior \cup {n} : n \in (eligible \ prior)}
    \cup
    {prior \ {n} : n \in prior}

CoverageNeighbors(liveCoverage, livePresent) ==
    LET baseCoverage == [t \in Targets |-> liveCoverage[t] \cap livePresent]
    IN
    {baseCoverage}
    \cup
    UNION
    {
        {[baseCoverage EXCEPT ![t] = @ \cup {n}] : n \in (livePresent \ baseCoverage[t])}
        : t \in Targets
    }
    \cup
    UNION
    {
        {[baseCoverage EXCEPT ![t] = @ \ {n}] : n \in baseCoverage[t]}
        : t \in Targets
    }

FpNeighbors(fpMap) ==
    {fpMap}
    \cup
    {[fpMap EXCEPT ![n] = fp] : n \in Nodes, fp \in Fingerprints}

TargetFpNeighbors(fpMap) ==
    {fpMap}
    \cup
    {[fpMap EXCEPT ![t] = fp] : t \in Targets, fp \in Fingerprints}

CorrUpdateNeighbors ==
    {DefaultCorrUpdate}
    \cup
    {[DefaultCorrUpdate EXCEPT ![n] = s] : n \in Nodes, s \in CorrUpdates}

PaperUpdateNeighbors ==
    {DefaultPaperUpdate}
    \cup
    {[DefaultPaperUpdate EXCEPT ![t] = s] : t \in Targets, s \in CorrUpdates}

SoundUpdateNeighbors ==
    {DefaultSoundUpdate}
    \cup
    {[DefaultSoundUpdate EXCEPT ![n] = s] : n \in Nodes, s \in SoundUpdates}

BaseCorrRawReport ==
    [
        corrFailNodes |-> {},
        preambleItemFail |-> FALSE,
        targetFailIds |-> {}
    ]

CorrRawReportNeighbors ==
    {BaseCorrRawReport}
    \cup
    {[BaseCorrRawReport EXCEPT !.corrFailNodes = {n}] : n \in Nodes}
    \cup
    {[BaseCorrRawReport EXCEPT !.preambleItemFail = TRUE]}
    \cup
    {[BaseCorrRawReport EXCEPT !.targetFailIds = {t}] : t \in Targets}
    \cup
    {
        [BaseCorrRawReport EXCEPT !.corrFailNodes = {n}, !.targetFailIds = {t}]
        : n \in Nodes, t \in Targets
    }
    \cup
    {[BaseCorrRawReport EXCEPT !.preambleItemFail = TRUE, !.targetFailIds = {t}] : t \in Targets}

SoundRawDecisions == {"sound", "unsound", "structural"}

SoundRawReportNeighbors ==
    {[decision |-> d] : d \in SoundRawDecisions}

CorrNodeLaneMapFromReport(report, verifyNodes) ==
    [n \in Nodes |->
        IF n \in verifyNodes THEN
            IF n \in report.corrFailNodes \/ (n = "Preamble" /\ report.preambleItemFail) THEN
                "fail"
            ELSE
                "pass"
        ELSE
            "same"
    ]

CorrTargetLaneMapFromReport(report, verifyTargets) ==
    [t \in Targets |->
        IF t \in verifyTargets THEN
            IF t \in report.targetFailIds THEN "fail" ELSE "pass"
        ELSE
            "same"
    ]

SoundLaneMapFromReport(report, verifyNodes) ==
    [n \in Nodes |->
        IF n \in verifyNodes THEN
            IF report.decision = "sound" THEN
                "pass"
            ELSE IF report.decision = "structural" THEN
                "structural"
            ELSE
                "fail"
        ELSE
            "same"
    ]

CorrNodeLaneMapsFromReports(laneReports, verifyNodes) ==
    [l \in VerifierLanes |-> CorrNodeLaneMapFromReport(laneReports[l], verifyNodes)]

CorrTargetLaneMapsFromReports(laneReports, verifyTargets) ==
    [l \in VerifierLanes |-> CorrTargetLaneMapFromReport(laneReports[l], verifyTargets)]

SoundLaneMapsFromReports(laneReports, verifyNodes) ==
    [l \in VerifierLanes |-> SoundLaneMapFromReport(laneReports[l], verifyNodes)]

CorrLaneReportDisagreement(l1, l2, r1, r2) ==
    [l \in VerifierLanes |->
        IF l = l1 THEN
            r1
        ELSE IF l = l2 THEN
            r2
        ELSE
            BaseCorrRawReport
    ]

SoundLaneReportDisagreement(l1, l2, r1, r2) ==
    [l \in VerifierLanes |->
        IF l = l1 THEN
            r1
        ELSE IF l = l2 THEN
            r2
        ELSE
            [decision |-> "sound"]
    ]

CorrLaneReportNeighbors ==
    { [l \in VerifierLanes |-> r] : r \in CorrRawReportNeighbors }
    \cup
    UNION
    {
        IF l1 # l2 /\ r1 # r2 THEN
            {
                CorrLaneReportDisagreement(l1, l2, r1, r2)
            }
        ELSE
            {}
        :
            l1 \in VerifierLanes,
            l2 \in VerifierLanes,
            r1 \in CorrRawReportNeighbors,
            r2 \in CorrRawReportNeighbors
    }

SoundLaneReportNeighbors ==
    { [l \in VerifierLanes |-> r] : r \in SoundRawReportNeighbors }
    \cup
    UNION
    {
        IF l1 # l2 /\ r1 # r2 THEN
            {
                SoundLaneReportDisagreement(l1, l2, r1, r2)
            }
        ELSE
            {}
        :
            l1 \in VerifierLanes,
            l2 \in VerifierLanes,
            r1 \in SoundRawReportNeighbors,
            r2 \in SoundRawReportNeighbors
    }

ReconcileCorrLaneMaps(laneMaps) ==
    [n \in Nodes |->
        LET votes == {laneMaps[l][n] : l \in VerifierLanes}
        IN IF votes = {"same"} THEN
            "same"
        ELSE IF \E s \in CorrStates: votes = {s} THEN
            CHOOSE s \in CorrStates: votes = {s}
        ELSE
            "same"
    ]

ReconcilePaperLaneMaps(laneMaps) ==
    [t \in Targets |->
        LET votes == {laneMaps[l][t] : l \in VerifierLanes}
        IN IF votes = {"same"} THEN
            "same"
        ELSE IF \E s \in CorrStates: votes = {s} THEN
            CHOOSE s \in CorrStates: votes = {s}
        ELSE
            "same"
    ]

\* Substantiveness reconciliation. Same shape as
\* `ReconcilePaperLaneMaps` but indexed over Nodes. Strict unanimity:
\* all lanes must agree on a single verdict; any disagreement collapses
\* to "same" (no status update). The kernel's per-node lane admits a
\* third "NotDoneYet" value that the kernel-level reconciler maps onto
\* "same" (no status update — node remains Unknown), so the spec's
\* CorrStates abstraction stays exact. See
\* `reconcile_substantiveness_lane_updates` (engine.rs).
ReconcileSubstantivenessLaneMaps(laneMaps) ==
    [n \in Nodes |->
        LET votes == {laneMaps[l][n] : l \in VerifierLanes}
        IN IF votes = {"same"} THEN
            "same"
        ELSE IF \E s \in CorrStates: votes = {s} THEN
            CHOOSE s \in CorrStates: votes = {s}
        ELSE
            "same"
    ]

ReconcileSoundLaneMaps(laneMaps) ==
    [n \in Nodes |->
        LET votes == {laneMaps[l][n] : l \in VerifierLanes}
        IN IF votes = {"same"} THEN
            "same"
        ELSE IF \E s \in SoundStates: votes = {s} THEN
            CHOOSE s \in SoundStates: votes = {s}
        ELSE
            "same"
    ]

LaneMapSplit(votes, states) ==
    Cardinality(votes \cap states) > 1

PaperLaneMapsSplit(laneMaps) ==
    \E t \in Targets:
        LaneMapSplit({laneMaps[l][t] : l \in VerifierLanes}, CorrStates)

SubstantivenessLaneMapsSplit(laneMaps) ==
    \E n \in Nodes:
        LaneMapSplit({laneMaps[l][n] : l \in VerifierLanes}, CorrStates)

CorrLaneMapsSplit(laneMaps) ==
    \E n \in Nodes:
        LaneMapSplit({laneMaps[l][n] : l \in VerifierLanes}, CorrStates)

SoundLaneMapsSplit(laneMaps) ==
    \E n \in Nodes:
        LaneMapSplit({laneMaps[l][n] : l \in VerifierLanes}, SoundStates)

DifficultyUpdateNeighbors(livePresent) ==
    {DefaultDifficultyUpdate}
    \cup
    {[DefaultDifficultyUpdate EXCEPT ![n] = d] : n \in livePresent, d \in DifficultyValues}

TaskBlockerChoices(global) ==
    {{}}
    \cup
    {{b} : b \in global}
    \cup
    IF global = {} THEN {} ELSE {global}

ReviewNextActiveChoices(livePhase, livePresent, liveOpen) ==
    IF livePhase = "cleanup" THEN
        {NoNode} \cup livePresent
    ELSE
        {NoNode} \cup {n \in livePresent : n \in liveOpen}

ProofComplete ==
    \A n \in presentNodes:
        ~(n \in currentProofNodes /\ n \in openNodes)

ReviewDecisionChoices(livePhase, global, retryKind) ==
    IF livePhase = "theorem_stating" THEN
        IF retryKind # "none" THEN
            {"CONTINUE", "NEED_INPUT"}
        ELSE IF global = {} THEN
            {"CONTINUE", "ADVANCE_PHASE", "NEED_INPUT"}
        ELSE
            {"CONTINUE", "NEED_INPUT"}
    ELSE IF livePhase = "proof_formalization" THEN
        {"CONTINUE", "NEED_INPUT"}
    ELSE IF livePhase = "cleanup" THEN
        {"CONTINUE", "NEED_INPUT", "DONE"}
    ELSE
        {"none"}

ApplyDifficultyUpdates(baseDifficulty, diffMap) ==
    [n \in Nodes |->
        IF diffMap[n] = "same" THEN
            baseDifficulty[n]
        ELSE
            diffMap[n]
    ]

ApplyDifficultyAttemptResets(baseDifficulty, baseAttempts, diffMap) ==
    [n \in Nodes |->
        IF diffMap[n] # "same" THEN
            0
        ELSE IF baseDifficulty[n] = "hard" THEN
            0
        ELSE
            baseAttempts[n]
    ]

ApplyDifficultyAfterSuccess(baseDifficulty, baseAttempts, diffMap, node) ==
    LET newDifficulty == ApplyDifficultyUpdates(baseDifficulty, diffMap)
        resetAttempts == ApplyDifficultyAttemptResets(baseDifficulty, baseAttempts, diffMap)
    IN [n \in Nodes |->
            IF newDifficulty[n] = "hard" THEN
                0
            ELSE IF /\ node # NoNode
                    /\ n = node
            THEN
                0
            ELSE
                resetAttempts[n]
       ]

ResetEasyAttemptForNode(baseAttempts, node) ==
    [n \in Nodes |->
        IF /\ node # NoNode
           /\ n = node
        THEN
            0
        ELSE
            baseAttempts[n]
    ]

ProofFailureDifficulty(baseDifficulty, baseAttempts, node) ==
    [n \in Nodes |->
        IF /\ node # NoNode
           /\ n = node
           /\ baseDifficulty[n] = "easy"
           /\ baseAttempts[n] + 1 >= EasyMaxRetries
        THEN
            "hard"
        ELSE
            baseDifficulty[n]
    ]

ProofFailureEasyAttempts(baseDifficulty, baseAttempts, node) ==
    [n \in Nodes |->
        IF /\ node # NoNode
           /\ n = node
           /\ baseDifficulty[n] = "easy"
        THEN
            IF baseAttempts[n] + 1 >= EasyMaxRetries THEN 0 ELSE baseAttempts[n] + 1
        ELSE IF baseDifficulty[n] = "hard" THEN
            0
        ELSE
            baseAttempts[n]
    ]

ApplyProofNodeUpdates(baseProofNodes, updateMap, livePresent) ==
    {n \in livePresent :
        IF updateMap[n] = "same" THEN
            n \in baseProofNodes
        ELSE
            updateMap[n] = "proof"
    }

ApplyNodeKindUpdates(baseNodeKinds, updateMap, livePresent) ==
    [n \in Nodes |->
        IF n \in livePresent THEN
            IF updateMap[n] = "same" THEN
                baseNodeKinds[n]
            ELSE
                updateMap[n]
        ELSE
            "definition"
    ]

ApplyNodeSetUpdates(baseMap, updateMap, livePresent) ==
    [n \in Nodes |->
        IF n \in livePresent THEN
            LET raw ==
                    IF updateMap[n].kind = "same" THEN
                        baseMap[n]
                    ELSE
                        updateMap[n].value
            IN (raw \cap livePresent) \ {n}
        ELSE
            {}
    ]

ApplyTargetClaimUpdates(baseMap, updateMap, livePresent, liveTargets) ==
    [n \in Nodes |->
        IF n \in livePresent THEN
            IF updateMap[n].kind = "same" THEN
                baseMap[n] \cap liveTargets
            ELSE
                updateMap[n].value \cap liveTargets
        ELSE
            {}
    ]

NormalizedProofNodeUpdates(baseProofNodes, observedProofNodes) ==
    [n \in Nodes |->
        IF (n \in baseProofNodes) # (n \in observedProofNodes) THEN
            IF n \in observedProofNodes THEN "proof" ELSE "not_proof"
        ELSE
            "same"
    ]

NormalizedNodeKindUpdates(baseNodeKinds, observedNodeKinds, livePresent) ==
    [n \in Nodes |->
        IF n \in livePresent /\ baseNodeKinds[n] # observedNodeKinds[n] THEN
            observedNodeKinds[n]
        ELSE
            "same"
    ]

ObservedNodeKinds(livePresent) ==
    [n \in Nodes |->
        IF n \in livePresent THEN
            NodeKinds[n]
        ELSE
            "definition"
    ]

NormalizedObservedNodeSetMap(observedMap, livePresent) ==
    [n \in Nodes |->
        IF n \in livePresent THEN
            (observedMap[n] \cap livePresent) \ {n}
        ELSE
            {}
    ]

ObservedDeps(livePresent) ==
    [n \in Nodes |->
        IF n \in livePresent THEN
            (Deps[n] \cap livePresent) \ {n}
        ELSE
            {}
    ]


NormalizedNodeSetUpdates(baseMap, observedMap, livePresent) ==
    LET normalizedObserved == NormalizedObservedNodeSetMap(observedMap, livePresent)
    IN [n \in Nodes |->
        IF baseMap[n] = normalizedObserved[n] THEN
            SameSetUpdate
        ELSE
            SetUpdate(normalizedObserved[n])
    ]

NormalizedExplicitNodeSetUpdates(baseMap, observedMap, livePresent, forceNodes) ==
    LET normalizedObserved == NormalizedObservedNodeSetMap(observedMap, livePresent)
    IN [n \in Nodes |->
        IF ~(n \in forceNodes) /\ baseMap[n] = normalizedObserved[n] THEN
            SameSetUpdate
        ELSE
            SetUpdate(normalizedObserved[n])
    ]

NormalizedTargetClaimUpdates(baseMap, observedMap, livePresent, liveTargets, forceNodes) ==
    [n \in Nodes |->
        LET normalizedObserved ==
                IF n \in livePresent THEN
                    observedMap[n] \cap liveTargets
                ELSE
                    {}
            current ==
                IF n \in livePresent THEN
                    baseMap[n] \cap liveTargets
                ELSE
                    {}
        IN IF ~(n \in forceNodes) /\ current = normalizedObserved THEN
            SameSetUpdate
        ELSE
            SetUpdate(normalizedObserved)
    ]

DefaultWorkerTargetClaimUpdates(livePresent) ==
    [n \in Nodes |->
        IF n \in (livePresent \ presentNodes) THEN
            SetUpdate({})
        ELSE
            SameSetUpdate
    ]

TargetClaimUpdateNeighbors(livePresent, liveTargets) ==
    {DefaultWorkerTargetClaimUpdates(livePresent)}
    \cup
    UNION {
        {
            [m \in Nodes |->
                IF m = n THEN
                    SetUpdate(claims)
                ELSE
                    DefaultWorkerTargetClaimUpdates(livePresent)[m]
            ] :
                claims \in TargetSetNeighbors(
                    IF n \in presentNodes THEN
                        currentTargetClaims[n] \cap liveTargets
                    ELSE
                        {}
                )
        } :
            n \in livePresent
    }

NormalizedWorkerCoverage(baseTargetClaims, targetClaimMap, livePresent, liveTargets) ==
    CoverageFromClaims(
        ApplyTargetClaimUpdates(baseTargetClaims, targetClaimMap, livePresent, liveTargets),
        livePresent,
        liveTargets
    )

WorkerStructuralDelta ==
    \/ \E n \in Nodes: response.proofNodeMap[n] # "same"
    \/ \E n \in Nodes: response.nodeKindMap[n] # "same"
    \/ \E n \in Nodes: response.depMap[n].kind # "same"
    \/ \E n \in Nodes: response.targetClaimMap[n].kind # "same"

WorkerSemanticDelta ==
    \/ WorkerStructuralDelta
    \/ <<
            response.present,
            response.open,
            response.corrCurrent,
            response.paperCurrent,
            response.soundCurrent,
            response.targetFp
       >>
       #
       <<
            presentNodes,
            openNodes,
            corrCurrentFp,
            paperCurrentFp,
            soundCurrentFp,
            currentTargetFp
       >>

WorkerCoverageMatchesUpdates ==
    response.kind # "worker"
    \/ response.coverage =
        NormalizedWorkerCoverage(
            currentTargetClaims,
            response.targetClaimMap,
            response.present,
            configuredTargets
        )

WorkerStructuralMapsMatchObserved ==
    response.kind # "worker"
    \/ /\ response.proofNodeMap =
            NormalizedProofNodeUpdates(
                currentProofNodes,
                ProofNodesFromKinds(ObservedNodeKinds(response.present), response.present)
            )
       /\ response.nodeKindMap =
            NormalizedNodeKindUpdates(currentNodeKinds, ObservedNodeKinds(response.present), response.present)
       /\ response.depMap =
            NormalizedNodeSetUpdates(currentDeps, ObservedDeps(response.present), response.present)
       /\ response.targetClaimMap =
            NormalizedTargetClaimUpdates(
                currentTargetClaims,
                ApplyTargetClaimUpdates(
                    currentTargetClaims,
                    response.targetClaimMap,
                    response.present,
                    configuredTargets
                ),
                response.present,
                configuredTargets,
                response.present \ inFlightRequest.currentPresentNodes
            )

WorkerRequiredExplicitTargetClaimNodes ==
    response.present \ inFlightRequest.currentPresentNodes

WorkerAcceptedResponsesSatisfyContract ==
    response.kind # "worker"
    \/ response.workerOutcome # "valid"
    \/ ~inFlightRequest.workerAcceptance.enabled
    \/ /\ WorkerStructuralMapsMatchObserved
       /\ WorkerValidationStepResultsSatisfyContract(
              inFlightRequest.workerAcceptance.validationExecutionPlan,
              response.validationStepResults
          )
       /\ WorkerCoverageMatchesUpdates
       /\ (
              (~inFlightRequest.workerAcceptance.requireExplicitTargetClaimsForNewNodes)
              \/ \A n \in WorkerRequiredExplicitTargetClaimNodes:
                    response.targetClaimMap[n].kind # "same"
          )

\* Tier 1: worker no-progress preserves committed structural state.
\*
\* When a worker returns Stuck or NeedsRestructure (the two non-progress
\* outcomes), the engine calls `state.restore_committed()` and emits
\* `RestoreWorktreeToActiveWorkerBase` to roll the live worktree back to
\* the active worker base (PROCESS_SEMANTICS §4.2, "Stuck" and
\* "NeedsRestructure" paragraphs). At every protocol pause point that
\* observes the post-rollback state — Reviewer (escalation), Worker
\* (same-cycle retry), HumanGate (downstream NEED_INPUT route) — the
\* live structural fields must therefore equal the committed mirror.
\*
\* Single-state form: when the retry-outcome discriminant is "stuck" or
\* "needs_restructure" and the stage observing it is one of those three
\* pause points, the structural tier (presentNodes, currentNodeKinds,
\* currentDeps, currentTargetClaims, openNodes) must equal its committed
\* counterpart. The verifier fingerprint tiers are intentionally OUT of
\* scope here — they are restored by the same engine path but a separate
\* Tier 1 invariant (`QuiescentLiveEqualsCommitted`) covers them at the
\* between-cycle resting point.
\*
\* Earlier (2026-04-ish) the spec reclassified Stuck/NeedsRestructure with
\* a state delta as Invalid; the kernel removed that rule when the
\* automatic worktree-rollback widened (commit `daf5ecf`), per
\* CLAUDES_NOTES_remove_stuck_nr_no_delta_rule.md. The rollback path is
\* the actual safety mechanism; this invariant captures the post-rollback
\* observable invariant on the live tier.
WorkerNoProgressPreservesState ==
    (retryOutcomeKind \in {"stuck", "needs_restructure"}
        /\ stage \in {"Reviewer", "Worker", "HumanGate"})
    => /\ presentNodes        = committedPresentNodes
       /\ currentNodeKinds    = committedNodeKinds
       /\ currentDeps         = committedDeps
       /\ currentTargetClaims = committedTargetClaims
       /\ openNodes           = committedOpenNodes

WorkerFinalOutcome ==
    IF response.kind # "worker" \/ response.status # "ok" THEN
        response.workerOutcome
    ELSE IF response.workerOutcome = "valid" /\ ~WorkerAcceptedResponsesSatisfyContract THEN
        "invalid"
    ELSE
        response.workerOutcome

ReviewResetChoices(currentPhase, retryKind) ==
    \* Mirror of kernel `request_allowed_resets` in model.rs.
    \* Cleanup phase invariant: every accepted state is Done-valid
    \* (formalization_complete). Rewinding would let the reviewer
    \* re-enter a state that may not be Done-valid, breaking the
    \* invariant. The kernel returns exactly {None} for Cleanup
    \* regardless of retry status.
    IF currentPhase = "cleanup" THEN
        {NoCheckpoint}
    ELSE
        LET base ==
                IF retryKind # "none"
                THEN
                    {NoCheckpoint, "lastCommit"}
                ELSE
                    {NoCheckpoint}
            canLastClean ==
                /\ hasEverBeenClean
                /\ cyclesSinceClean >= 1
        IN IF canLastClean THEN base \cup {"lastClean"} ELSE base

WorkerRetryThreshold(currentPhase, retryKind) ==
    IF /\ currentPhase = "theorem_stating"
          /\ retryKind \in {"invalid", "stuck"}
    THEN
        MaxAttempt
    ELSE IF /\ currentPhase = "proof_formalization"
               /\ retryKind \in {"invalid", "stuck"}
    THEN
        ProofInvalidReviewThreshold
    ELSE
        0

CurrentRetryAttempt(retryKind) ==
    IF retryOutcomeKind = retryKind THEN attempt ELSE 1

CurrentNodeCorrState(n) ==
    IF /\ n \in presentNodes
       /\ corrStatus[n] = "pass"
       /\ corrCurrentFp[n] = corrApprovedFp[n]
    THEN
        "pass"
    ELSE IF /\ n \in presentNodes
            /\ corrStatus[n] = "fail"
            /\ corrCurrentFp[n] = corrApprovedFp[n]
    THEN
        "fail"
    ELSE
        "unknown"

CurrentNodeCorrPass(n) ==
    CurrentNodeCorrState(n) = "pass"

CurrentNodeCorrFail(n) ==
    CurrentNodeCorrState(n) = "fail"

CurrentNodeCorrUnknown(n) ==
    CurrentNodeCorrState(n) = "unknown"

PostNodeCorrState(n, corrMap, corrApprovedMap) ==
    IF /\ n \in presentNodes
       /\ corrMap[n] = "pass"
       /\ corrCurrentFp[n] = corrApprovedMap[n]
    THEN
        "pass"
    ELSE IF /\ n \in presentNodes
            /\ corrMap[n] = "fail"
            /\ corrCurrentFp[n] = corrApprovedMap[n]
    THEN
        "fail"
    ELSE
        "unknown"

PostNodeCorrUnknown(n, corrMap, corrApprovedMap) ==
    PostNodeCorrState(n, corrMap, corrApprovedMap) = "unknown"

CurrentPaperState(t) ==
    IF /\ t \in configuredTargets
       /\ currentCoverage[t] = {}
    THEN
        "fail"
    ELSE IF /\ t \in configuredTargets
       /\ paperStatus[t] = "pass"
       /\ paperCurrentFp[t] = paperApprovedFp[t]
    THEN
        "pass"
    ELSE IF /\ t \in configuredTargets
            /\ paperStatus[t] = "fail"
            /\ paperCurrentFp[t] = paperApprovedFp[t]
    THEN
        "fail"
    ELSE
        "unknown"

CurrentPaperPass(t) ==
    CurrentPaperState(t) = "pass"

CurrentPaperFail(t) ==
    CurrentPaperState(t) = "fail"

CurrentPaperUnknown(t) ==
    CurrentPaperState(t) = "unknown"

\* ----------------------------------------------------------------------
\* Deviation lane state surface
\* ----------------------------------------------------------------------
\* Mirror of kernel `current_deviation_state` (model.rs:6576-6594 after
\* sticky-Fail / empty-fp follow-ups in efaafa7). An entry is meaningful
\* only when `deviationFiles[id]` (the kernel `BTreeMap` has the key).
\* For absent ids the spec returns "unknown" so downstream helpers can
\* treat them as "no longer authorized" — the lane is empty and no
\* blocker fires (see `DeviationBlockersFor` which filters on
\* `deviationFiles[id]` first).
\*
\* Sticky-Fail (efaafa7 model.rs:6582-6594): a Fail verdict remains
\* visible iff the live fingerprint equals the approved fingerprint AND
\* both are non-empty. Once the live fp drifts (the worker re-edited
\* the reference file), the lane reopens to Unknown — the reviewer
\* cannot leave an entry sitting on Fail across a content rewrite.
\* The empty-fingerprint clause is part of the same follow-up: a
\* file that disappeared or is unreadable observation-time has empty
\* `deviationCurrentFp`, which by this rule yields Unknown even with
\* status=Pass.
CurrentDeviationState(id) ==
    IF ~deviationFiles[id] THEN
        "unknown"
    ELSE IF /\ deviationStatus[id] = "pass"
            /\ deviationCurrentFp[id] # NoFingerprint
            /\ deviationCurrentFp[id] = deviationApprovedFp[id]
    THEN
        "pass"
    ELSE IF /\ deviationStatus[id] = "fail"
            /\ deviationCurrentFp[id] # NoFingerprint
            /\ deviationCurrentFp[id] = deviationApprovedFp[id]
    THEN
        "fail"
    ELSE
        "unknown"

CurrentDeviationPass(id)    == CurrentDeviationState(id) = "pass"
CurrentDeviationFail(id)    == CurrentDeviationState(id) = "fail"
CurrentDeviationUnknown(id) == CurrentDeviationState(id) = "unknown"

\* True iff the node claims at least one authorized deviation whose
\* current state is NOT Pass. Mirror of kernel
\* `node_has_unauthorized_deviation_claim` (model.rs:6553-6559). Used
\* both as a substantiveness short-circuit (in
\* `CurrentSubstantivenessState` below) and as the carrier check for
\* `nodeDeviationClaims` post-mutation pruning.
NodeHasUnauthorizedDeviationClaim(n) ==
    \E id \in nodeDeviationClaims[n] : ~CurrentDeviationPass(id)

\* The frontier of deviations whose authorization is currently Unknown
\* and thus eligible for the per-cycle verifier dispatch. Mirror of
\* kernel `deviation_verify_ids` (model.rs:6522-6528).
DeviationVerifyIds ==
    {id \in Deviations : deviationFiles[id] /\ CurrentDeviationUnknown(id)}

\* Substantiveness lane (theorem-stating + proof-formalization). Cleanup
\* and Complete are dormant: returns "pass" unconditionally so downstream
\* legality predicates don't get wedged on stale Unknown entries from a
\* pre-advance state. Mirror of kernel `current_substantiveness_state`
\* (model.rs:2391). Helper nodes added by Hard restructure in
\* proof-formalization receive substantiveness checks just like
\* theorem-stating nodes.
\*
\* Audit follow-up (2026-05-27, kernel commit 4e83783, model.rs:6650-6651):
\* when `nodeHasUnauthorizedDeviationClaim(n)` holds (the node claims at
\* least one deviation whose authorization is not currently Pass), the
\* substantiveness state short-circuits to Unknown — the lane cannot
\* advance until either the deviation is authorized or the claim is
\* dropped. This is the "blocks substantiveness lane via short-circuit"
\* mirror of `current_substantiveness_state`'s
\* `node_has_unauthorized_deviation_claim` branch.
CurrentSubstantivenessState(n) ==
    IF phase \notin {"theorem_stating", "proof_formalization"} THEN
        "pass"
    ELSE IF n \notin presentNodes THEN
        "unknown"
    ELSE IF NodeHasUnauthorizedDeviationClaim(n) THEN
        "unknown"
    ELSE IF /\ substantivenessStatus[n] = "pass"
            /\ substantivenessCurrentFp[n] = substantivenessApprovedFp[n]
    THEN "pass"
    ELSE IF /\ substantivenessStatus[n] = "fail"
            /\ substantivenessCurrentFp[n] = substantivenessApprovedFp[n]
    THEN "fail"
    ELSE "unknown"

CurrentSubstantivenessPass(n)    == CurrentSubstantivenessState(n) = "pass"
CurrentSubstantivenessFail(n)    == CurrentSubstantivenessState(n) = "fail"
CurrentSubstantivenessUnknown(n) == CurrentSubstantivenessState(n) = "unknown"

NeedsSound(n) ==
    /\ n \in presentNodes
    /\ n \in currentProofNodes
    /\ n \in openNodes

CurrentSoundState(n) ==
    IF ~NeedsSound(n) THEN
        "pass"
    ELSE IF /\ soundStatus[n] = "pass"
            /\ soundCurrentFp[n] = soundApprovedFp[n]
    THEN
        "pass"
    ELSE IF /\ soundStatus[n] \in {"fail", "structural"}
            /\ soundCurrentFp[n] = soundApprovedFp[n]
    THEN
        "fail"
    ELSE
        "unknown"

CurrentSoundPass(n) ==
    CurrentSoundState(n) = "pass"

CurrentSoundFail(n) ==
    CurrentSoundState(n) = "fail"

CurrentSoundUnknown(n) ==
    CurrentSoundState(n) = "unknown"

BlockedTargets ==
    {t \in configuredTargets : ~CurrentPaperPass(t)}

\* In TheoremStating, only nodes whose substantiveness lane
\* has passed are eligible for correspondence verification — the corr lane
\* should never run before the substantiveness lane gives a Pass. The
\* substantiveness lane is also active in ProofFormalization (see
\* `SubstantivenessVerifyNodes`), but the StartCycle dispatch priority
\* drains substantiveness ahead of corr in both phases, so by the time
\* CorrVerifyNodes is consulted the proof-phase substantiveness frontier
\* is already empty — the filter would be a no-op there. (The kernel's
\* `corr_verify_nodes` applies the substantiveness Pass filter
\* unconditionally; the spec's phase split is a benign over-permissive
\* abstraction for unreachable proof-phase corr-without-substantiveness-
\* drain states.)
CorrVerifyNodes ==
    IF phase = "theorem_stating" THEN
        {n \in presentNodes : CurrentNodeCorrUnknown(n) /\ CurrentSubstantivenessPass(n)}
    ELSE
        {n \in presentNodes : CurrentNodeCorrUnknown(n)}

PaperVerifyTargets ==
    {t \in configuredTargets : CurrentPaperUnknown(t)}

\* Substantiveness frontier. Fires in theorem-stating + proof-formalization
\* (helper nodes added by Hard restructure are checked too). Cleanup and
\* Complete are dormant — the lane is empty there.
\*
\* Kernel/spec divergence (benign): the kernel's
\* `substantiveness_verify_nodes()` excludes Preamble-kind nodes
\* (their fingerprint is empty and there is no .tex statement
\* block to verify). This spec over-approximates by including all
\* `presentNodes`. The over-approximation is safe: a Preamble node's
\* `CurrentSubstantivenessUnknown` is unreachable since the kernel
\* never issues a Pass/Fail update for it, leaving status at the
\* default — which the spec models as `unknown`. If TLC ever
\* generates a state where a Preamble node enters the frontier,
\* it will linger forever, exposing the abstraction. Empirically
\* this has not occurred; refining if needed: filter Preamble in
\* `presentNodes` here once the spec exposes node-kind structure.
SubstantivenessVerifyNodes ==
    IF phase \in {"theorem_stating", "proof_formalization"} THEN
        {n \in presentNodes : CurrentSubstantivenessUnknown(n)}
    ELSE
        {}

\* Mirror of kernel `corr_blockers_exist` in model.rs. Disjunction over
\* every lane whose open verdict suspends Sound verifier dispatch:
\* node corr, paper-target faithfulness, substantiveness (PF/TS only),
\* and deviation authorization. The deviation clause was added with
\* the deviation lane (kernel 2026-05-27/28 commits 7aad7cb /
\* 4e83783 / efaafa7); a tracked deviation that is Unknown or Fail
\* sits in the same "verification surface in motion" bucket that the
\* Sound verifier must avoid pinning a verdict against.
CorrespondenceBlockersExist ==
    \/ \E n \in presentNodes: CurrentNodeCorrState(n) # "pass"
    \/ \E t \in configuredTargets: CurrentPaperState(t) # "pass"
    \/ /\ phase \in {"theorem_stating", "proof_formalization"}
       /\ \E n \in presentNodes: CurrentSubstantivenessState(n) # "pass"
    \/ \E id \in Deviations:
            /\ deviationFiles[id]
            /\ ~CurrentDeviationPass(id)

TheoremSoundCandidates ==
    IF CorrespondenceBlockersExist THEN
        {}
    ELSE
        {
            n \in presentNodes :
                /\ n \in currentProofNodes
                /\ n \in openNodes
                /\ CurrentNodeCorrPass(n)
                /\ CurrentSoundState(n) # "pass"
        }

SelectedTheoremHeldTarget ==
    IF /\ heldTarget \in TheoremSoundCandidates
       /\ \A m \in TheoremSoundCandidates: NodeRank[heldTarget] >= NodeRank[m]
    THEN
        heldTarget
    ELSE IF TheoremSoundCandidates = {} THEN
        NoNode
    ELSE
        CHOOSE n \in TheoremSoundCandidates:
            \A m \in TheoremSoundCandidates:
                \/ NodeRank[n] > NodeRank[m]
                \/ /\ NodeRank[n] = NodeRank[m]
                   /\ NodeOrder[n] >= NodeOrder[m]

NodeCorrBlockersFor(corrMap, corrCurrentMap, corrApprovedMap, livePresent) ==
    {Blocker("node_corr", NodeObject(n), corrCurrentMap[n]) :
        n \in {m \in livePresent : ~(corrMap[m] = "pass" /\ corrCurrentMap[m] = corrApprovedMap[m])}}

PaperBlockersFor(targetMap, targetCurrentMap, targetApprovedMap, liveConfiguredTargets) ==
    {Blocker("paper_faithfulness", TargetObject(t), targetCurrentMap[t]) :
        t \in {u \in liveConfiguredTargets : ~(targetMap[u] = "pass" /\ targetCurrentMap[u] = targetApprovedMap[u])}}

\* Substantiveness blockers. Fires in theorem-stating + proof-formalization
\* (helpers added by Hard restructure participate too); cleanup and complete
\* are dormant. Mirrors `NodeCorrBlockersFor` in shape (node-bound,
\* fingerprint = current).
SubstantivenessBlockersFor(nodeMap, nodeCurrentMap, nodeApprovedMap, livePresent) ==
    IF phase \notin {"theorem_stating", "proof_formalization"} THEN
        {}
    ELSE
        \* Mirror `CurrentSubstantivenessState` (which short-circuits to
        \* Unknown when a node has an unauthorized deviation claim, per
        \* kernel model.rs:6650-6651). The blocker enumeration must
        \* agree with the lane-state predicate — without the
        \* unauthorized-claim disjunct, a Pass+fp-matched node with a
        \* claim to a non-Pass deviation reads Unknown via
        \* `CurrentSubstantivenessState` but doesn't appear in
        \* GlobalBlockers, violating `GlobalBlockersExhaustive`.
        {Blocker("substantiveness", NodeObject(n), nodeCurrentMap[n]) :
            n \in {m \in livePresent :
                \/ NodeHasUnauthorizedDeviationClaim(m)
                \/ ~(nodeMap[m] = "pass" /\ nodeCurrentMap[m] = nodeApprovedMap[m])}}

SoundBlockersFor(soundMap, soundCurrentMap, soundApprovedMap, livePresent, liveOpen) ==
    {Blocker("soundness", NodeObject(n), soundCurrentMap[n]) :
        n \in {m \in livePresent : m \in currentProofNodes /\ m \in liveOpen /\ ~(soundMap[m] = "pass" /\ soundCurrentMap[m] = soundApprovedMap[m])}}

\* Deviation blockers (kernel `BlockerKind::Deviation`, model.rs:7099-7115
\* and 8173-8189 — both `global_blockers` and `current_failed_blockers`
\* enumerate them). Membership rule: any id in `deviationFiles` whose
\* current state is NOT Pass is a non-deferred blocker carrying the
\* live fingerprint. Note this is phase-independent — unlike the
\* substantiveness lane there's no phase-dormancy carve-out. A failed
\* deviation in Cleanup is a stuck state at the protocol level:
\* `review_task_blocker_in_worker_scope` (model.rs:2947-2956) returns
\* FALSE for Deviation blockers in Cleanup/Complete, so no worker can
\* take the blocker as a task. The deviation must be retired or
\* re-authorized BEFORE Cleanup entry.
DeviationBlockersFor(filesMap, statusMap, currentMap, approvedMap) ==
    {Blocker("deviation", DeviationObject(id), currentMap[id]) :
        id \in
            {d \in Deviations :
                /\ filesMap[d]
                /\ ~(/\ statusMap[d] = "pass"
                     /\ currentMap[d] # NoFingerprint
                     /\ currentMap[d] = approvedMap[d])}}

GlobalBlockers ==
    NodeCorrBlockersFor(corrStatus, corrCurrentFp, corrApprovedFp, presentNodes)
    \cup
    PaperBlockersFor(paperStatus, paperCurrentFp, paperApprovedFp, configuredTargets)
    \cup
    SubstantivenessBlockersFor(substantivenessStatus, substantivenessCurrentFp, substantivenessApprovedFp, presentNodes)
    \cup
    SoundBlockersFor(soundStatus, soundCurrentFp, soundApprovedFp, presentNodes, openNodes)
    \cup
    DeviationBlockersFor(deviationFiles, deviationStatus, deviationCurrentFp, deviationApprovedFp)

\* Patch C extension (LOCAL_CLOSURE_IMPL_PLAN.md §7.6, §1.1).
\* `FormalizationComplete` now also requires that no proof_node is in
\* the local-closure-unverified set. The unverified set is sorry-free-
\* only by invariant (§7.2) — a sorry-d proof_node sits in `openNodes`
\* and is caught by `ProofComplete`. The new clause closes the stale-
\* pass gap: a sorry-free importer whose helper's statement has changed
\* is recorded as unverified, and the engine cannot flip
\* ProofFormalization → Cleanup until the unverified node is either
\* re-closed or operator-waived. Mirrors kernel `formalization_complete`
\* per plan §7.6 (the `unverified_clean` and `records_present` clauses
\* both reduce, at TLA's abstraction level, to "no proof_node sits in
\* `localClosureUnverified` at completion time"). The stochastic check
\* `StalePassClosurePreventsCleanupTransition` (below) captures this.
\*
\* Blockers-clean clause: the kernel's `formalization_complete`
\* (model.rs) requires `global_blockers().is_empty()` so a PF→Cleanup
\* transition cannot leave a verifier blocker dangling. Without it the
\* `CleanupHasNoBlockers` invariant would be discovered only after the
\* transition fires, instead of forming part of the gate. Spec mirrors
\* the kernel's `blockers_clean` clause here.
\*
\* Placement (2026-06-05): hoisted from its original site at line
\* ~2276 to immediately after `GlobalBlockers` so SANY's strict
\* forward-declaration check is satisfied (the body references
\* `GlobalBlockers`).
FormalizationComplete ==
    /\ ProofComplete
    /\ \A n \in presentNodes:
        ~(n \in currentProofNodes /\ n \in localClosureUnverified)
    /\ GlobalBlockers = {}

\* === Active-coarse-anchor machinery (proposal v32) ===
\* The mechanism is GATED on coarseDagNodes # {}: when the coarse DAG
\* is empty (pre-implementation runs; model.rs:1246), all helpers
\* below degrade to no-ops so legacy behavior is preserved.

\* Node-bound blockers that are NOT in the down-cone of the active
\* coarse anchor. Paper blockers are target-bound; their carrier is
\* the union of all currentCoverage entries for the failing target.
CoarseTaskBlockerNodes ==
    LET nodeBlockerCarriers ==
            {b.object.node :
                b \in {bb \in GlobalBlockers :
                          bb.kind \in {"node_corr", "substantiveness", "soundness"}}}
        paperBlockerCarriers ==
            UNION { currentCoverage[b.object.target] :
                    b \in {bb \in GlobalBlockers : bb.kind = "paper_faithfulness"} }
    IN  (nodeBlockerCarriers \cup paperBlockerCarriers) \cap presentNodes

\* TRUE when at least one task blocker carrier lies outside the
\* active-coarse cone. Reviewer prompt frames the cycle as
\* "repair these blockers, no broader formalization."
CoarseRepairMode ==
    /\ activeCoarseNode # NoNode
    /\ \E b \in CoarseTaskBlockerNodes :
           b \notin CoarseNodeSupportCone(activeCoarseNode, presentNodes)

\* Legal next_active set when an active coarse anchor is set.
\*   * Base case: the down-cone of the anchor.
\*   * In CoarseRepairMode: extended with each task blocker node and
\*     its own down-cone (the user-clarified "work on any blocker or
\*     a recursive import of a blocker" rule).
\* When activeCoarseNode = NoNode (TheoremStating / Cleanup / boot,
\* or coarseDagNodes empty), returns presentNodes so existing
\* legality remains the sole gate.
CoarseLegalActiveSet ==
    IF activeCoarseNode = NoNode \/ coarseDagNodes = {} THEN
        presentNodes
    ELSE
        LET baseCone == CoarseNodeSupportCone(activeCoarseNode, presentNodes)
        IN  IF CoarseRepairMode THEN
                baseCone \cup CoarseTaskBlockerNodes \cup
                UNION { CoarseNodeSupportCone(b, presentNodes) :
                        b \in CoarseTaskBlockerNodes }
            ELSE
                baseCone

\* True iff the reviewer is permitted to change the active coarse
\* anchor this cycle. Three escape paths:
\*   1. No anchor yet (initial seed).
\*   2. Strict unlock: anchor shallow-closed AND no blockers.
\*   3. Starvation escape: stuck in CoarseRepairMode for >= threshold
\*      cycles. Prevents indefinite blocker-chain drift (audit issue 3).
\* Mirror of kernel `ProtocolState::ever_shallow_coarse_closed_regressed()`
\* (model.rs ~6425). Subset of `everShallowCoarseClosed` whose members
\* are NOT currently shallow-closed against the COMMITTED snapshot.
\* Computed against committed (not live) state so in-flight worker
\* deltas whose changes have not yet been accepted do not pollute the
\* regression set. Empty when `coarseDagNodes` is empty.
\*
\* This is the load-bearing global_repair_mode S8 safety set: the
\* anchor-change lock is held while it is non-empty (see
\* `AnchorChangeForbiddenDuringGlobalRepair` invariant below).
EverShallowCoarseClosedRegressed ==
    IF coarseDagNodes = {} THEN {}
    ELSE
        {n \in everShallowCoarseClosed :
            /\ n \in coarseDagNodes
            /\ ~ShallowlyClosedFromCoarse(n,
                committedPresentNodes, committedOpenNodes, coarseDagNodes)}

\* Gated on coarseDagNodes # {} -- when empty the whole mechanism is
\* dormant and "change allowed" is vacuously TRUE.
\*
\* global_repair_mode S8: kernel `active_coarse_change_allowed`
\* (model.rs ~6481) ALSO requires that
\* `ever_shallow_coarse_closed_regressed()` is empty UNLESS the
\* starvation escape fires. The starvation escape is a deliberate
\* bypass — see model.rs ~6498 comment — so a stuck regression cannot
\* lock the run forever. The spec restates this gate explicitly: the
\* strict shallow-closed unlock additionally requires the regression
\* set to be empty.
ActiveCoarseChangeAllowed ==
    \/ coarseDagNodes = {}
    \/ activeCoarseNode = NoNode
    \/ /\ ShallowlyClosedFromCoarse(
              activeCoarseNode, presentNodes, openNodes, coarseDagNodes)
       /\ GlobalBlockers = {}
       /\ EverShallowCoarseClosedRegressed = {}
    \/ cyclesInCoarseRepairMode >= StuckCoarseRepairThreshold

\* Mirror of kernel `WrapperRequest.coarse_anchor_starvation_unlocked`
\* (model.rs). TRUE iff the anchor lock is currently open ONLY because
\* the starvation guard fired, not because the anchor reached its
\* clean-unlock predicate. Computed only on Review requests in
\* ProofFormalization outside retry contexts.
\* The flag is consumed by the must-advance-anchor-on-clean-unlock
\* rejection rule (kernel commit 31d9012): starvation unlocks let the
\* reviewer keep discretion, clean unlocks force an advance.
CoarseAnchorStarvationUnlocked ==
    /\ phase = "proof_formalization"
    /\ retryOutcomeKind = "none"
    /\ coarseDagNodes # {}
    /\ activeCoarseNode # NoNode
    /\ cyclesInCoarseRepairMode >= StuckCoarseRepairThreshold
    /\ CoarseRepairMode

\* Coarse-anchor candidates surfaced to the reviewer when change is
\* allowed: present coarse nodes that are not yet shallow-closed.
\* Empty when ActiveCoarseChangeAllowed is FALSE -- reviewer cannot
\* pick anything.
\*
\* Audit-2 followup #6: also empty on retry-Reviews. The validator
\* `ReviewContinueProof` already rejects non-NoNode `nextActiveCoarse`
\* under `retryOutcomeKind # "none"` (line ~10733); without the
\* retry gate here the reviewer would see candidates the response
\* validator then refuses.
KernelHintedNextActiveCoarseNodes ==
    IF /\ phase = "proof_formalization"
       /\ coarseDagNodes # {}
       /\ retryOutcomeKind = "none"
       /\ ActiveCoarseChangeAllowed
    THEN
        (coarseDagNodes \cap presentNodes) \ ShallowlyClosedCoarseNodes(presentNodes, openNodes, coarseDagNodes)
    ELSE
        {}

\* Audit-2 followup #3: legal-active cone for an arbitrary prospective
\* anchor, mirroring the kernel's `coarse_legal_active_set_for_anchor`
\* in `WrapperRequest`. Used by `ReviewDecisionLegal` to validate
\* `response.nextActive` when the reviewer is also proposing
\* `response.nextActiveCoarse # NoNode` -- the prospective anchor's
\* cone, not the current `CoarseLegalActiveSet`.
CoarseLegalActiveSetForAnchor(anchor) ==
    IF coarseDagNodes = {} \/ anchor = NoNode THEN
        presentNodes
    ELSE IF anchor \notin presentNodes THEN
        {}
    ELSE
        LET baseCone == CoarseNodeSupportCone(anchor, presentNodes)
            needsWidening == \E c \in CoarseTaskBlockerNodes : c \notin baseCone
        IN  IF needsWidening THEN
                baseCone \cup CoarseTaskBlockerNodes \cup
                UNION { CoarseNodeSupportCone(b, presentNodes) :
                        b \in CoarseTaskBlockerNodes }
            ELSE
                baseCone

\* True iff `node` is the natural focus for repairing some outstanding
\* blocker — either a Node-bound blocker on itself (NodeCorr / Soundness /
\* Substantiveness), or a Target-bound PaperFaithfulness blocker that
\* this node covers. Mirrors kernel `proof_node_repairs_blocker`
\* (model.rs:2563). The kernel uses this to make a closed-proof node
\* legal as `next_active` when the only outstanding work is on it.
\*
\* Spec gap closure (2026-05-05): the previous `ActiveNodeLegal` body
\* required `node \in liveOpen` for proof_formalization, missing the
\* closed-proof blocker-recovery clause that the kernel has carried
\* since the original blocker-aware reviewer surface landed.
\*
\* Order note (2026-05-21): this and the two helpers below must appear
\* before `ProofActiveNodeBaseLegalCandidates` (the only call site) so
\* SANY can resolve the forward references. The previous interleaved
\* layout tripped SANY's strict prior-declaration check, leaving the
\* spec un-parseable. No semantic change.
ProofNodeRepairsBlocker(node) ==
    /\ node \in presentNodes
    /\ \/ ~CurrentNodeCorrPass(node)
       \/ /\ NeedsSound(node)
          /\ ~CurrentSoundPass(node)
       \/ ~CurrentSubstantivenessPass(node)
       \/ \E t \in configuredTargets :
            /\ ~CurrentPaperPass(t)
            /\ node \in currentCoverage[t]

\* Direct consumer focus for failed substantiveness. If a proof node
\* directly imports a node with a live Substantiveness blocker, the
\* reviewer may focus the importing node so the worker can remove or
\* replace that dependency. This is deliberately direct-only rather
\* than transitive: higher ancestors should not be exposed just because
\* they eventually depend on the failed node.
ProofNodeDirectlyImportsSubstantivenessBlocker(node) ==
    /\ phase = "proof_formalization"
    /\ node \in presentNodes
    /\ \E dep \in currentDeps[node] :
        /\ dep \in presentNodes
        /\ ~CurrentSubstantivenessPass(dep)

\* Closed-proof aggregate-focus candidate set, conservative shape:
\* fires only when every live blocker is node-bound and there are at
\* least two distinct blocked nodes (single-node case is already
\* handled by `ProofNodeRepairsBlocker`). Returns the minimal common
\* importers of all blocked nodes under dep-closure containment, so
\* high aggregator roots above the closest common importer are not
\* exposed. Mirrors kernel
\* `proof_aggregate_node_blocker_focus_candidates`. Guards a routing
\* trap that arises when two sibling helper nodes carry soundness
\* blockers and no single legal `next_active` covers both: without an
\* aggregate focus candidate the reviewer would be forced into LastClean
\* as the only escape.
\*
\* Why "any target-bound blocker → empty": this rule deliberately does
\* not extend the aggregate-focus affordance into paper-coverage repair
\* territory. PaperFaithfulness is target-bound, lives behind a
\* different worker-scope path (`task_blockers_outside_review_worker_scope`
\* takes a target-cone disjunction route), and broadening the rule
\* would risk handing the worker an aggregate-mode task whose
\* downstream scope rules don't authorize the necessary edits.
ProofAggregateNodeBlockerFocusCandidates ==
    IF phase # "proof_formalization" THEN
        {}
    ELSE
        LET
            blockedNodes ==
                IF \E b \in GlobalBlockers : b.object.otype = "target"
                THEN {}
                ELSE { b.object.node : b \in GlobalBlockers }
            candidateCovers ==
                { n \in presentNodes :
                    blockedNodes \subseteq DepClosure({n}, presentNodes) }
        IN
            IF Cardinality(blockedNodes) < 2 THEN
                {}
            ELSE
                { n \in candidateCovers :
                    ~\E other \in candidateCovers :
                        /\ other # n
                        /\ DepClosure({other}, presentNodes) \subseteq DepClosure({n}, presentNodes)
                        /\ DepClosure({other}, presentNodes) # DepClosure({n}, presentNodes) }

ProofNodeRepairsAggregateNodeBlockers(node) ==
    node \in ProofAggregateNodeBlockerFocusCandidates

\* Audit-2 followup #3: pre-cone-narrowing `next_active` candidates in
\* ProofFormalization. Mirrors the kernel
\* `proof_active_node_base_legal_candidates()` -- the same disjuncts
\* `ActiveNodeLegal` uses for the proof_formalization branch, minus
\* the `node \in CoarseLegalActiveSet` conjunct so the set is
\* anchor-agnostic. Combined with `CoarseLegalActiveSetForAnchor(B)`
\* this validates a one-cycle anchor switch A -> B in
\* `ReviewDecisionLegal`.
\*
\* Definition moved (parser fix): TLA+ semantic analysis under TLC 1.7.2
\* requires operator definitions to precede their referents; the body
\* below uses `ProofNodeRepairsBlocker`,
\* `ProofNodeDirectlyImportsSubstantivenessBlocker`, and
\* `ProofNodeRepairsAggregateNodeBlockers` defined just above, so the
\* whole block sits after them. The original position (immediately
\* before the `ProofNodeRepairsBlocker` cluster) violated
\* forward-reference rules and failed semantic analysis. The Tier 1,
\* Tier 2, and Tier 3 worktrees all independently surfaced and applied
\* this same reordering before their invariant work could TLC-parse.
ProofActiveNodeBaseLegalCandidates ==
    IF phase # "proof_formalization" THEN
        {}
    ELSE
        {n \in presentNodes :
            \/ n \in openNodes
            \/ n \in localClosureUnverified
            \/ ProofNodeRepairsBlocker(n)
            \/ ProofNodeDirectlyImportsSubstantivenessBlocker(n)
            \/ ProofNodeRepairsAggregateNodeBlockers(n)}

\* Updated 2026-05-05: proof_formalization now permits closed-proof
\* nodes that directly carry a blocker (`ProofNodeRepairsBlocker`),
\* directly import a substantiveness-blocked node
\* (`ProofNodeDirectlyImportsSubstantivenessBlocker`), or are a minimal
\* common importer of all live node-bound blocked nodes
\* (`ProofNodeRepairsAggregateNodeBlockers`). The `node \in liveOpen`
\* clause covers the worker-drives-sorry case; the other clauses cover
\* blocker-recovery focuses where the worker's task is to repair other
\* nodes within scope.
\*
\* Snapshot-asymmetry caveat: the `livePresent` and `liveOpen`
\* parameters are honored, but `ProofNodeRepairsBlocker` and
\* `ProofNodeRepairsAggregateNodeBlockers` read unprimed live state
\* (`presentNodes`, `currentDeps`, `corrStatus`, etc.) regardless of
\* which snapshot the caller supplies. This matches the kernel
\* (`proof_node_repairs_blocker` reads `self.live.*` independent of
\* the `snapshot` parameter), and is also harmless in current Rust
\* callsites because they all pass `&self.live`. In the spec, however,
\* `ActiveNodeLegal` is also invoked with `committedPresentNodes` /
\* `committedOpenNodes` (worker-reject paths around lines ~5283,
\* ~5712, etc.); for those callers, the new blocker-repair clauses
\* reflect the pre-restore lane state, which can disagree with the
\* post-restore lane state when verifier `*_current_fp` mirrors are
\* restored but `*_status` / `*_approved_fp` are not. The current
\* invariants (PendingTaskConsistent, etc.) do not require
\* `ActiveNodeLegal(activeNode, …)`, so the divergence does not cause
\* TLC to fire a counterexample — but it is a modeling-fidelity gap
\* worth lifting if a future invariant turns post-restore activeNode
\* legality into a checked property.
\* Patch C extension (LOCAL_CLOSURE_IMPL_PLAN.md §7.4): a proof-phase
\* active node is also legal if it lies in `localClosureUnverified`,
\* mirroring the kernel's plan to plumb the unverified set into the
\* same scheduling predicates that `live.open_nodes` already drives.
\* The unverified set is sorry-free-only (§7.2) so this disjunct adds
\* candidates that are not in `liveOpen`. `ActiveNodeLegal` is called
\* with both live and committed snapshots; reading `localClosureUnverified`
\* unprimed mirrors the plan's "live tier" semantics and matches how the
\* existing `ProofNodeRepairsBlocker` family already reads unprimed live
\* state (see snapshot-asymmetry caveat above).
\* Proposal v32: in proof_formalization with a coarse anchor set, the
\* active node must also lie in CoarseLegalActiveSet. The new conjunct
\* reads global `activeCoarseNode` (same pattern as `GlobalBlockers`),
\* so the snapshot-asymmetry caveat above applies here too: callers
\* passing committedPresent/committedOpen still see the live coarse
\* anchor and live blocker set. When coarseDagNodes = {} the cone set
\* defaults to presentNodes, preserving legacy behavior.
ActiveNodeLegal(livePhase, node, livePresent, liveOpen) ==
    node = NoNode
    \/
    IF livePhase \in {"cleanup", "theorem_stating"} THEN
        node \in livePresent
    ELSE
        /\ node \in livePresent
        /\ \/ node \in liveOpen
           \/ node \in localClosureUnverified
           \/ ProofNodeRepairsBlocker(node)
           \/ ProofNodeDirectlyImportsSubstantivenessBlocker(node)
           \/ ProofNodeRepairsAggregateNodeBlockers(node)
        /\ node \in CoarseLegalActiveSet

CurrentFailedBlockers ==
    {b \in GlobalBlockers :
        \/ /\ b.kind = "node_corr"
           /\ CurrentNodeCorrFail(b.object.node)
        \/ /\ b.kind = "paper_faithfulness"
           /\ CurrentPaperFail(b.object.target)
        \/ /\ b.kind = "substantiveness"
           /\ CurrentSubstantivenessFail(b.object.node)
        \/ /\ b.kind = "soundness"
           /\ CurrentSoundFail(b.object.node)
        \/ /\ b.kind = "deviation"
           /\ CurrentDeviationFail(b.object.deviation)}

ResetNodeCorrNodes(blockers) ==
    {n \in Nodes : \E b \in blockers: /\ b.kind = "node_corr" /\ b.object.node = n}

ResetPaperTargets(blockers) ==
    {t \in Targets : \E b \in blockers: /\ b.kind = "paper_faithfulness" /\ b.object.target = t}

ResetSubstantivenessNodes(blockers) ==
    {n \in Nodes : \E b \in blockers: /\ b.kind = "substantiveness" /\ b.object.node = n}

ResetSoundNodes(blockers) ==
    {n \in Nodes : \E b \in blockers: /\ b.kind = "soundness" /\ b.object.node = n}

\* Deviation ids targeted by a reviewer reset block. Mirror of the
\* equivalent Node/Target/Sound reset families. The kernel applies
\* `apply_review_blocker_resets` (model.rs:9354-9362 area) which
\* zeros `deviation_status[id] = Unknown` and drops the approved
\* fingerprint, forcing a fresh verifier pass.
ResetDeviationIds(blockers) ==
    {id \in Deviations :
        \E b \in blockers : /\ b.kind = "deviation" /\ b.object.deviation = id}

ApplyReviewCorrStatusResets(corrMap, blockers) ==
    [n \in Nodes |->
        IF n \in ResetNodeCorrNodes(blockers) THEN "unknown" ELSE corrMap[n]]

ApplyReviewPaperStatusResets(targetMap, blockers) ==
    [t \in Targets |->
        IF t \in ResetPaperTargets(blockers) THEN "unknown" ELSE targetMap[t]]

ApplyReviewSubstantivenessStatusResets(nodeMap, blockers) ==
    [n \in Nodes |->
        IF n \in ResetSubstantivenessNodes(blockers) THEN "unknown" ELSE nodeMap[n]]

ApplyReviewSoundStatusResets(soundMap, blockers) ==
    [n \in Nodes |->
        IF n \in ResetSoundNodes(blockers) THEN "unknown" ELSE soundMap[n]]

AdjudicatedNodeCorrPassNodes(blockers) ==
    {n \in Nodes :
        /\ n \in latestCorrReviewNodes
        /\ CurrentNodeCorrUnknown(n)
        /\ \E b \in blockers: /\ b.kind = "node_corr" /\ b.object.node = n}

AdjudicatedPaperPassTargets(blockers) ==
    {t \in Targets :
        /\ t \in latestPaperReviewTargets
        /\ CurrentPaperUnknown(t)
        /\ \E b \in blockers: /\ b.kind = "paper_faithfulness" /\ b.object.target = t}

AdjudicatedSubstantivenessPassNodes(blockers) ==
    {n \in Nodes :
        /\ n \in latestSubstantivenessReviewNodes
        /\ CurrentSubstantivenessUnknown(n)
        /\ \E b \in blockers: /\ b.kind = "substantiveness" /\ b.object.node = n}

AdjudicatedSoundPassNodes(blockers) ==
    {n \in Nodes :
        /\ n \in latestSoundReviewNodes
        /\ CurrentSoundUnknown(n)
        /\ \E b \in blockers: /\ b.kind = "soundness" /\ b.object.node = n}

\* Option C (2026-06-04): the override→Pass arm has been retired across
\* all four lanes. Adjudication is now task-only (→Fail). See
\* REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md.
ApplyReviewCorrStatusAdjudications(corrMap, taskBlockers) ==
    [n \in Nodes |->
        IF n \in AdjudicatedNodeCorrPassNodes(taskBlockers) THEN
            "fail"
        ELSE
            corrMap[n]]

ApplyReviewPaperStatusAdjudications(targetMap, taskBlockers) ==
    [t \in Targets |->
        IF t \in AdjudicatedPaperPassTargets(taskBlockers) THEN
            "fail"
        ELSE
            targetMap[t]]

ApplyReviewSubstantivenessStatusAdjudications(nodeMap, taskBlockers) ==
    [n \in Nodes |->
        IF n \in AdjudicatedSubstantivenessPassNodes(taskBlockers) THEN
            "fail"
        ELSE
            nodeMap[n]]

ApplyReviewSoundStatusAdjudications(soundMap, taskBlockers) ==
    [n \in Nodes |->
        IF n \in AdjudicatedSoundPassNodes(taskBlockers) THEN
            "fail"
        ELSE
            soundMap[n]]

\* Rich-taxonomy mirror of `ApplyReviewSoundStatusAdjudications`. When
\* the reviewer pins a Soundness task blocker, the kernel sets the
\* assessment to `reviewer_pinned_fail` (model.rs site in
\* `apply_blocker_assignment` -> `(BlockerObject::Node, BlockerKind::
\* Soundness)`). Reset blockers (kernel `apply_review_blocker_resets`)
\* zero the stored assessment back to `fresh_unknown`.
ApplyReviewSoundAssessmentResets(assessmentMap, blockers) ==
    [n \in Nodes |->
        IF n \in ResetSoundNodes(blockers) THEN "fresh_unknown"
        ELSE assessmentMap[n]]

ApplyReviewSoundAssessmentAdjudications(assessmentMap, taskBlockers) ==
    [n \in Nodes |->
        IF n \in AdjudicatedSoundPassNodes(taskBlockers) THEN
            "reviewer_pinned_fail"
        ELSE
            assessmentMap[n]]

\* Reset helpers below clear approvedFp alongside the status reset, matching
\* the kernel's `apply_review_blocker_resets` (model.rs:2663), which removes
\* the node/target from `*_approved_fingerprints` so the next verify pass
\* pins a fresh baseline.
ApplyReviewCorrApprovedFpResets(fpMap, blockers) ==
    [n \in Nodes |->
        IF n \in ResetNodeCorrNodes(blockers) THEN NoFingerprint ELSE fpMap[n]]

ApplyReviewPaperApprovedFpResets(fpMap, blockers) ==
    [t \in Targets |->
        IF t \in ResetPaperTargets(blockers) THEN NoFingerprint ELSE fpMap[t]]

ApplyReviewSubstantivenessApprovedFpResets(fpMap, blockers) ==
    [n \in Nodes |->
        IF n \in ResetSubstantivenessNodes(blockers) THEN NoFingerprint ELSE fpMap[n]]

ApplyReviewSoundApprovedFpResets(fpMap, blockers) ==
    [n \in Nodes |->
        IF n \in ResetSoundNodes(blockers) THEN NoFingerprint ELSE fpMap[n]]

\* Adjudication helpers below pin approvedFp = currentFp when the reviewer
\* resolves a split panel via task_blockers (→Fail). Matches the kernel's
\* `apply_review_blocker_adjudications` (model.rs) which writes approvedFp
\* unconditionally on the task→Fail verdict. Without this pin, the
\* reviewer-adjudicated Fail would be inert (`current_*_fail` requires
\* status=Fail AND current==approved).
\*
\* Option C (2026-06-04): the override→Pass arm has been retired; only
\* task→Fail remains.
ApplyReviewCorrApprovedFpAdjudications(fpMap, currentFp, taskBlockers) ==
    [n \in Nodes |->
        IF n \in AdjudicatedNodeCorrPassNodes(taskBlockers)
        THEN currentFp[n] ELSE fpMap[n]]

ApplyReviewPaperApprovedFpAdjudications(fpMap, currentFp, taskBlockers) ==
    [t \in Targets |->
        IF t \in AdjudicatedPaperPassTargets(taskBlockers)
        THEN currentFp[t] ELSE fpMap[t]]

ApplyReviewSubstantivenessApprovedFpAdjudications(fpMap, currentFp, taskBlockers) ==
    [n \in Nodes |->
        IF n \in AdjudicatedSubstantivenessPassNodes(taskBlockers)
        THEN currentFp[n] ELSE fpMap[n]]

ApplyReviewSoundApprovedFpAdjudications(fpMap, currentFp, taskBlockers) ==
    [n \in Nodes |->
        IF n \in AdjudicatedSoundPassNodes(taskBlockers)
        THEN currentFp[n] ELSE fpMap[n]]

\* ----------------------------------------------------------------------
\* Deviation lane reviewer-adjudication helpers
\* ----------------------------------------------------------------------
\* Mirror of the corresponding helpers for other lanes. The kernel
\* `apply_review_blocker_adjudications` (model.rs) routes blockers in
\* `task_blockers` to Fail (task→Fail). Adjudicable ids must (a) be in
\* `latestDeviationReviewIds` and (b) be currently Fail (kernel
\* `current_deviation_fail` + the `review_blocker_adjudicable`
\* Deviation arm). Note this differs from the other lanes' "adjudicable
\* iff Unknown" rule — Deviation's task→Fail is a no-op (effectively a
\* reset-on-current-fingerprint pin).
\*
\* Option C (2026-06-04): the override→Pass arm has been retired;
\* deviation adjudication is task-only.
\*
\* `ResetDeviationStatusResets` etc. are the reset partition's
\* counterpart: an id in the reset block gets `deviationStatus[id]
\* := unknown` and approvedFp cleared so the next verifier pass
\* pins a fresh baseline (mirror of the kernel reset block in
\* model.rs).
AdjudicatedDeviationPassIds(blockers) ==
    {id \in Deviations :
        /\ id \in latestDeviationReviewIds
        /\ CurrentDeviationFail(id)
        /\ \E b \in blockers : /\ b.kind = "deviation" /\ b.object.deviation = id}

ApplyReviewDeviationStatusResets(statusMap, blockers) ==
    [id \in Deviations |->
        IF id \in ResetDeviationIds(blockers) THEN "unknown" ELSE statusMap[id]]

ApplyReviewDeviationApprovedFpResets(fpMap, blockers) ==
    [id \in Deviations |->
        IF id \in ResetDeviationIds(blockers) THEN NoFingerprint ELSE fpMap[id]]

ApplyReviewDeviationStatusAdjudications(statusMap, taskBlockers) ==
    [id \in Deviations |->
        IF id \in AdjudicatedDeviationPassIds(taskBlockers) THEN
            "fail"
        ELSE
            statusMap[id]]

ApplyReviewDeviationApprovedFpAdjudications(fpMap, currentFp, taskBlockers) ==
    [id \in Deviations |->
        IF id \in AdjudicatedDeviationPassIds(taskBlockers)
        THEN currentFp[id] ELSE fpMap[id]]

TheoremNodeHasOpenBlocker(n) ==
    /\ n \in presentNodes
    /\ (~CurrentNodeCorrPass(n)
        \/ (NeedsSound(n) /\ ~CurrentSoundPass(n)))

TheoremNodeHasCurrentFailBlocker(n) ==
    /\ n \in presentNodes
    /\ (CurrentNodeCorrFail(n)
        \/ (NeedsSound(n) /\ CurrentSoundFail(n)))

TheoremTargetedModeLegal(node) ==
    /\ node # NoNode
    /\ IF BlockedTargets # {} THEN
            \E t \in BlockedTargets: node \in TargetSupportCone(t, currentCoverage, presentNodes)
       ELSE
            TheoremNodeHasCurrentFailBlocker(node)

TheoremReviewNextActiveNodeAllowed(node) ==
    IF BlockedTargets # {} THEN
        \E t \in BlockedTargets: node \in TargetSupportCone(t, currentCoverage, presentNodes)
    ELSE IF CorrVerifyNodes # {} THEN
        TheoremNodeHasCurrentFailBlocker(node)
    ELSE IF SelectedTheoremHeldTarget # NoNode THEN
        node \in DepClosure({SelectedTheoremHeldTarget}, presentNodes)
    ELSE
        ActiveNodeLegal("theorem_stating", node, presentNodes, openNodes)

SoundVerifyNodes ==
    IF phase = "theorem_stating" THEN
        IF /\ SelectedTheoremHeldTarget # NoNode
           /\ CurrentSoundUnknown(SelectedTheoremHeldTarget)
        THEN
            {SelectedTheoremHeldTarget}
        ELSE
            {}
    ELSE IF phase = "proof_formalization" THEN
        \* Every present node whose current sound state is Unknown — not
        \* just activeNode. Covers newly-added helpers and drift-induced
        \* Unknowns (status=Pass but current_fp ≠ approved) reachable
        \* under CoarseRestructure mode.
        \*
        \* Cross-lane gate (kernel commit 63cd53b, `sound_verify_nodes`
        \* in model.rs): Sound dispatch is suppressed in
        \* ProofFormalization while any non-Sound verifier lane has
        \* open work — mirrors `corr_blockers_exist()` in the kernel,
        \* captured here by `CorrespondenceBlockersExist`. This avoids
        \* dispatching Sound runs that would observe stale corr/paper/
        \* substantiveness statements; both auto-dispatch and
        \* reviewer-requested verifications are gated.
        IF CorrespondenceBlockersExist THEN
            {}
        ELSE
            {n \in presentNodes : NeedsSound(n) /\ CurrentSoundUnknown(n)}
    ELSE
        {}

\* Post-paper-accept routing helper. Mirrors the kernel's
\* `route_non_adjudicable_unknown_verifier`: when a paper/substantiveness
\* Fail would normally route to Reviewer, a non-adjudicable Unknown with a
\* live verifier frontier must drain first. These operators are parameterized
\* by the post-response maps for the paper/substantiveness lanes because the
\* paper-accept actions compute their next status/fingerprint maps locally
\* before choosing the next stage.
PostPaperUnknownTargets(targetMap, targetApprovedMap) ==
    {t \in configuredTargets :
        /\ currentCoverage[t] # {}
        /\ ~(targetMap[t] = "pass" /\ paperCurrentFp[t] = targetApprovedMap[t])
        /\ ~(targetMap[t] = "fail" /\ paperCurrentFp[t] = targetApprovedMap[t])}

PostSubstantivenessUnknownNodes(nodeMap, nodeApprovedMap) ==
    IF phase \in {"theorem_stating", "proof_formalization"} THEN
        {n \in presentNodes :
            /\ ~(nodeMap[n] = "pass" /\ substantivenessCurrentFp[n] = nodeApprovedMap[n])
            /\ ~(nodeMap[n] = "fail" /\ substantivenessCurrentFp[n] = nodeApprovedMap[n])}
    ELSE
        {}

PostCorrVerifyNodes(nodeMap, nodeApprovedMap) ==
    IF phase = "theorem_stating" THEN
        {n \in presentNodes :
            /\ CurrentNodeCorrUnknown(n)
            /\ nodeMap[n] = "pass"
            /\ substantivenessCurrentFp[n] = nodeApprovedMap[n]}
    ELSE
        {n \in presentNodes : CurrentNodeCorrUnknown(n)}

PostCorrVerifyNodesWithCorrMaps(corrMap, corrApprovedMap, nodeMap, nodeApprovedMap) ==
    IF phase = "theorem_stating" THEN
        {n \in presentNodes :
            /\ PostNodeCorrUnknown(n, corrMap, corrApprovedMap)
            /\ nodeMap[n] = "pass"
            /\ substantivenessCurrentFp[n] = nodeApprovedMap[n]}
    ELSE
        {n \in presentNodes : PostNodeCorrUnknown(n, corrMap, corrApprovedMap)}

PostSoundVerifyNodes(selectedHeldTarget) ==
    IF phase = "theorem_stating" THEN
        IF /\ selectedHeldTarget # NoNode
           /\ CurrentSoundUnknown(selectedHeldTarget)
        THEN
            {selectedHeldTarget}
        ELSE
            {}
    ELSE IF phase = "proof_formalization" THEN
        \* TODO: kernel commit 63cd53b gates `sound_verify_nodes` on
        \* `~corr_blockers_exist()`. Spec's `CorrespondenceBlockersExist`
        \* reads pre-accept paper/substantiveness state; the kernel
        \* computes against post-accept state. Passing the updated
        \* paper/substantiveness maps through this helper would let us
        \* mirror the kernel exactly; for now we model the more
        \* permissive frontier (every Unknown sound node), which is a
        \* safe over-approximation for non-adjudicable-frontier
        \* membership.
        {n \in presentNodes : NeedsSound(n) /\ CurrentSoundUnknown(n)}
    ELSE
        {}

PostNonAdjudicablePaperOrSubstantivenessFrontier(targetMap, targetApprovedMap, nodeMap, nodeApprovedMap, latestPaperTargets, latestSubstantivenessNodes) ==
    \/ (PostPaperUnknownTargets(targetMap, targetApprovedMap) \ latestPaperTargets) # {}
    \/ (PostSubstantivenessUnknownNodes(nodeMap, nodeApprovedMap) \ latestSubstantivenessNodes) # {}

PostNonAdjudicableCorrFrontier(nodeMap, nodeApprovedMap, latestCorrNodes) ==
    (PostCorrVerifyNodes(nodeMap, nodeApprovedMap) \ latestCorrNodes) # {}

PostNonAdjudicableCorrFrontierWithCorrMaps(corrMap, corrApprovedMap, nodeMap, nodeApprovedMap, latestCorrNodes) ==
    (PostCorrVerifyNodesWithCorrMaps(corrMap, corrApprovedMap, nodeMap, nodeApprovedMap) \ latestCorrNodes) # {}

PostNonAdjudicableSoundFrontier(selectedHeldTarget, latestSoundNodes) ==
    (PostSoundVerifyNodes(selectedHeldTarget) \ latestSoundNodes) # {}

RequestSoundVerifyNode(kind) ==
    \* A Sound request verifies exactly one node per dispatch. When multiple
    \* present nodes are Unknown on soundness (e.g. the active node plus
    \* helpers added by a restructure worker), they are sequenced: this
    \* request covers one, and the remaining Unknowns re-surface on the
    \* next Sound request after this one's response lands.
    IF kind \in {"sound"} /\ SoundVerifyNodes # {} THEN
        CHOOSE n \in SoundVerifyNodes: TRUE
    ELSE
        NoNode

RequestSoundVerifyNodes(kind) ==
    IF RequestSoundVerifyNode(kind) # NoNode THEN
        {RequestSoundVerifyNode(kind)}
    ELSE
        {}

RequestRuntimeSupportRequired(kind) ==
    \* Mirror of kernel `RequestKind::requires_runtime_support`
    \* (model.rs). Audit + StuckMathAudit added with the cleanup-v2
    \* audit lane / StuckMathAudit role respectively.
    kind \in {"worker", "paper", "corr", "sound", "review", "audit", "stuck_math_audit"}

RequestBlockers(kind) ==
    IF kind = "worker" THEN
        pendingTask.taskBlockers
    ELSE
        GlobalBlockers

RequestVerifyNodes(kind) ==
    IF kind = "paper" THEN
        {}
    ELSE IF kind = "corr" THEN
        CorrVerifyNodes
    ELSE IF kind = "sound" THEN
        SoundVerifyNodes
    ELSE
        {}

RequestVerifyTargets(kind) ==
    IF kind = "paper" THEN
        PaperVerifyTargets
    ELSE
        {}

RequestCorrVerifyNodes(kind) ==
    IF kind \in {"corr"} THEN
        CorrVerifyNodes
    ELSE
        {}

RequestPaperVerifyTargets(kind) ==
    \* Per-cycle scheduling rule (mirror of kernel
    \* `request_paper_verify_targets`, model.rs:3107): when a Paper
    \* request fires, choose the target frontier first; only when it's
    \* empty do we dispatch the per-node frontier. Both share the same
    \* RequestKind, distinguished by which set is non-empty.
    IF kind \in {"paper"} /\ PaperVerifyTargets # {} THEN
        PaperVerifyTargets
    ELSE
        {}

\* Substantiveness frontier for the in-flight Paper request.
\* Empty unless the kernel selected the per-node scenario, which it does
\* iff the target frontier is empty AND no deviation-verify id is
\* selected AND the per-node frontier is non-empty (theorem-stating +
\* proof-formalization; cleanup is dormant). Mirror of kernel
\* `request_substantiveness_verify_nodes` in model.rs.
\*
\* Deviation scenario gating (kernel commit 7aad7cb): a Paper request
\* with a `deviation_verify_id` set is a deviation-authorization run,
\* not a substantiveness frontier drain. The spec abstracts the
\* deviation lane via env actions; here we exclude substantiveness
\* when at least one tracked Unknown-state deviation exists, mirroring
\* `request_deviation_verify_id`'s prioritization.
RequestPaperVerifyNodes(kind) ==
    IF kind \in {"paper"}
       /\ PaperVerifyTargets = {}
       /\ DeviationVerifyIds = {}
    THEN
        SubstantivenessVerifyNodes
    ELSE
        {}

RequestCorrVerifyTargets(kind) ==
    {}

RequestVerifyLanes(kind) ==
    IF kind \in {"paper", "corr", "sound"} THEN
        VerifierLanes
    ELSE
        {}

RequestPaperVerifyLaneBindings(kind) ==
    IF kind \in {"paper"} THEN
        LaneBindings(SoundVerifierBindingByLane, RequestVerifyLanes(kind))
    ELSE
        {}

RequestCorrVerifyLaneBindings(kind) ==
    IF kind \in {"corr"} THEN
        LaneBindings(CorrVerifierBindingByLane, RequestVerifyLanes(kind))
    ELSE
        {}

RequestSoundVerifyLaneBindings(kind) ==
    IF kind \in {"sound"} THEN
        LaneBindings(SoundVerifierBindingByLane, RequestVerifyLanes(kind))
    ELSE
        {}

RequestAllowedDecisions(kind) ==
    IF kind # "review" THEN
        {}
    ELSE IF phase = "theorem_stating" THEN
        IF retryOutcomeKind # "none" THEN
            {"CONTINUE", "NEED_INPUT"}
        ELSE
            {"CONTINUE", "ADVANCE_PHASE", "NEED_INPUT"}
    ELSE IF phase = "proof_formalization" THEN
        {"CONTINUE", "NEED_INPUT"}
    ELSE IF phase = "cleanup" THEN
        \* Cleanup phase invariant: every accepted state is Done-valid
        \* (formalization_complete). NeedInput is NOT in the allowed set
        \* — there's nothing to escalate. AdvancePhase is rejected because
        \* cleanup is the last work phase. Cleanup-v2 (kernel commit
        \* eb7d190 / audit Finding 2): once `cleanupForceDone` latches,
        \* Continue is also removed — the reviewer's only legal move is
        \* Done. Mirror of `request_allowed_decisions` in kernel
        \* model.rs.
        IF cleanupForceDone THEN
            {"DONE"}
        ELSE
            {"CONTINUE", "DONE"}
    ELSE
        {}

RequestAllowedNextActiveNodes(kind) ==
    IF kind # "review" THEN
        {}
    ELSE IF phase = "theorem_stating" THEN
        IF retryOutcomeKind = "invalid" THEN
            {}
        ELSE
            {n \in presentNodes : TheoremReviewNextActiveNodeAllowed(n)}
    ELSE
        {n \in presentNodes : ActiveNodeLegal(phase, n, presentNodes, openNodes)}

RequestTargetedNextActiveNodes(kind) ==
    IF /\ kind = "review"
       /\ phase = "theorem_stating"
       /\ retryOutcomeKind # "invalid"
    THEN
        {n \in presentNodes : TheoremTargetedModeLegal(n)}
    ELSE
        {}

RequestAllowedNextModes(kind) ==
    IF kind # "review" THEN
        {}
    ELSE IF phase = "theorem_stating" THEN
        IF retryOutcomeKind = "invalid" THEN
            {CurrentMode}
        ELSE IF RequestTargetedNextActiveNodes(kind) = {} THEN
            {"global"}
        ELSE
            {"global", "targeted"}
    ELSE IF phase = "proof_formalization" THEN
        ProofEditModes
    ELSE IF phase = "cleanup" THEN
        {"cleanup"}
    ELSE
        {}

RequestAllowTargetedWithoutNextActive(kind) ==
    /\ kind = "review"
    /\ phase = "theorem_stating"
    /\ retryOutcomeKind = "invalid"
    /\ CurrentMode = "targeted"

RequestAllowedResets(kind) ==
    IF kind # "review" THEN
        {}
    ELSE
        ReviewResetChoices(phase, retryOutcomeKind)

RequestAllowedResetBlockers(kind) ==
    IF /\ kind = "review"
       /\ phase = "theorem_stating"
    THEN
        CurrentFailedBlockers
    ELSE
        {}

RequestAllowedResetBlockerIds(kind) ==
    {BlockerChoiceId(b) : b \in RequestAllowedResetBlockers(kind)}

\* Adjudicability per kernel `review_blocker_adjudicable` (model.rs).
\* Used by `has_non_adjudicable_unknown_blocker` (verifier preempt) and
\* by `review_task_blocker_forwardable` (sound task forwarding).
\*
\* Option C (2026-06-04): the override→Pass apply path is retired. The
\* predicate itself stays because it gates the Sound task-forwarding
\* and the verifier-preempt routing in
\* `route_non_adjudicable_unknown_verifier`. The previous
\* override→Pass direction (Unknown→Pass for four lanes, Fail→Pass for
\* Deviation) no longer fires.
ReviewBlockerAdjudicable(b) ==
    \/ /\ b.kind = "node_corr"
       /\ b.object.node \in latestCorrReviewNodes
       /\ CurrentNodeCorrUnknown(b.object.node)
    \/ /\ b.kind = "substantiveness"
       /\ b.object.node \in latestSubstantivenessReviewNodes
       /\ CurrentSubstantivenessUnknown(b.object.node)
    \/ /\ b.kind = "paper_faithfulness"
       /\ b.object.target \in latestPaperReviewTargets
       /\ CurrentPaperUnknown(b.object.target)
    \/ /\ b.kind = "soundness"
       /\ b.object.node \in latestSoundReviewNodes
       /\ CurrentSoundUnknown(b.object.node)
    \/ /\ b.kind = "deviation"
       /\ b.object.deviation \in latestDeviationReviewIds
       /\ CurrentDeviationFail(b.object.deviation)

\* Option C (2026-06-04): retired. The reviewer's allowed_override_blockers
\* contract field is now always empty. Retained as an operator returning
\* `{}` for back-compat with the wrapper-request shape; new spec code
\* should not reference it.
RequestAllowedOverrideBlockers(kind) ==
    {}

RequestAllowedOverrideBlockerIds(kind) ==
    {}

RequestReviewBlockerChoices(kind) ==
    {BlockerChoice(b) : b \in RequestBlockers(kind)}

RequestAllowedDifficultyUpdateNodes(kind) ==
    IF kind = "review" THEN
        presentNodes
    ELSE
        {}

\* The broad envelope of nodes the reviewer COULD authorize given
\* nextActive/nextMode. For proof Restructure/CoarseRestructure the
\* reviewer must pick an explicit `authorizedNodes` subset of this
\* envelope; the worker's edit permission is then exactly that subset
\* (see `EffectiveAuthorizedNodes` below).
ReviewScopeEnvelope(nextMode, nextActive) ==
    LET focus == IF nextActive # NoNode THEN nextActive ELSE activeNode
    IN
        IF phase = "theorem_stating" /\ nextMode = "global" THEN
            presentNodes
        ELSE IF phase = "cleanup" THEN
            presentNodes
        ELSE IF \/ /\ phase = "theorem_stating"
                   /\ nextMode = "targeted"
                \/ /\ phase = "proof_formalization"
                   /\ nextMode \in {"restructure", "coarse_restructure"}
        THEN
            IF focus = NoNode THEN {} ELSE ImpactRegion(focus, presentNodes)
        ELSE
            {}

\* Audit-gated global repair can widen proof-formalization
\* Restructure/CoarseRestructure authorization beyond the current active
\* coarse cone. Retry state must not remove this escape hatch; the grant is
\* what carries the out-of-cone authority.
GlobalRepairGrantedNodes(rsp) ==
    IF /\ phase = "proof_formalization"
       /\ rsp.consumeGlobalRepairGrant
       /\ pendingGlobalRepairGrant # NoGlobalRepairGrant
    THEN
        pendingGlobalRepairGrant.approvedExtensionNodes
    ELSE
        {}

ReviewScopeEnvelopeWithGlobalRepair(rsp) ==
    ReviewScopeEnvelope(rsp.nextMode, rsp.nextActive)
        \cup GlobalRepairGrantedNodes(rsp)

\* Effective authorized set used for blocker-coverage checks. For
\* proof Restructure/CoarseRestructure with a non-empty explicit list
\* in the response, the reviewer's `authorizedNodes` IS the worker's
\* edit permission; everything else falls back to the envelope.
EffectiveAuthorizedNodes(rsp) ==
    IF /\ phase = "proof_formalization"
       /\ rsp.nextMode \in {"restructure", "coarse_restructure"}
       /\ rsp.authorizedNodes # {}
    THEN
        rsp.authorizedNodes
    ELSE
        ReviewScopeEnvelope(rsp.nextMode, rsp.nextActive)

\* Backwards-compatible alias (kept so existing simulation traces and
\* invariant references continue to type-check).
ReviewAuthorizedNodes(nextMode, nextActive) ==
    ReviewScopeEnvelope(nextMode, nextActive)

ReviewTaskBlockerInWorkerScope(b, rsp) ==
    LET authorized == EffectiveAuthorizedNodes(rsp)
    IN
        IF b.object.otype = "node" THEN
            b.object.node \in authorized
        ELSE IF b.kind = "paper_faithfulness" THEN
            LET coverageNodes == {n \in presentNodes : b.object.target \in currentTargetClaims[n]}
            IN
                IF coverageNodes = {} THEN
                    /\ phase = "theorem_stating"
                    /\ rsp.nextMode = "global"
                ELSE
                    DepClosure(coverageNodes, presentNodes) \cap authorized # {}
        ELSE IF b.kind = "deviation" THEN
            \* Mirror of kernel `review_task_blocker_in_worker_scope`
            \* (model.rs:2947-2956). Deviation blockers are takeable
            \* only by:
            \*   - TheoremStating + nextMode global (`TheoremGlobal`
            \*     validation kind),
            \*   - ProofFormalization + nextMode ∈ {easy, local,
            \*     restructure, coarse_restructure} (any of the four
            \*     Proof modes).
            \* Cleanup and Complete refuse — a Failed deviation at
            \* Cleanup entry is a stuck state at the protocol level.
            \* The spec models worker scope by nextMode (the worker
            \* validation kind is derived from it in
            \* `CurrentWorkerValidationKind`).
            \*
            \* HINT — TLC traces that reach Cleanup with a Failed
            \* deviation will report a deadlock. That is the
            \* structurally-correct response, NOT a spec bug:
            \*   (1) The Cleanup invariant (PROCESS_SEMANTICS.md §4.6)
            \*       precludes Cleanup entry while any global blocker
            \*       is live — `global_blockers().is_empty()` is the
            \*       gate, and an unresolved Deviation is a global
            \*       blocker (see `DeviationBlockersFor`).
            \*   (2) Cleanup / FinalCleanup workers cannot introduce
            \*       fresh Deviation Fails — `deviation_requests`,
            \*       `deviation_deletions`, and `node_deviation_claims`
            \*       are dormant in their prompt schema
            \*       (`request_contracts.rs:1872-1888`).
            \* So a Cleanup-phase Deviation Fail = state corruption
            \* the invariant disallows. The spec's refusal to route a
            \* Deviation blocker into a Cleanup worker (no case for
            \* `phase = "cleanup"` below) is the correct response: no
            \* enabled action can drain it, hence the deadlock. If a
            \* trace forces this state, the trace constructed
            \* corruption — not a spec defect.
            \/ /\ phase = "theorem_stating"
               /\ rsp.nextMode = "global"
            \/ /\ phase = "proof_formalization"
               /\ rsp.nextMode \in {"easy", "local", "restructure", "coarse_restructure"}
        ELSE
            FALSE

\* New advance-gate request fields (kernel `request_approved_target_nodes`,
\* `request_approved_corr_fingerprints`, `current_paper_approved_fingerprints`,
\* `coarse_dag_nodes`). Replaces the retired `protectedNodes` /
\* `protectedSnapshot` mechanism. The kernel populates
\* approvedTargetNodes / approvedCorrFingerprints only for
\* proof-formalization worker requests (model.rs:2731-2737, 2743-2747);
\* currentPaperApprovedFp is always populated; coarseDagNodes is populated
\* for worker + review requests in any phase, snapshotted at the
\* theorem-stating -> proof-formalization advance gate.
\* SPEC GAP (post-2026-05-03): the kernel's `approved_target_nodes()`
\* (kernel/src/model.rs around line 2043) returns
\*     UNION {approvedCoverage[t] : t \in approvedConfiguredTargets}
\*       \cup approvedTargets.protected_closure_nodes
\* where the closure is the narrow Lean type-surface closure of the
\* covering nodes (project-defined definitions reached by walking each
\* covering node's Lean type signature, recursing into def values per
\* `scripts/lean_semantic_fingerprint.lean`'s closure policy; theorem
\* proof bodies are excluded). Frozen at AdvancePhase Approve from the
\* worker observation slot `live.protected_closure_nodes_per_target`.
\* The spec models only the coverage subset because the closure is
\* derived from a Lean-side observation that has no analog in the
\* protocol-level spec; the worker-acceptance reopen guard
\* (`paper_target_corr_reopen_guard_errors`) and `request_protected_*`
\* projections are correspondingly stricter in the kernel than below.
\* See SPEC_TODO.md for the full follow-up.
RequestApprovedTargetNodes(kind) ==
    IF /\ phase = "proof_formalization"
       /\ kind = "worker"
    THEN
        UNION {approvedCoverage[t] : t \in approvedConfiguredTargets}
    ELSE
        {}

RequestApprovedCorrFingerprints(kind) ==
    [n \in RequestApprovedTargetNodes(kind) |-> corrApprovedFp[n]]

RequestCurrentPaperApprovedFp(kind) ==
    paperApprovedFp

\* coarseDagNodes is the snapshot of presentNodes captured at the
\* theorem-stating -> proof-formalization advance gate. Surfaced on
\* worker + review requests in proof phase; empty otherwise.
RequestCoarseDagNodes(kind) ==
    IF kind \in {"worker", "review"} THEN
        coarseDagNodes
    ELSE
        {}

CurrentWorkerAuthorizedNodes ==
    IF OrphanCleanupActive THEN
        presentNodes
    ELSE IF phase = "theorem_stating" /\ targetEditMode = "global" THEN
        presentNodes
    ELSE IF CurrentWorkerValidationKind = "final_cleanup" THEN
        presentNodes
    ELSE IF activeNode = NoNode THEN
        {}
    ELSE IF phase = "theorem_stating" /\ targetEditMode = "targeted" THEN
        ImpactRegion(activeNode, presentNodes)
    \* Proof Restructure / CoarseRestructure: the explicit list in
    \* pendingTask.authorizedNodes IS the worker's edit permission. An
    \* empty list (only possible for legacy persisted tasks predating
    \* the explicit-list contract or for synthetic engine paths) falls
    \* back to the legacy impact-region envelope.
    ELSE IF CurrentWorkerValidationKind \in {"proof_restructure", "proof_coarse_restructure"} THEN
        IF pendingTask.authorizedNodes # {} THEN
            pendingTask.authorizedNodes
        ELSE
            ImpactRegion(activeNode, presentNodes)
    ELSE
        {}

CurrentWorkerValidationExecutionPlan ==
    IF CurrentWorkerValidationKind = "theorem_global" THEN
        <<
            [
                kind |-> "scoped_tablet",
                allowedNodesMode |-> "all_present",
                explicitNodes |-> {}
            ]
        >>
    ELSE IF CurrentWorkerValidationKind = "theorem_targeted" THEN
        <<
            [
                kind |-> "theorem_target_edit_scope",
                target |-> activeNode,
                initialScope |-> CurrentWorkerAuthorizedNodes
            ],
            [
                kind |-> "scoped_tablet",
                allowedNodesMode |-> "previous_or_explicit",
                explicitNodes |-> CurrentWorkerAuthorizedNodes
            ]
        >>
    ELSE IF CurrentWorkerValidationKind \in {
        "proof_local",
        "proof_restructure",
        "proof_coarse_restructure"
    } THEN
        \* Difficulty is advisory only. Scope comes from proof_edit_mode; the
        \* reviewer-chosen booleans independently decide whether new helper
        \* obligations may remain open and whether the active node must close.
        <<
            [
                kind |-> "proof_worker_delta",
                active |-> activeNode,
                mode |->
                    IF CurrentWorkerValidationKind = "proof_local" THEN
                        "local"
                    ELSE IF CurrentWorkerValidationKind = "proof_restructure" THEN
                        "restructure"
                    ELSE
                        "coarse_restructure",
                authorizedNodes |-> CurrentWorkerAuthorizedNodes,
                allowNewObligations |-> CurrentWorkerAllowNewObligations,
                mustCloseActive |-> CurrentWorkerMustCloseActive
            ]
        >>
    ELSE IF CurrentWorkerValidationKind = "cleanup" THEN
        <<
            [kind |-> "cleanup_preserving"]
        >>
    ELSE IF CurrentWorkerValidationKind = "final_cleanup" THEN
        <<
            [kind |-> "final_cleanup_preserving"]
        >>
    ELSE
        << >>

CurrentWorkerAcceptance ==
    [
        enabled |-> TRUE,
        validationKind |-> CurrentWorkerValidationKind,
        authorizedNodes |-> CurrentWorkerAuthorizedNodes,
        validationExecutionPlan |-> CurrentWorkerValidationExecutionPlan,
        requireExplicitTargetClaimsForNewNodes |-> TRUE,
        forbidTabletChangesWhenStuck |-> FALSE,
        observationPlan |->
            IF CurrentWorkerValidationKind = "theorem_global" THEN
                [
                    captureBeforeSnapshot |-> TRUE,
                    captureBeforeTabletContents |-> FALSE,
                    captureScopedTabletBaselineErrors |-> TRUE,
                    scopedTabletBaselineScope |-> "all_present",
                    captureImportsBefore |-> FALSE,
                    captureExpectedActiveHash |-> FALSE,
                    captureBaselineDeclarationHashes |-> FALSE,
                    captureBaselineCorrespondenceHashes |-> FALSE
                ]
            ELSE IF CurrentWorkerValidationKind = "theorem_targeted" THEN
                [
                    captureBeforeSnapshot |-> TRUE,
                    captureBeforeTabletContents |-> FALSE,
                    captureScopedTabletBaselineErrors |-> TRUE,
                    scopedTabletBaselineScope |-> "authorized_nodes",
                    captureImportsBefore |-> FALSE,
                    captureExpectedActiveHash |-> FALSE,
                    captureBaselineDeclarationHashes |-> FALSE,
                    captureBaselineCorrespondenceHashes |-> FALSE
                ]
            ELSE IF CurrentWorkerValidationKind \in {
                "proof_local",
                "proof_restructure",
                "proof_coarse_restructure"
            } THEN
                [
                    captureBeforeSnapshot |-> TRUE,
                    captureBeforeTabletContents |-> FALSE,
                    captureScopedTabletBaselineErrors |-> FALSE,
                    scopedTabletBaselineScope |-> "none",
                    captureImportsBefore |-> FALSE,
                    captureExpectedActiveHash |-> TRUE,
                    captureBaselineDeclarationHashes |-> FALSE,
                    captureBaselineCorrespondenceHashes |-> FALSE
                ]
            ELSE IF CurrentWorkerValidationKind = "cleanup" THEN
                [
                    captureBeforeSnapshot |-> TRUE,
                    captureBeforeTabletContents |-> TRUE,
                    captureScopedTabletBaselineErrors |-> FALSE,
                    scopedTabletBaselineScope |-> "none",
                    captureImportsBefore |-> FALSE,
                    captureExpectedActiveHash |-> FALSE,
                    captureBaselineDeclarationHashes |-> FALSE,
                    captureBaselineCorrespondenceHashes |-> FALSE
                ]
            ELSE IF CurrentWorkerValidationKind = "final_cleanup" THEN
                [
                    captureBeforeSnapshot |-> TRUE,
                    captureBeforeTabletContents |-> FALSE,
                    captureScopedTabletBaselineErrors |-> FALSE,
                    scopedTabletBaselineScope |-> "none",
                    captureImportsBefore |-> FALSE,
                    captureExpectedActiveHash |-> FALSE,
                    captureBaselineDeclarationHashes |-> TRUE,
                    captureBaselineCorrespondenceHashes |-> TRUE
                ]
            ELSE
                [
                    captureBeforeSnapshot |-> FALSE,
                    captureBeforeTabletContents |-> FALSE,
                    captureScopedTabletBaselineErrors |-> FALSE,
                    scopedTabletBaselineScope |-> "none",
                    captureImportsBefore |-> FALSE,
                    captureExpectedActiveHash |-> FALSE,
                    captureBaselineDeclarationHashes |-> FALSE,
                    captureBaselineCorrespondenceHashes |-> FALSE
                ]
    ]

WorkerResponse(currentCycle, outcome, livePresent, liveOpen, liveCoverage, liveCorrCurrent, livePaperCurrent, liveSoundCurrent, liveTargetFp) ==
    LET observedDeps ==
            [n \in Nodes |->
                IF n \in livePresent THEN
                    (Deps[n] \cap livePresent) \ {n}
                ELSE
                    {}
            ]
        observedNodeKinds ==
            [n \in Nodes |->
                IF n \in livePresent THEN
                    NodeKinds[n]
                ELSE
                    "definition"
            ]
    IN [
        status |-> "ok",
        kind |-> "worker",
        cycle |-> currentCycle,
        workerOutcome |-> outcome,
        validationStepResults |-> SuccessfulValidationStepResults(CurrentWorkerValidationExecutionPlan),
        present |-> livePresent,
        open |-> liveOpen,
        coverage |-> liveCoverage,
        corrCurrent |-> liveCorrCurrent,
        paperCurrent |-> livePaperCurrent,
        soundCurrent |-> liveSoundCurrent,
        targetFp |-> liveTargetFp,
        corrMap |-> DefaultCorrUpdate,
        paperMap |-> DefaultPaperUpdate,
        substantivenessMap |-> DefaultSubstantivenessUpdate,
        soundMap |-> DefaultSoundUpdate,
        corrLaneMaps |-> DefaultCorrLaneMaps,
        paperLaneMaps |-> DefaultPaperLaneMaps,
        substantivenessLaneMaps |-> DefaultSubstantivenessLaneMaps,
        soundLaneMaps |-> DefaultSoundLaneMaps,
        paperPanelSplit |-> FALSE,
        substantivenessPanelSplit |-> FALSE,
        corrPanelSplit |-> FALSE,
        soundPanelSplit |-> FALSE,
        proofNodeMap |->
            [n \in Nodes |->
                IF (n \in currentProofNodes) # (observedNodeKinds[n] = "proof") THEN
                    IF observedNodeKinds[n] = "proof" THEN "proof" ELSE "not_proof"
                ELSE
                    "same"
            ],
        nodeKindMap |->
            [n \in Nodes |->
                IF n \in livePresent /\ currentNodeKinds[n] # observedNodeKinds[n] THEN
                    observedNodeKinds[n]
                ELSE
                    "same"
            ],
        depMap |->
            [n \in Nodes |->
                IF currentDeps[n] = observedDeps[n] THEN SameSetUpdate ELSE SetUpdate(observedDeps[n])
            ],
        targetClaimMap |->
            [n \in Nodes |->
                IF n \in (livePresent \ presentNodes) THEN
                    SetUpdate(IF n \in livePresent THEN currentTargetClaims[n] \cap configuredTargets ELSE {})
                ELSE
                    SameSetUpdate
            ],
        difficultyMap |-> DefaultDifficultyUpdate,
        decision |-> "none",
        comments |-> "",
        taskBlockers |-> {},
        overrideBlockers |-> {},
        resetBlockers |-> {},
        nextActive |-> NoNode,
        \* Proposal v32: reviewer-chosen next active coarse anchor.
        \* Must be NoNode outside proof_formalization, on retry-review,
        \* or when ActiveCoarseChangeAllowed is FALSE. When set, must be
        \* a member of KernelHintedNextActiveCoarseNodes.
        nextActiveCoarse |-> NoNode,
        reset |-> NoCheckpoint,
        nextMode |-> "global",
        humanChoice |-> "none",
        clearHumanInput |-> FALSE,
        nextWorkerContextMode |-> "resume",
        paperFocusRanges |-> << >>,
        workStyleHint |-> "none",
        allowNewObligations |-> TRUE,
        mustCloseActive |-> FALSE,
        authorizedNodes |-> {}
    ]

RequestWorkerContext(kind) ==
    IF kind = "worker" THEN
        [
            enabled |-> TRUE,
            activeDifficulty |-> CurrentActiveDifficulty,
            activeEasyAttempts |-> CurrentActiveEasyAttempts,
            workerProfile |-> CurrentWorkerProfile,
            validationKind |-> CurrentWorkerValidationKind,
            authorizedNodes |-> CurrentWorkerAuthorizedNodes,
            allowNewObligations |-> CurrentWorkerAllowNewObligations,
            mustCloseActive |-> CurrentWorkerMustCloseActive,
            nextContextMode |-> pendingTask.nextWorkerContextMode,
            paperFocusRanges |-> pendingTask.paperFocusRanges,
            workStyleHint |-> pendingTask.workStyleHint,
            \* Step C presentational flag carried from pendingTask
            \* (kernel `state.pending_task.consumed_global_repair_grant`
            \* mirrored into `WrapperRequest.consumed_global_repair_grant`
            \* at request build time in model.rs ~10578).
            consumedGlobalRepairGrant |-> pendingTask.consumedGlobalRepairGrant
        ]
    ELSE
        NoWorkerContext

RequestFreshContext(kind) ==
    IF kind \in {"paper", "corr", "sound"} THEN
        TRUE
    ELSE IF kind = "worker" THEN
        ~(<<kind, phase>> \in nativeHistoryKinds) \/ (RequestWorkerContext(kind).nextContextMode = "fresh")
    ELSE IF kind = "review" THEN
        ~(<<kind, phase>> \in nativeHistoryKinds)
    ELSE
        FALSE

SchemePromptFragment(kind) ==
    IF kind \in {"paper", "corr", "sound"} THEN
        "common/TRELLIS_FORMALIZATION_SCHEME.md"
    ELSE IF RequestFreshContext(kind) THEN
        "common/TRELLIS_FORMALIZATION_SCHEME.md"
    ELSE
        "common/00_trellis_scheme_brief.md"

RequestHasPaperFaithfulnessBlockers(kind) ==
    \E b \in RequestBlockers(kind): b.kind = "paper_faithfulness"

RequestHasCorrespondenceBlockers(kind) ==
    \E b \in RequestBlockers(kind): b.kind = "node_corr"

RequestHasSoundnessBlockers(kind) ==
    \E b \in RequestBlockers(kind): b.kind = "soundness"

PaperVerifierScenarioFragments(kind) ==
    IF kind \in {"paper"} THEN
        IF previousPaperFindingLanes = {} THEN
            <<"verifier/paper_faithfulness/05_fresh_target_package.md">>
        ELSE
            <<"verifier/paper_faithfulness/05_revisit_target_package.md">>
    ELSE
        << >>

CorrVerifierScenarioFragments(kind) ==
    IF kind \in {"corr"} THEN
        (IF previousCorrFindingLanes = {} THEN
            <<"verifier/correspondence/05_frontier.md">>
         ELSE
            <<"verifier/correspondence/05_revisit_frontier.md">>)
        \o
        (IF "Preamble" \in RequestVerifyNodes(kind) THEN
            <<"verifier/correspondence/06_with_preamble.md">>
         ELSE
            << >>)
        \o
        <<"verifier/correspondence/07_scratchpad.md">>
    ELSE
        << >>

SoundVerifierScenarioFragments(kind) ==
    IF kind \in {"sound"} THEN
        (IF phase = "theorem_stating" THEN
            <<"verifier/soundness/05_theorem_target.md">>
         ELSE
            <<"verifier/soundness/05_proof_node.md">>)
        \o
        (IF previousSoundFindingLanes # {} THEN
            <<"verifier/soundness/06_revisit_target.md">>
         ELSE
            << >>)
    ELSE
        << >>

ProofWorkerScenarioFragments ==
    (IF CurrentWorkerValidationKind = "proof_restructure" THEN
        <<"worker/proof_formalization/05_scope_restructure.md">>
     ELSE IF CurrentWorkerValidationKind = "proof_coarse_restructure" THEN
        <<"worker/proof_formalization/05_scope_coarse_restructure.md">>
     ELSE
        <<"worker/proof_formalization/05_scope_local.md">>)
    \o
    <<IF CurrentWorkerAllowNewObligations THEN
        "worker/proof_formalization/06_gate_allow_new_obligations.md"
      ELSE
        "worker/proof_formalization/06_gate_no_new_obligations.md",
      IF CurrentWorkerMustCloseActive THEN
        "worker/proof_formalization/07_gate_must_close_active.md"
      ELSE
        "worker/proof_formalization/07_gate_active_may_remain_open.md">>

WorkerScenarioFragments(kind) ==
    IF kind = "worker" THEN
        IF CurrentWorkerProfile = "theorem" THEN
            IF RequestHasPaperFaithfulnessBlockers(kind) THEN
                <<"worker/theorem_stating/05_after_paper_faithfulness_review.md">>
            ELSE IF RequestHasCorrespondenceBlockers(kind) THEN
                <<"worker/theorem_stating/05_after_correspondence_review.md">>
            ELSE IF RequestHasSoundnessBlockers(kind) THEN
                <<"worker/theorem_stating/05_after_soundness_review.md">>
            ELSE
                <<"worker/theorem_stating/05_frontier_work.md">>
        ELSE IF CurrentWorkerProfile \in {"proof_easy", "proof_hard"} THEN
            ProofWorkerScenarioFragments
        ELSE IF CurrentWorkerProfile = "cleanup" THEN
            <<"worker/cleanup/05_orphan_cleanup_task.md">>
        ELSE IF CurrentWorkerProfile = "final_cleanup" THEN
            <<"worker/final_cleanup/05_task.md">>
        ELSE
            <<"worker/generic/05_task.md">>
    ELSE
        << >>

ReviewScenarioFragments(kind) ==
    IF kind = "review" THEN
        (<<
            IF retryOutcomeKind = "invalid" THEN
                "review/common/05_after_worker_invalid.md"
            ELSE IF retryOutcomeKind = "stuck" THEN
                "review/common/05_after_worker_stuck.md"
            ELSE IF retryOutcomeKind = "needs_restructure" THEN
                "review/common/05_after_worker_needs_restructure.md"
            ELSE IF RequestHasPaperFaithfulnessBlockers(kind) THEN
                IF latestPaperPanelSplit THEN
                    "review/common/05_after_split_paper_faithfulness.md"
                ELSE
                    "review/common/05_after_failed_paper_faithfulness.md"
            ELSE IF RequestHasCorrespondenceBlockers(kind) THEN
                IF latestCorrPanelSplit THEN
                    "review/common/05_after_split_correspondence.md"
                ELSE
                    "review/common/05_after_failed_correspondence.md"
            ELSE IF RequestHasSoundnessBlockers(kind) THEN
                IF latestSoundPanelSplit THEN
                    "review/common/05_after_split_soundness.md"
                ELSE
                    "review/common/05_after_failed_soundness.md"
            ELSE
                "review/common/05_after_clean_verification.md"
        >>)
        \o
        (IF humanInputOutstanding THEN
            <<"review/common/06_with_outstanding_human_input.md">>
         ELSE
            << >>)
    ELSE
        << >>

RequestPaperContract(kind) ==
    IF kind \in {"paper"} THEN
        [
            promptFragments |->
                <<SchemePromptFragment(kind),
                  "verifier/common/00_intro.md">>
                \o
                PaperVerifierScenarioFragments(kind)
                \o
                <<"shared/10_repository_root.md",
                  "verifier/common/10_lane_id.md",
                  "verifier/common/15_previous_findings.md",
                  "shared/20_read_files.md",
                  "shared/25_filespec.md",
                  "shared/30_project_invariants.md",
                  "verifier/paper_faithfulness/20_targets.md",
                  "verifier/paper_faithfulness/30_contract.md",
                  "verifier/paper_faithfulness/40_rubric.md",
                  "verifier/paper_faithfulness/50_authority.md",
                  "shared/90_artifact_delivery.md">>,
            requestSummary |->
                [
                    phase |-> phase,
                    targets |-> RequestVerifyTargets(kind),
                    blockedTargets |-> BlockedTargets
                ],
            previousOwnFindingsByLane |-> previousPaperFindingLanes,
            issueReportingPolicy |-> "current_failures_only",
            fixedItemReportingPolicy |-> "summary_only",
            targetIssueScope |-> RequestVerifyTargets(kind),
            rubric |->
                [
                    paperStatementAuthority |-> "configured_target_ids_label_first",
                    coveringSetAuthority |-> "covering_nodes_collectively_cover_target_statement",
                    definitionDependencyAuthority |-> "definition_statement_hashes_only",
                    faithfulnessStandard |-> "genuine_progress_not_repackaging"
                ],
            artifactContract |->
                [
                    resultType |-> "paper_faithfulness_result_v1",
                    overallRule |-> "approve_iff_pass",
                    promptSchemaExample |->
                        [
                            paperFaithfulness |->
                                [
                                    decision |-> "PASS or FAIL",
                                    issues |-> <<[node |-> "target_id", description |-> "..."]>>
                                ],
                            overall |-> "APPROVE or REJECT",
                            summary |-> "brief overall summary",
                            comments |-> "optional short note"
                        ],
                    phaseBlocks |->
                        [
                            paperFaithfulness |->
                                [
                                    decisionValues |-> <<"PASS", "FAIL">>,
                                    issueSubjectKind |-> "target"
                                ]
                        ]
                ],
            artifactPromptView |->
                [
                    rawOutputFormat |-> "json_only",
                    escapeJsonBackslashes |-> TRUE,
                    doneMarkerContract |-> "write_done_after_json_check_passes",
                    checkerAuthority |-> "exact_command_is_authoritative",
                    jsonCheckCommandTemplate |->
                        <<"python3", "{{check_script_path}}", "paper-faithfulness-result", "{{raw_output_path}}">>,
                    acceptanceCheckCommandTemplate |-> <<>>,
                    failureRecovery |-> "json_check_required_acceptance_check_best_effort",
                    stdoutPolicy |-> "do_not_print_json_to_stdout"
                ]
        ]
    ELSE
        NoPaperContract

RequestCorrContract(kind) ==
    IF kind \in {"corr"} THEN
        [
            promptFragments |->
                <<SchemePromptFragment(kind),
                  "verifier/common/00_intro.md">>
                \o
                CorrVerifierScenarioFragments(kind)
                \o
                <<"shared/10_repository_root.md",
                  "verifier/common/10_lane_id.md",
                  "verifier/common/15_previous_findings.md",
                  "shared/20_read_files.md",
                  "shared/25_filespec.md",
                  "shared/30_project_invariants.md",
                  "verifier/correspondence/20_frontier.md",
                  "verifier/correspondence/30_contract.md",
                  "verifier/correspondence/40_rubric.md",
                  "verifier/correspondence/50_authority.md",
                  "shared/90_artifact_delivery.md">>,
            requestSummary |->
                [
                    phase |-> phase,
                    targets |-> RequestVerifyTargets(kind),
                    nodes |-> RequestVerifyNodes(kind),
                    blockedTargets |-> BlockedTargets
                ],
            previousOwnFindingsByLane |-> previousCorrFindingLanes,
            issueReportingPolicy |-> "current_failures_only",
            fixedItemReportingPolicy |-> "summary_only",
            nodeIssueScope |-> RequestVerifyNodes(kind),
            rubric |->
                [
                    statementAlignmentChecks |->
                        <<"quantifier_scope", "type_constraints", "implicit_assumptions", "domain_context">>,
                    projectDefinitionPolicy |-> "expand_project_definitions_but_trust_mathlib",
                    definitionHygiene |->
                        <<"reject_opaque", "reject_axiom", "reject_constant", "reject_sorry_in_definition">>,
                    duplicateMathlibDefinitionPolicy |-> "reject_project_duplicates",
                    preambleItemIssuePolicy |-> "use_exact_item_id"
                ],
            artifactContract |->
                [
                    resultType |-> "correspondence_result_v1",
                    overallRule |-> "approve_iff_pass",
                    promptSchemaExample |->
                        [
                            correspondence |->
                                [
                                    decision |-> "PASS or FAIL",
                                    issues |-> <<[node |-> "node_id", description |-> "..."]>>
                                ],
                            overall |-> "APPROVE or REJECT",
                            summary |-> "brief overall summary",
                            comments |-> "optional short note"
                        ],
                    phaseBlocks |->
                        [
                            correspondence |->
                                [
                                    decisionValues |-> <<"PASS", "FAIL">>,
                                    issueSubjectKind |-> "node"
                                ]
                        ]
                ],
            artifactPromptView |->
                [
                    rawOutputFormat |-> "json_only",
                    escapeJsonBackslashes |-> TRUE,
                    doneMarkerContract |-> "write_done_after_json_check_passes",
                    checkerAuthority |-> "exact_command_is_authoritative",
                    jsonCheckCommandTemplate |->
                        <<"python3", "{{check_script_path}}", "correspondence-result", "{{raw_output_path}}">>,
                    acceptanceCheckCommandTemplate |-> <<>>,
                    failureRecovery |-> "json_check_required_acceptance_check_best_effort",
                    stdoutPolicy |-> "do_not_print_json_to_stdout"
                ],
            preambleContract |->
                [
                    mode |-> IF "Preamble" \in RequestVerifyNodes(kind) THEN "one_way_support" ELSE "none",
                    itemIds |-> IF "Preamble" \in RequestVerifyNodes(kind) THEN PreambleItemIds ELSE {},
                    emptyItemsVacuouslySupported |-> TRUE
                ]
        ]
    ELSE
        NoCorrContract

RequestSoundContract(kind) ==
    IF kind \in {"sound"} THEN
        [
            promptFragments |->
                <<SchemePromptFragment(kind),
                  "verifier/common/00_intro.md">>
                \o
                SoundVerifierScenarioFragments(kind)
                \o
                <<"shared/10_repository_root.md",
                  "verifier/common/10_lane_id.md",
                  "verifier/common/15_previous_findings.md",
                  "shared/20_read_files.md",
                  "shared/25_filespec.md",
                  "shared/30_project_invariants.md",
                  "verifier/soundness/20_target.md",
                  "verifier/soundness/30_contract.md",
                  "verifier/soundness/40_rubric.md",
                  "verifier/soundness/50_authority.md",
                  "shared/90_artifact_delivery.md">>,
            requestSummary |->
                [
                    phase |-> phase,
                    node |-> RequestSoundVerifyNode(kind),
                    activeNode |-> activeNode,
                    heldTarget |-> heldTarget
                ],
            previousOwnFindingsByLane |-> previousSoundFindingLanes,
            targetNodes |-> SoundVerifyNodes,
            evaluationBasis |-> "nl_only",
            detailFloor |-> "paper_floor",
            rubric |->
                [
                    proofStandard |-> "line_by_line_rigorous",
                    rejectSketches |-> TRUE,
                    detailFloor |-> "paper_floor",
                    leanCodeRelevance |-> "ignore_lean_check_nl_only"
                ],
            artifactContract |->
                [
                    resultType |-> "soundness_result_v1",
                    decisionValues |-> <<"SOUND", "UNSOUND", "STRUCTURAL">>,
                    overallRule |-> "approve_iff_sound",
                    promptSchemaExample |->
                        [
                            node |-> "target_node",
                            soundness |->
                                [
                                    decision |-> "SOUND, UNSOUND, or STRUCTURAL",
                                    explanation |-> "brief explanation"
                                ],
                            overall |-> "APPROVE or REJECT",
                            summary |-> "brief overall summary",
                            feedback |-> "optional short note"
                        ]
                ]
            ,
            artifactPromptView |->
                [
                    rawOutputFormat |-> "json_only",
                    escapeJsonBackslashes |-> TRUE,
                    doneMarkerContract |-> "write_done_after_json_check_passes",
                    checkerAuthority |-> "exact_command_is_authoritative",
                    jsonCheckCommandTemplate |->
                        <<"python3", "{{check_script_path}}", "soundness-result", "{{raw_output_path}}", "--node", "{{node_name}}">>,
                    acceptanceCheckCommandTemplate |-> <<>>,
                    failureRecovery |-> "json_check_required_acceptance_check_best_effort",
                    stdoutPolicy |-> "do_not_print_json_to_stdout"
                ]
        ]
    ELSE
        NoSoundContract

RequestWorkerContract(kind) ==
    IF kind = "worker" THEN
        [
            promptFragments |->
                <<SchemePromptFragment(kind)>>
                \o
                (IF CurrentWorkerProfile = "theorem" THEN
                    <<"worker/theorem_stating/00_intro.md">>
                 ELSE IF CurrentWorkerProfile \in {"proof_easy", "proof_hard"} THEN
                    <<"worker/proof_formalization/00_intro.md">>
                 ELSE IF CurrentWorkerProfile = "cleanup" THEN
                    <<"worker/cleanup/00_intro.md">>
                 ELSE IF CurrentWorkerProfile = "final_cleanup" THEN
                    <<"worker/final_cleanup/00_intro.md">>
                 ELSE
                    <<"worker/generic/00_intro.md">>)
                \o
                WorkerScenarioFragments(kind)
                \o
                <<"shared/10_repository_root.md",
                  "shared/20_read_files.md",
                  "worker/common/15_loogle.md",
                  "shared/25_filespec.md",
                  "shared/30_project_invariants.md",
                  "worker/common/20_authority.md",
                  "worker/common/30_request.md",
                  "worker/common/31_scratchpad.md">>
                \o
                (IF CurrentWorkerValidationKind \in WorkerNewNodesAllowedKinds THEN
                    <<"worker/common/38_new_node_difficulty.md">>
                 ELSE
                    << >>)
                \o
                <<"worker/common/34_verifier_evidence.md">>
                \o
                (IF CurrentWorkerProfile = "theorem" THEN
                    <<"worker/theorem_stating/10_mode_guidance.md",
                      "worker/theorem_stating/15_initial_dag_size.md",
                      "worker/theorem_stating/20_common_failure_modes.md">>
                 ELSE IF CurrentWorkerProfile \in {"proof_easy", "proof_hard"} THEN
                    <<"worker/proof_formalization/10_operational_guidance.md",
                      "worker/proof_formalization/15_failure_triage.md",
                      "worker/proof_formalization/20_helper_decomposition.md">>
                 ELSE
                    << >>)
                \o
                <<"worker/common/33_routing_hints.md",
                  IF CurrentWorkerProfile \in {"cleanup", "final_cleanup"} THEN
                      "worker/cleanup/35_reviewer_comments.md"
                  ELSE
                      "worker/common/35_reviewer_comments.md",
                  IF CurrentWorkerProfile \in {"cleanup", "final_cleanup"} THEN
                      "worker/cleanup/37_field_guidance.md"
                  ELSE
                      "worker/common/37_field_guidance.md",
                  "worker/common/40_contract.md",
                  IF CurrentWorkerProfile \in {"cleanup", "final_cleanup"} THEN
                      "worker/cleanup/45_outcomes.md"
                  ELSE
                      "worker/common/45_outcomes.md",
                  "worker/common/50_acceptance.md",
                  "shared/90_artifact_delivery.md",
                  "worker/common/95_gate_authority.md">>,
            requestSummary |->
                [
                    phase |-> phase,
                    mode |-> CurrentMode,
                    activeNode |-> activeNode,
                    heldTarget |-> heldTarget,
                    freshContext |-> RequestFreshContext(kind),
                    workerContext |-> RequestWorkerContext(kind),
                    blockers |-> RequestBlockers(kind),
                    currentPresentNodes |-> presentNodes,
                    currentProofNodes |-> currentProofNodes,
                    currentDeps |-> currentDeps,
                    currentTargetClaims |-> currentTargetClaims
                ],
            reviewerComments |-> reviewerComments,
            resultType |-> "worker_result_v1",
            kernelDerivesStructuralSnapshot |-> TRUE,
            allowedOutcomes |->
                IF CurrentWorkerProfile \in {"cleanup", "final_cleanup"} THEN
                    <<"valid", "invalid">>
                ELSE
                    <<"valid", "invalid", "stuck", "needs_restructure">>,
            reportedDeltaFields |->
                IF CurrentWorkerProfile \in {"cleanup", "final_cleanup"} THEN
                    <<"semantic_dep_updates">>
                ELSE
                    <<"semantic_dep_updates", "target_claim_updates", "difficulty_updates">>,
            forbiddenLegacyFields |-> <<"status", "CRISIS">>,
            promptSchemaExample |->
                [
                    outcome |->
                        IF CurrentWorkerProfile \in {"cleanup", "final_cleanup"} THEN
                            "valid / invalid"
                        ELSE
                            "valid / invalid / stuck / needs_restructure",
                    summary |-> "brief summary",
                    comments |-> "optional short note",
                    semanticDepUpdates |->
                        [nodeId |-> <<"semantic_dep_node", "...">>],
                    targetClaimUpdates |->
                        [nodeId |-> <<"target_id">>],
                    difficultyUpdates |-> [nodeId |-> "easy or hard"]
                ],
            scopeContract |->
                [
                    existingNodeScopeMode |->
                        IF CurrentWorkerValidationKind \in {"theorem_global", "cleanup", "final_cleanup"} THEN
                            "all_present"
                        ELSE IF CurrentWorkerValidationKind \in {"theorem_targeted", "proof_restructure", "proof_coarse_restructure"} THEN
                            "authorized_existing_nodes"
                        ELSE IF CurrentWorkerValidationKind = "proof_local" THEN
                            "active_node_only"
                        ELSE
                            "none",
                    authorizedExistingNodes |-> CurrentWorkerAuthorizedNodes,
                    configuredTargets |-> configuredTargets,
                    pendingTargets |-> BlockedTargets,
                    pendingTargetsMeaning |-> "targets_lacking_current_approved_support",
                    newNodesAllowed |->
                        CurrentWorkerValidationKind \in WorkerNewNodesAllowedKinds,
                    allowNewObligations |-> CurrentWorkerAllowNewObligations,
                    mustCloseActive |-> CurrentWorkerMustCloseActive
                ],
            stuckContract |->
                [
                    allowed |-> ~(CurrentWorkerProfile \in {"cleanup", "final_cleanup"}),
                    forbidTabletChangesWhenStuck |-> CurrentWorkerAcceptance.forbidTabletChangesWhenStuck,
                    meaning |->
                        IF CurrentWorkerProfile \in {"cleanup", "final_cleanup"} THEN
                            "none"
                        ELSE
                            "cannot_make_progress_on_pending_work_under_current_scope"
                ],
            needsRestructureContract |->
                [
                    allowed |-> ~(CurrentWorkerProfile \in {"cleanup", "final_cleanup"}),
                    forbidTabletChangesWhenNeedsRestructure |-> FALSE,
                    meaning |->
                        IF CurrentWorkerProfile \in {"cleanup", "final_cleanup"} THEN
                            "none"
                        ELSE
                            "worker_can_name_broader_restructure_needed_but_current_scope_does_not_authorize_it"
                ],
            artifactPromptView |->
                [
                    rawOutputFormat |-> "json_only",
                    escapeJsonBackslashes |-> TRUE,
                    doneMarkerContract |-> "write_done_after_json_check_passes",
                    checkerAuthority |-> "exact_command_is_authoritative",
                    jsonCheckCommandTemplate |->
                        <<"python3", "{{check_script_path}}", "trellis-worker-result", "{{raw_output_path}}">>,
                    acceptanceCheckCommandTemplate |->
                        <<"python3", "{{check_script_path}}", "trellis-worker-result", "{{raw_output_path}}", "--repo", "{{repo_path}}", "--context-json", "{{acceptance_context_path}}">>,
                    \* The worker may run this exact command inside the worker
                    \* repo as advisory evidence, but the supervisor reruns the
                    \* same command against the synced supervisor workspace and
                    \* only that rerun is authoritative. Any mismatch between
                    \* the advisory worker result and the authoritative
                    \* supervisor result is treated as a deterministic invalid
                    \* attempt and should force the next worker request to use
                    \* fresh context.
                    failureRecovery |-> "json_check_required_acceptance_check_best_effort",
                    stdoutPolicy |-> "do_not_print_json_to_stdout"
                ]
        ]
    ELSE
        NoWorkerContract

RequestReviewContract(kind) ==
    IF kind = "review" THEN
        [
            promptFragments |->
                <<SchemePromptFragment(kind),
                  "review/common/00_intro.md">>
                \o
                ReviewScenarioFragments(kind)
                \o
                <<"shared/10_repository_root.md",
                  "shared/20_read_files.md",
                  "shared/25_filespec.md",
                  "shared/30_project_invariants.md",
                  "review/common/10_request.md",
                  "review/common/12_deterministic_worker_rejection.md",
                  "review/common/20_blocker_choices.md",
                  "review/common/25_verifier_reasoning.md",
                  "review/common/30_contract.md",
                  "review/common/32_revert.md",
                  "review/common/33_routing_hints.md",
                  "review/common/34_worker_context_strategy.md",
                  "review/common/35_comments.md">>
                \o
                (IF phase = "proof_formalization" THEN
                    <<"review/common/37_restructure_strategy.md">>
                 ELSE
                    << >>)
                \o
                <<"review/common/38_paper_focus_strategy.md",
                  "review/common/39_revert_strategy.md",
                  "review/common/40_authority.md",
                  "shared/90_artifact_delivery.md">>,
            requestSummary |->
                [
                    phase |-> phase,
                    mode |-> CurrentMode,
                    activeNode |-> activeNode,
                    heldTarget |-> heldTarget,
                    invalidAttempt |-> invalidAttempt,
                    retryOutcomeKind |-> retryOutcomeKind,
                    deterministicWorkerRejectionReasons |->
                        IF retryOutcomeKind = "invalid" THEN <<"set">> ELSE << >>,
                    retryAttempt |-> IF retryOutcomeKind = "none" THEN 0 ELSE attempt,
                    humanInputOutstanding |-> humanInputOutstanding,
                    blockedTargets |-> BlockedTargets
                ],
            artifactContract |->
                [
                    resultType |-> "review_result_v1",
                    \* `authorized_node_ids` is required when the
                    \* reviewer can hand a proof Restructure /
                    \* CoarseRestructure worker task to the next
                    \* worker; otherwise it appears in optional. Mirrors
                    \* the Rust contract emitter at
                    \* request_contracts.rs.
                    requiredFields |->
                        IF /\ phase = "proof_formalization"
                           /\ "CONTINUE" \in RequestAllowedDecisions(kind)
                           /\ \/ "restructure" \in RequestAllowedNextModes(kind)
                              \/ "coarse_restructure" \in RequestAllowedNextModes(kind)
                        THEN
                            <<"decision", "reason", "comments", "task_blocker_ids", "override_blocker_ids",
                              "reset_blocker_ids", "next_active", "next_mode", "reset",
                              "difficulty_updates", "allow_new_obligations", "must_close_active",
                              "authorized_node_ids">>
                        ELSE
                            <<"decision", "reason", "comments", "task_blocker_ids", "override_blocker_ids",
                              "reset_blocker_ids", "next_active", "next_mode", "reset",
                              "difficulty_updates", "allow_new_obligations", "must_close_active">>,
                    optionalFields |->
                        IF /\ phase = "proof_formalization"
                           /\ "CONTINUE" \in RequestAllowedDecisions(kind)
                           /\ "restructure" \notin RequestAllowedNextModes(kind)
                           /\ "coarse_restructure" \notin RequestAllowedNextModes(kind)
                        THEN
                            <<"clear_human_input", "next_worker_context_mode", "paper_focus_ranges", "work_style_hint", "authorized_node_ids">>
                        ELSE
                            <<"clear_human_input", "next_worker_context_mode", "paper_focus_ranges", "work_style_hint">>,
                    promptSchemaExample |->
                        [
                            decision |-> RequestAllowedDecisions(kind),
                            reason |-> "brief rationale for the decision",
                            comments |-> "optional non-authoritative comments to the next worker",
                            taskBlockerIds |-> <<"subset of listed ids">>,
                            overrideBlockerIds |-> <<"subset of allowed override ids">>,
                            resetBlockerIds |-> <<"subset of allowed reset ids">>,
                            nextActive |-> "node id or empty string",
                            nextMode |-> RequestAllowedNextModes(kind),
                            reset |-> RequestAllowedResets(kind),
                            difficultyUpdates |-> [nodeId |-> "easy or hard"],
                            allowNewObligations |-> "proof-formalization only: true permits new helper obligations with sorry",
                            mustCloseActive |-> "proof-formalization only: true requires active node to be Lean-closed",
                            clearHumanInput |->
                                IF humanInputOutstanding THEN
                                    TRUE
                                ELSE
                                    "omit unless clearing human input",
                            nextWorkerContextMode |-> "resume or fresh",
                            paperFocusRanges |-> <<[startLine |-> 1, endLine |-> 2, reason |-> "set"]>>,
                            workStyleHint |-> "none or restructure",
                            authorizedNodeIds |-> "narrow list of existing nodes the worker may edit"
                        ]
                ],
            verifierEvidence |->
                [paper |-> latestPaperEvidenceLanes, corr |-> latestCorrEvidenceLanes, sound |-> latestSoundEvidenceLanes],
            \* Each list is optional; omitted blockers remain live. The kernel
            \* checks subset + pairwise disjoint, NOT completeness.
            blockerActions |->
                [
                    required |-> FALSE,
                    actionFields |-> <<"task_blocker_ids", "override_blocker_ids", "reset_blocker_ids">>,
                    choices |-> RequestReviewBlockerChoices(kind),
                    allowedOverrideIds |-> RequestAllowedOverrideBlockerIds(kind),
                    allowedResetIds |-> RequestAllowedResetBlockerIds(kind),
                    resetSemantics |-> "clear_current_fail_to_unknown"
                ],
            nextActiveContract |->
                [
                    allowedNodes |-> RequestAllowedNextActiveNodes(kind),
                    targetedAllowedNodes |-> RequestTargetedNextActiveNodes(kind),
                    allowTargetedWithoutNextActive |-> RequestAllowTargetedWithoutNextActive(kind)
                ],
            difficultyUpdateContract |->
                [
                    allowedNodes |-> RequestAllowedDifficultyUpdateNodes(kind)
                ],
            clearHumanInputContract |->
                [
                    allowedWhenOutstanding |-> humanInputOutstanding,
                    omitWhenNotAllowed |-> TRUE
                ],
            commentsContract |->
                [
                    field |-> "comments",
                    semantics |-> "non_authoritative_guidance_forwarded_to_future_workers",
                    emptyStringMeansNoComments |-> TRUE
                ],
            routingHintsContract |->
                [
                    nextWorkerContextModeValues |-> <<"resume", "fresh">>,
                    paperFocusRangesShape |->
                        [
                            startLine |-> ">= 1",
                            endLine |-> ">= start_line",
                            reason |-> "optional short reason"
                        ],
                    workStyleHintValues |-> <<"none", "restructure">>,
                    continueOnly |-> TRUE,
                    advisoryOnly |-> TRUE,
                    semantics |-> "non_authoritative_hints_forwarded_to_future_workers_without_expanding_kernel_authority"
                ],
            resetContract |->
                [
                    allowedResets |-> RequestAllowedResets(kind),
                    lastCommitSemantics |-> "discard_unaccepted_live_changes_and_resume_from_last_accepted_checkpoint"
                ],
            artifactPromptView |->
                [
                    rawOutputFormat |-> "json_only",
                    escapeJsonBackslashes |-> TRUE,
                    doneMarkerContract |-> "write_done_after_json_check_passes",
                    checkerAuthority |-> "exact_command_is_authoritative",
                    jsonCheckCommandTemplate |->
                        <<"python3", "{{check_script_path}}", "trellis-reviewer-result", "{{raw_output_path}}">>,
                    acceptanceCheckCommandTemplate |->
                        <<"python3", "{{check_script_path}}", "trellis-reviewer-result", "{{raw_output_path}}", "--context-json", "{{context_json_path}}">>,
                    failureRecovery |-> "json_check_required_acceptance_check_best_effort",
                    stdoutPolicy |-> "do_not_print_json_to_stdout"
                ]
        ]
    ELSE
        NoReviewContract

RequestWorkerBinding(kind) ==
    IF kind = "worker" THEN
        WorkerBindingByProfile[CurrentWorkerProfile]
    ELSE
        DefaultVerifierBinding

RequestReviewerBinding(kind) ==
    IF kind = "review" THEN
        ReviewerBinding
    ELSE
        DefaultVerifierBinding

RequestRecord(reqId, kind) ==
    [
        id |-> reqId,
        kind |-> kind,
        cycle |-> cycle,
        phase |-> phase,
        active |-> activeNode,
        held |-> heldTarget,
        mode |-> CurrentMode,
        blockers |-> RequestBlockers(kind),
        blockedTargets |-> BlockedTargets,
        configuredTargets |-> configuredTargets,
        verifyNodes |-> RequestVerifyNodes(kind),
        verifyTargets |-> RequestVerifyTargets(kind),
        verifyLanes |-> RequestVerifyLanes(kind),
        paperVerifyLaneBindings |-> RequestPaperVerifyLaneBindings(kind),
        corrVerifyLaneBindings |-> RequestCorrVerifyLaneBindings(kind),
        soundVerifyLaneBindings |-> RequestSoundVerifyLaneBindings(kind),
        paperVerifyTargets |-> RequestPaperVerifyTargets(kind),
        substantivenessVerifyNodes |-> RequestPaperVerifyNodes(kind),
        corrVerifyNodes |-> RequestCorrVerifyNodes(kind),
        corrVerifyTargets |-> RequestCorrVerifyTargets(kind),
        soundVerifyNodes |-> RequestSoundVerifyNodes(kind),
        soundVerifyNode |-> RequestSoundVerifyNode(kind),
        runtimeSupportRequired |-> RequestRuntimeSupportRequired(kind),
        allowedDecisions |-> RequestAllowedDecisions(kind),
        allowedNextModes |-> RequestAllowedNextModes(kind),
        allowedNextActiveNodes |-> RequestAllowedNextActiveNodes(kind),
        targetedNextActiveNodes |-> RequestTargetedNextActiveNodes(kind),
        allowTargetedWithoutNextActive |-> RequestAllowTargetedWithoutNextActive(kind),
        allowedResets |-> RequestAllowedResets(kind),
        allowedResetBlockers |-> RequestAllowedResetBlockers(kind),
        allowedOverrideBlockers |-> RequestAllowedOverrideBlockers(kind),
        allowedOverrideBlockerIds |-> RequestAllowedOverrideBlockerIds(kind),
        allowedResetBlockerIds |-> RequestAllowedResetBlockerIds(kind),
        reviewBlockerChoices |-> RequestReviewBlockerChoices(kind),
        allowedDifficultyUpdateNodes |-> RequestAllowedDifficultyUpdateNodes(kind),
        currentPresentNodes |-> presentNodes,
        currentProofNodes |-> currentProofNodes,
        currentNodeKinds |-> [n \in presentNodes |-> currentNodeKinds[n]],
        currentDeps |-> currentDeps,
        currentTargetClaims |-> currentTargetClaims,
        reviewerComments |-> reviewerComments,
        deterministicWorkerRejectionReasons |->
            IF retryOutcomeKind = "invalid" THEN <<"set">> ELSE << >>,
        reviewVerifierEvidence |-> [paper |-> latestPaperEvidenceLanes, corr |-> latestCorrEvidenceLanes, sound |-> latestSoundEvidenceLanes],
        retryOutcomeKind |-> retryOutcomeKind,
        retryAttempt |-> IF retryOutcomeKind = "none" THEN 0 ELSE attempt,
        projectInvariants |-> ProjectInvariants,
        freshContext |-> RequestFreshContext(kind),
                    promptContractVersion |-> IF kind = "none" THEN 0 ELSE 34,
        paperContract |-> RequestPaperContract(kind),
        corrContract |-> RequestCorrContract(kind),
        soundContract |-> RequestSoundContract(kind),
        workerContract |-> RequestWorkerContract(kind),
        reviewContract |-> RequestReviewContract(kind),
        workerBinding |-> RequestWorkerBinding(kind),
        reviewerBinding |-> RequestReviewerBinding(kind),
        workerContext |-> RequestWorkerContext(kind),
        workerAcceptance |-> IF kind = "worker" THEN CurrentWorkerAcceptance ELSE NoWorkerAcceptance,
        invalidAttempt |-> invalidAttempt,
        humanInputOutstanding |-> humanInputOutstanding,
        gateKind |-> gateKind,
        approvedTargetNodes |-> RequestApprovedTargetNodes(kind),
        approvedCorrFingerprints |-> RequestApprovedCorrFingerprints(kind),
        currentPaperApprovedFp |-> RequestCurrentPaperApprovedFp(kind),
        coarseDagNodes |-> RequestCoarseDagNodes(kind)
    ]

RequestKindForStage(s) ==
    IF s = "Worker" THEN
        "worker"
    ELSE IF s = "VerifyPaper" THEN
        "paper"
    ELSE IF s = "VerifyCorr" THEN
        "corr"
    ELSE IF s = "VerifySound" THEN
        "sound"
    ELSE IF s = "Reviewer" THEN
        "review"
    ELSE IF s = "HumanGate" THEN
        "human_gate"
    ELSE IF s = "CleanupAudit" THEN
        \* Cleanup-v2: audit sub-phase request kind. Mirrors kernel
        \* `request_stage` (engine.rs).
        "audit"
    ELSE IF s = "StuckMathAudit" THEN
        \* StuckMathAudit role. Mirrors kernel `request_stage`.
        "stuck_math_audit"
    ELSE
        "none"


TheoremReviewNextActiveLegal(node) ==
    node = NoNode
    \/
    TheoremReviewNextActiveNodeAllowed(node)

TypeOK ==
    /\ phase \in PhaseValues
    /\ stage \in StageValues
    /\ cycle \in 0..MaxCycle
    /\ attempt \in 0..MaxAttempt
    /\ requestSeq \in Nat
    /\ invalidAttempt \in BOOLEAN
    /\ retryOutcomeKind \in RetryOutcomeKinds
    /\ gateKind \in GateKinds
    /\ gateFromInvalidAttempt \in BOOLEAN
    /\ activeNode \in Nodes \cup {NoNode}
    /\ heldTarget \in Nodes \cup {NoNode}
    /\ targetEditMode \in TargetEditModes
    /\ proofEditMode \in ProofEditModes
    /\ configuredTargets \subseteq Targets
    /\ approvedConfiguredTargets \subseteq Targets
    /\ currentNodeKinds \in [Nodes -> NodeKindValues]
    /\ committedNodeKinds \in [Nodes -> NodeKindValues]
    /\ currentProofNodes \subseteq presentNodes
    /\ committedProofNodes \subseteq committedPresentNodes
    /\ currentProofNodes = ProofNodesFromKinds(currentNodeKinds, presentNodes)
    /\ committedProofNodes = ProofNodesFromKinds(committedNodeKinds, committedPresentNodes)
    /\ currentDeps \in [Nodes -> SUBSET Nodes]
    /\ committedDeps \in [Nodes -> SUBSET Nodes]
    /\ currentTargetClaims \in [Nodes -> SUBSET Targets]
    /\ committedTargetClaims \in [Nodes -> SUBSET Targets]
    /\ presentNodes \subseteq Nodes
    /\ committedPresentNodes \subseteq Nodes
    /\ SupportFilesAvailable
    /\ PresentClosed(presentNodes)
    /\ \A n \in committedPresentNodes: committedDeps[n] \subseteq committedPresentNodes
    /\ openNodes \subseteq presentNodes
    /\ committedOpenNodes \subseteq committedPresentNodes
    \* Patch C (LOCAL_CLOSURE_IMPL_PLAN.md §7.2): closure-unverified set
    \* is sorry-free-only — a node has `sorry` (sits in `openNodes`)
    \* xor it lacks a fresh local-closure record (sits in
    \* `localClosureUnverified`). Mutually exclusive. Subset of
    \* present proof_nodes per §7.2 invariant.
    /\ localClosureUnverified \subseteq (presentNodes \cap currentProofNodes)
    /\ committedLocalClosureUnverified \subseteq (committedPresentNodes \cap committedProofNodes)
    /\ localClosureUnverified \cap openNodes = {}
    /\ committedLocalClosureUnverified \cap committedOpenNodes = {}
    /\ ActiveNodeLegal(phase, activeNode, presentNodes, openNodes)
    /\ HeldTargetLegal(heldTarget, presentNodes, openNodes)
    /\ (phase # "theorem_stating") => heldTarget = NoNode
    /\ (phase # "theorem_stating") => targetEditMode = "global"
    /\ (phase # "proof_formalization") => proofEditMode = "local"
    /\ currentCoverage \in [Targets -> SUBSET Nodes]
    /\ committedCoverage \in [Targets -> SUBSET Nodes]
    /\ approvedCoverage \in [Targets -> SUBSET Nodes]
    /\ \A t \in Targets:
        /\ currentCoverage[t] \subseteq presentNodes
        /\ committedCoverage[t] \subseteq committedPresentNodes
        /\ approvedCoverage[t] \subseteq Nodes
    /\ currentCoverage = CoverageFromClaims(currentTargetClaims, presentNodes, configuredTargets)
    /\ committedCoverage = CoverageFromClaims(committedTargetClaims, committedPresentNodes, configuredTargets)
    /\ paperStatus \in [Targets -> CorrStates]
    /\ paperCurrentFp \in [Targets -> Fingerprints]
    /\ committedPaperCurrentFp \in [Targets -> Fingerprints]
    /\ paperApprovedFp \in [Targets -> Fingerprints]
    /\ substantivenessStatus \in [Nodes -> CorrStates]
    /\ substantivenessCurrentFp \in [Nodes -> Fingerprints]
    /\ committedSubstantivenessCurrentFp \in [Nodes -> Fingerprints]
    /\ substantivenessApprovedFp \in [Nodes -> Fingerprints]
    /\ currentTargetFp \in [Nodes -> Fingerprints]
    /\ committedTargetFp \in [Nodes -> Fingerprints]
    /\ approvedTargetFp \in [Nodes -> Fingerprints]
    /\ coarseDagNodes \subseteq Nodes
    /\ corrStatus \in [Nodes -> CorrStates]
    /\ corrCurrentFp \in [Nodes -> Fingerprints]
    /\ committedCorrCurrentFp \in [Nodes -> Fingerprints]
    /\ corrApprovedFp \in [Nodes -> Fingerprints]
    /\ soundStatus \in [Nodes -> SoundStates]
    /\ soundCurrentFp \in [Nodes -> Fingerprints]
    /\ committedSoundCurrentFp \in [Nodes -> Fingerprints]
    /\ soundApprovedFp \in [Nodes -> Fingerprints]
    \* Sound assessment taxonomy (kernel `sound_assessments` /
    \* `reviewer_requested_sound_verifier_nodes` /
    \* `sound_reverification_context`, model.rs).
    /\ soundAssessmentStatus \in [Nodes -> SoundAssessmentStatuses]
    /\ reviewerRequestedSoundVerifierNodes \subseteq Nodes
    /\ soundReverificationContext \in SoundReverificationContextValues
    \* Reviewer-requested verifier dispatch carriers must remain in
    \* presentNodes (the kernel `retain` in `apply_structural_filters`,
    \* model.rs, drops any non-present carrier).
    /\ reviewerRequestedSoundVerifierNodes \subseteq presentNodes
    \* When a reverification context is queued, its `target` must be a
    \* present node and its current status one of the two drift values
    \* the kernel actually surfaces this for.
    /\ soundReverificationContext # NoSoundReverificationContext
        => soundReverificationContext.target \in presentNodes
    \* Deviation lane TypeOK. All maps total over the abstract
    \* `Deviations` constant; presence is recorded in
    \* `deviationFiles[id]: BOOLEAN`. Note we use Fingerprints here
    \* (NOT Fingerprints \cup {NoFingerprint}) — NoFingerprint is
    \* already an element of Fingerprints by ASSUME at line 74.
    /\ deviationFiles \in [Deviations -> BOOLEAN]
    /\ committedDeviationFiles \in [Deviations -> BOOLEAN]
    /\ deviationStatus \in [Deviations -> CorrStates]
    /\ deviationCurrentFp \in [Deviations -> Fingerprints]
    /\ committedDeviationCurrentFp \in [Deviations -> Fingerprints]
    /\ deviationApprovedFp \in [Deviations -> Fingerprints]
    /\ nodeDeviationClaims \in [Nodes -> SUBSET Deviations]
    /\ committedNodeDeviationClaims \in [Nodes -> SUBSET Deviations]
    /\ lastCleanDeviationFiles \in [Deviations -> BOOLEAN]
    /\ lastCleanDeviationStatus \in [Deviations -> CorrStates]
    /\ lastCleanDeviationApprovedFp \in [Deviations -> Fingerprints]
    /\ lastCleanNodeDeviationClaims \in [Nodes -> SUBSET Deviations]
    /\ latestDeviationReviewIds \subseteq Deviations
    /\ latestDeviationEvidenceLanes \subseteq VerifierLanes
    \* Carrier hygiene (mirror of kernel
    \* `normalize_live_structural_state` model.rs:5316-5320 and
    \* `normalize_committed_structural_state`): claim sets may only
    \* name ids that are currently in the files map (live/committed
    \* mirror respectively), and only present-node carriers may
    \* hold claims.
    /\ \A n \in Nodes :
        /\ nodeDeviationClaims[n] \subseteq {id \in Deviations : deviationFiles[id]}
        /\ (n \notin presentNodes) => nodeDeviationClaims[n] = {}
        /\ committedNodeDeviationClaims[n] \subseteq {id \in Deviations : committedDeviationFiles[id]}
        /\ (n \notin committedPresentNodes) => committedNodeDeviationClaims[n] = {}
    /\ nodeDifficulty \in [Nodes -> DifficultyValues]
    /\ easyAttempts \in [Nodes -> Nat]
    /\ reviewerComments \in ReviewerCommentValues
    /\ latestPaperEvidenceLanes \subseteq VerifierLanes
    /\ latestCorrEvidenceLanes \subseteq VerifierLanes
    /\ latestSoundEvidenceLanes \subseteq VerifierLanes
    /\ latestPaperReviewTargets \subseteq Targets
    /\ latestCorrReviewNodes \subseteq Nodes
    /\ latestSoundReviewNodes \subseteq Nodes
    /\ latestPaperPanelSplit \in BOOLEAN
    /\ latestCorrPanelSplit \in BOOLEAN
    /\ latestSoundPanelSplit \in BOOLEAN
    /\ previousPaperFindingLanes \subseteq VerifierLanes
    /\ previousCorrFindingLanes \subseteq VerifierLanes
    /\ previousSoundFindingLanes \subseteq VerifierLanes
    /\ latestSubstantivenessEvidenceLanes \subseteq VerifierLanes
    /\ latestSubstantivenessReviewNodes \subseteq Nodes
    /\ latestSubstantivenessPanelSplit \in BOOLEAN
    /\ previousSubstantivenessFindingLanes \subseteq VerifierLanes
    /\ humanInputOutstanding \in BOOLEAN
    /\ nativeHistoryKinds \subseteq {<<kind, ph>> : kind \in (RequestKinds \ {NoRequest.kind}), ph \in PhaseValues}
    /\ cyclesSinceClean \in {0, 1}
    /\ hasEverBeenClean \in BOOLEAN
    /\ forceReviewAfterConeClean \in BOOLEAN
    \* Proposal v32: active coarse anchor is either NoNode or a member
    \* of coarseDagNodes, and must be NoNode outside proof_formalization
    \* or whenever coarseDagNodes = {} (mechanism disabled).
    /\ activeCoarseNode \in (coarseDagNodes \cup {NoNode})
    /\ (phase # "proof_formalization") => activeCoarseNode = NoNode
    /\ (coarseDagNodes = {}) => activeCoarseNode = NoNode
    /\ cyclesInCoarseRepairMode \in Nat
    /\ (activeCoarseNode = NoNode) => cyclesInCoarseRepairMode = 0
    \* StuckMathAudit producer state (kernel `StuckMathAuditState` in
    \* model.rs + `stuck_math_audit_burst_retry_count` +
    \* `last_stuck_math_audit_dispatched_cycle`).
    \* `STUCK_MATH_AUDIT_BURST_RETRY_LIMIT = 1` in model.rs; the
    \* retry counter takes values 0..1. The dispatched-cycle is
    \* `Option<u32>`; modeled as `NoCycle \cup 0..MaxCycle`.
    /\ stuckMathAuditActive \in BOOLEAN
    /\ stuckMathAuditNeedInputAudit \in NeedInputAuditContextValues
    /\ stuckMathAuditBurstRetryCount \in 0..1
    /\ lastStuckMathAuditDispatchedCycle \in ({NoCycle} \cup 0..MaxCycle)
    \* global_repair_mode TypeOK (kernel model.rs Step 1+3). Sentinels
    \* NoGlobalRepairRequest / NoGlobalRepairGrant model `Option::None`.
    \* The per-record extension-node sets are subsets of Nodes; the
    \* request also carries a reviewer-attached cycle (NoCycle if not
    \* yet set in a partial state).
    /\ pendingGlobalRepairRequest \in {NoGlobalRepairRequest}
        \cup [proposedExtensionNodes: SUBSET Nodes,
             dispatchedAtCycle: 0..MaxCycle]
    /\ pendingGlobalRepairGrant \in {NoGlobalRepairGrant}
        \cup [approvedExtensionNodes: SUBSET Nodes,
             dispatchedAtCycle: 0..MaxCycle]
    /\ latestGlobalRepairAuditDeclineReason \in {"", "declined"}
    /\ latestGlobalRepairAuditDeclineCycle \in ({NoCycle} \cup 0..MaxCycle)
    /\ lastReviewerGlobalRepairRequestCycle \in ({NoCycle} \cup 0..MaxCycle)
    /\ everShallowCoarseClosed \subseteq Nodes
    /\ globalRepairModeEnabled \in BOOLEAN
    \* Post-advance routing latch (kernel `post_advance_routing_pending`,
    \* model.rs). Forces the first burst after a human-approved phase
    \* advance to be a routing Reviewer.
    /\ postAdvanceRoutingPending \in BOOLEAN
    /\ postAdvanceRoutingPending => phase = "proof_formalization"
    \* Protected-target reapproval (kernel
    \* `pending_protected_reapproval_nodes` and
    \* `pending_protected_semantic_scope_confirmation`, model.rs).
    /\ pendingProtectedReapprovalNodes \subseteq Nodes
    /\ pendingProtectedSemanticScopeConfirmation \in ProtectedSemanticChangeConfirmationValues
    \* StuckMathAudit audit_plan lane (kernel `audit_plan` /
    \* `superseded_audit_plan`, model.rs). Task ids may only be drawn
    \* from the bounded `AuditTaskIds` set; `coneClean` is either NoNode
    \* or a present node when set on a live plan.
    /\ auditPlan \in AuditPlanValues
    /\ supersededAuditPlan \in AuditPlanValues
    /\ (auditPlan # NoAuditPlan /\ auditPlan.coneClean # NoNode)
        => auditPlan.coneClean \in presentNodes
    \* S7: pending request / grant extension nodes must remain in
    \* presentNodes after any structural mutation; the helper
    \* relegalize_global_repair_against_present drops carriers losing
    \* every node, so we mirror that invariant here.
    /\ (pendingGlobalRepairRequest # NoGlobalRepairRequest)
        => pendingGlobalRepairRequest.proposedExtensionNodes \subseteq presentNodes
    /\ (pendingGlobalRepairGrant # NoGlobalRepairGrant)
        => pendingGlobalRepairGrant.approvedExtensionNodes \subseteq presentNodes
    \* Latch / dispatch link: when the latch is on AND the spec is
    \* dispatched (stage = "StuckMathAudit"), the in-flight request is
    \* `stuck_math_audit`. Conversely, a NeedInput-audit context implies
    \* the latch is on (kernel `refresh_stuck_math_audit_latch` invariant
    \* enforced by `stuck_math_audit.need_input_audit.is_some() =>
    \* stage == Stage::StuckMathAudit` at model.rs:10601).
    /\ stuckMathAuditNeedInputAudit # NoNeedInputAuditContext
        => stuckMathAuditActive
    \* Audit-lane mutex: the NeedInputAuditor lane and the
    \* GlobalRepairAuditor lane both reuse Stage::StuckMathAudit but
    \* carry distinct contracts (audit role fragment in
    \* `request_contracts.rs`). The kernel enforces this with proactive
    \* clears in `route_need_input_to_auditor` /
    \* `route_global_repair_request_to_auditor` and an auto-decline in
    \* the retry-exhaust branch of
    \* `retry_or_transition_stuck_math_audit_to_reviewer`; the spec
    \* mirror sites are `ReviewNeedInputProof` and
    \* `AcceptStuckMathAuditRetryExhaustedBackToReviewer`.
    /\ ~(pendingGlobalRepairRequest # NoGlobalRepairRequest
         /\ stuckMathAuditNeedInputAudit # NoNeedInputAuditContext)
    /\ pendingTask.taskBlockers \subseteq BlockerUniverse
    /\ pendingTask.node \in Nodes \cup {NoNode}
    /\ pendingTask.mode \in TaskModes
    /\ pendingTask.orphanCleanupNodes \subseteq Nodes
    /\ pendingTask.nextWorkerContextMode \in WorkerContextModes
    /\ pendingTask.paperFocusRanges \in PaperFocusRangeSeqValues
    /\ pendingTask.workStyleHint \in WorkerWorkStyleHints
    /\ pendingTask.allowNewObligations \in BOOLEAN
    /\ pendingTask.mustCloseActive \in BOOLEAN
    \* Step C presentational flag (kernel model.rs
    \* `PendingTask::consumed_global_repair_grant`).
    /\ pendingTask.consumedGlobalRepairGrant \in BOOLEAN
    /\ inFlightRequest.id \in Nat
    /\ inFlightRequest.kind \in RequestKinds
    /\ inFlightRequest.cycle \in Nat
    /\ inFlightRequest.phase \in PhaseValues
    /\ inFlightRequest.active \in Nodes \cup {NoNode}
    /\ inFlightRequest.held \in Nodes \cup {NoNode}
    /\ inFlightRequest.mode \in TaskModes
    /\ inFlightRequest.blockers \subseteq BlockerUniverse
    /\ inFlightRequest.blockedTargets \subseteq Targets
    /\ inFlightRequest.configuredTargets \subseteq Targets
    /\ inFlightRequest.verifyNodes \subseteq Nodes
    /\ inFlightRequest.verifyTargets \subseteq Targets
    /\ inFlightRequest.verifyLanes \subseteq VerifierLanes
    /\ inFlightRequest.paperVerifyLaneBindings \subseteq LaneBindingUniverse
    /\ inFlightRequest.corrVerifyLaneBindings \subseteq LaneBindingUniverse
    /\ inFlightRequest.soundVerifyLaneBindings \subseteq LaneBindingUniverse
    /\ inFlightRequest.workerBinding \in VerifierBindings
    /\ inFlightRequest.reviewerBinding \in VerifierBindings
    /\ inFlightRequest.paperVerifyLaneBindings =
        IF inFlightRequest.kind \in {"paper"} THEN
            LaneBindings(SoundVerifierBindingByLane, inFlightRequest.verifyLanes)
        ELSE
            {}
    /\ inFlightRequest.corrVerifyLaneBindings =
        IF inFlightRequest.kind \in {"corr"} THEN
            LaneBindings(CorrVerifierBindingByLane, inFlightRequest.verifyLanes)
        ELSE
            {}
    /\ inFlightRequest.soundVerifyLaneBindings =
        IF inFlightRequest.kind \in {"sound"} THEN
            LaneBindings(SoundVerifierBindingByLane, inFlightRequest.verifyLanes)
        ELSE
            {}
    /\ inFlightRequest.paperVerifyTargets \subseteq Targets
    /\ inFlightRequest.substantivenessVerifyNodes \subseteq Nodes
    \* Per-cycle scheduling guarantee (mirror of kernel
    \* `request_paper_verify_targets` / `request_substantiveness_verify_nodes`):
    \* exactly one of the two paper frontiers is exposed per Paper
    \* request. The kernel branches on which is non-empty to drive the
    \* right reconciler / status mirror; both being non-empty would
    \* break that branch. Outside `kind = "paper"` both are empty.
    /\ ~(inFlightRequest.paperVerifyTargets # {} /\ inFlightRequest.substantivenessVerifyNodes # {})
    /\ inFlightRequest.corrVerifyNodes \subseteq Nodes
    /\ inFlightRequest.corrVerifyTargets \subseteq Targets
    /\ inFlightRequest.soundVerifyNodes \subseteq Nodes
    /\ inFlightRequest.soundVerifyNode \in Nodes \cup {NoNode}
    \* A Sound request verifies exactly one node per dispatch; the kernel
    \* normalizer (verification_normalization.rs::normalize_sound_response)
    \* rejects any other cardinality. Multi-Unknown states sequence through
    \* separate Sound requests.
    /\ Cardinality(inFlightRequest.soundVerifyNodes) <= 1
    /\ IF Cardinality(inFlightRequest.soundVerifyNodes) = 1 THEN
            inFlightRequest.soundVerifyNode \in inFlightRequest.soundVerifyNodes
       ELSE
            inFlightRequest.soundVerifyNode = NoNode
    /\ inFlightRequest.runtimeSupportRequired \in BOOLEAN
    /\ inFlightRequest.runtimeSupportRequired =
        RequestRuntimeSupportRequired(inFlightRequest.kind)
    /\ inFlightRequest.allowedDecisions \subseteq ReviewDecisions
    /\ inFlightRequest.allowedNextModes \subseteq TaskModes
    /\ inFlightRequest.allowedNextActiveNodes \subseteq Nodes
    /\ inFlightRequest.targetedNextActiveNodes \subseteq Nodes
    /\ inFlightRequest.allowTargetedWithoutNextActive \in BOOLEAN
    \* "theoremStatingNode" is the audit-authorized cone-clean reset
    \* (kernel ResetChoice::TheoremStatingNode, model.rs:691). The
    \* kernel's `request_allowed_resets` (model.rs:7262) never returns
    \* it for a Review request — it is reserved for the
    \* StuckMathAudit-authorized path. Inclusion here keeps the spec's
    \* type universe aligned with the kernel enum even though no spec
    \* action currently puts it into `allowedResets`.
    /\ inFlightRequest.allowedResets \subseteq {NoCheckpoint, "lastCommit", "lastClean", "theoremStatingNode"}
    /\ inFlightRequest.allowedResetBlockers \subseteq BlockerUniverse
    /\ inFlightRequest.allowedOverrideBlockers \subseteq BlockerUniverse
    /\ inFlightRequest.allowedOverrideBlockerIds \subseteq BlockerChoiceIdUniverse
    /\ inFlightRequest.allowedOverrideBlockerIds =
        {BlockerChoiceId(b) : b \in inFlightRequest.allowedOverrideBlockers}
    /\ inFlightRequest.allowedResetBlockerIds \subseteq BlockerChoiceIdUniverse
    /\ inFlightRequest.allowedResetBlockerIds =
        {BlockerChoiceId(b) : b \in inFlightRequest.allowedResetBlockers}
    /\ inFlightRequest.reviewBlockerChoices \subseteq BlockerChoiceUniverse
    /\ inFlightRequest.reviewBlockerChoices =
        {BlockerChoice(b) : b \in inFlightRequest.blockers}
    /\ inFlightRequest.allowedDifficultyUpdateNodes \subseteq Nodes
    /\ inFlightRequest.currentPresentNodes \subseteq Nodes
    /\ inFlightRequest.currentProofNodes \subseteq Nodes
    /\ inFlightRequest.currentNodeKinds \in [inFlightRequest.currentPresentNodes -> NodeKindValues]
    /\ inFlightRequest.currentProofNodes =
        {n \in inFlightRequest.currentPresentNodes : inFlightRequest.currentNodeKinds[n] = "proof"}
    /\ inFlightRequest.currentDeps \in [Nodes -> SUBSET Nodes]
    /\ inFlightRequest.currentTargetClaims \in [Nodes -> SUBSET Targets]
    /\ inFlightRequest.reviewerComments \in ReviewerCommentValues
    /\ inFlightRequest.deterministicWorkerRejectionReasons \in DeterministicWorkerRejectionReasonSeqValues
    /\ IF inFlightRequest.kind = "none" THEN
            TRUE
       ELSE
            inFlightRequest.reviewVerifierEvidence =
                [paper |-> latestPaperEvidenceLanes, corr |-> latestCorrEvidenceLanes, sound |-> latestSoundEvidenceLanes]
    /\ inFlightRequest.freshContext \in BOOLEAN
    \* Mirror of kernel `prompt_contract_version()` in
    \* `kernel/src/request_contracts.rs`. Bumped through several rounds
    \* of contract evolution (active-coarse-anchor, StuckMathAudit,
    \* NeedInputAuditor, deviation protocol prompts, etc.); current
    \* value 34.
    /\ inFlightRequest.promptContractVersion \in 0..34
    /\ inFlightRequest.promptContractVersion =
        IF inFlightRequest.kind = "none" THEN 0 ELSE 34
    /\ inFlightRequest.paperContract = RequestPaperContract(inFlightRequest.kind)
    /\ inFlightRequest.corrContract = RequestCorrContract(inFlightRequest.kind)
    /\ inFlightRequest.soundContract = RequestSoundContract(inFlightRequest.kind)
    /\ inFlightRequest.workerContract = RequestWorkerContract(inFlightRequest.kind)
    /\ inFlightRequest.reviewContract = RequestReviewContract(inFlightRequest.kind)
    /\ inFlightRequest.workerContext.enabled \in BOOLEAN
    /\ inFlightRequest.workerContext.activeDifficulty \in DifficultyValues
    /\ inFlightRequest.workerContext.activeEasyAttempts \in Nat
    /\ inFlightRequest.workerContext.workerProfile \in WorkerProfiles
    /\ inFlightRequest.workerContext.validationKind \in WorkerValidationKinds
    /\ inFlightRequest.workerContext.authorizedNodes \subseteq Nodes
    /\ inFlightRequest.workerContext.allowNewObligations \in BOOLEAN
    /\ inFlightRequest.workerContext.mustCloseActive \in BOOLEAN
    /\ inFlightRequest.workerContext.nextContextMode \in WorkerContextModes
    /\ inFlightRequest.workerContext.paperFocusRanges \in PaperFocusRangeSeqValues
    /\ inFlightRequest.workerContext.workStyleHint \in WorkerWorkStyleHints
    /\ inFlightRequest.workerContext.consumedGlobalRepairGrant \in BOOLEAN
    /\ inFlightRequest.workerContext.enabled <=> (inFlightRequest.kind = "worker")
    /\ inFlightRequest.workerAcceptance.enabled \in BOOLEAN
    /\ inFlightRequest.workerAcceptance.validationKind \in WorkerValidationKinds
    /\ inFlightRequest.workerAcceptance.authorizedNodes \subseteq Nodes
    /\ inFlightRequest.workerAcceptance.validationExecutionPlan \in Seq(WorkerValidationExecutionPlanStepValues)
    /\ inFlightRequest.workerAcceptance.requireExplicitTargetClaimsForNewNodes \in BOOLEAN
    /\ inFlightRequest.workerAcceptance.forbidTabletChangesWhenStuck \in BOOLEAN
    /\ inFlightRequest.workerAcceptance.observationPlan.captureBeforeSnapshot \in BOOLEAN
    /\ inFlightRequest.workerAcceptance.observationPlan.captureBeforeTabletContents \in BOOLEAN
    /\ inFlightRequest.workerAcceptance.observationPlan.captureScopedTabletBaselineErrors \in BOOLEAN
    /\ inFlightRequest.workerAcceptance.observationPlan.scopedTabletBaselineScope \in WorkerBaselineScopes
    /\ inFlightRequest.workerAcceptance.observationPlan.captureImportsBefore \in BOOLEAN
    /\ inFlightRequest.workerAcceptance.observationPlan.captureExpectedActiveHash \in BOOLEAN
    /\ inFlightRequest.workerAcceptance.observationPlan.captureBaselineDeclarationHashes \in BOOLEAN
    /\ inFlightRequest.workerAcceptance.observationPlan.captureBaselineCorrespondenceHashes \in BOOLEAN
    /\ inFlightRequest.workerAcceptance.enabled <=> (inFlightRequest.kind = "worker")
    /\ inFlightRequest.invalidAttempt \in BOOLEAN
    /\ inFlightRequest.retryOutcomeKind \in RetryOutcomeKinds
    /\ inFlightRequest.retryAttempt \in Nat
    /\ inFlightRequest.humanInputOutstanding \in BOOLEAN
    /\ inFlightRequest.gateKind \in GateKinds
    /\ inFlightRequest.approvedTargetNodes \subseteq Nodes
    /\ inFlightRequest.approvedCorrFingerprints \in [inFlightRequest.approvedTargetNodes -> Fingerprints]
    /\ inFlightRequest.currentPaperApprovedFp \in [Targets -> Fingerprints]
    /\ inFlightRequest.coarseDagNodes \subseteq Nodes
    /\ inFlightRequest.id <= requestSeq
    /\ (inFlightRequest.kind = "none") <=> (inFlightRequest = NoRequest)
    /\ response.status \in ResponseStatuses
    /\ response.kind \in RequestKinds
    /\ response.cycle \in Nat
    /\ response.workerOutcome \in WorkerOutcomes
    /\ \A i \in 1..Len(response.validationStepResults):
        /\ response.validationStepResults[i].kind \in WorkerValidationStepKinds
        /\ response.validationStepResults[i].ok \in BOOLEAN
        /\ response.validationStepResults[i].detail \in STRING
        /\ response.validationStepResults[i].errors \in Seq(STRING)
        /\ response.validationStepResults[i].buildOutput \in STRING
        /\ response.validationStepResults[i].allowedNodes \subseteq Nodes
    /\ response.present \subseteq Nodes
    /\ response.open \subseteq response.present
    /\ response.coverage \in [Targets -> SUBSET Nodes]
    /\ \A t \in Targets: response.coverage[t] \subseteq response.present
    /\ response.corrCurrent \in [Nodes -> Fingerprints]
    /\ response.paperCurrent \in [Targets -> Fingerprints]
    /\ response.soundCurrent \in [Nodes -> Fingerprints]
    /\ response.targetFp \in [Nodes -> Fingerprints]
    /\ response.corrMap \in [Nodes -> CorrUpdates]
    /\ response.paperMap \in [Targets -> CorrUpdates]
    /\ response.substantivenessMap \in [Nodes -> CorrUpdates]
    /\ response.soundMap \in [Nodes -> SoundUpdates]
    /\ response.corrLaneMaps \in [VerifierLanes -> [Nodes -> CorrUpdates]]
    /\ response.paperLaneMaps \in [VerifierLanes -> [Targets -> CorrUpdates]]
    /\ response.substantivenessLaneMaps \in [VerifierLanes -> [Nodes -> CorrUpdates]]
    /\ response.soundLaneMaps \in [VerifierLanes -> [Nodes -> SoundUpdates]]
    /\ response.paperPanelSplit \in BOOLEAN
    /\ response.substantivenessPanelSplit \in BOOLEAN
    /\ response.corrPanelSplit \in BOOLEAN
    /\ response.soundPanelSplit \in BOOLEAN
    /\ response.proofNodeMap \in [Nodes -> {"proof", "not_proof", "same"}]
    /\ response.nodeKindMap \in [Nodes -> NodeKindUpdates]
    /\ response.depMap \in [Nodes -> NodeSetUpdateValues]
    /\ response.targetClaimMap \in [Nodes -> TargetClaimUpdateValues]
    /\ response.difficultyMap \in [Nodes -> DifficultyUpdates]
    /\ response.decision \in ReviewDecisions
    /\ IF response.kind \in {"review", "none"} THEN
            response.comments \in ReviewerCommentValues
       ELSE
            TRUE
    /\ response.taskBlockers \subseteq BlockerUniverse
    /\ response.overrideBlockers \subseteq BlockerUniverse
    /\ response.resetBlockers \subseteq BlockerUniverse
    /\ response.nextActive \in Nodes \cup {NoNode}
    /\ response.nextActiveCoarse \in Nodes \cup {NoNode}
    /\ response.reset \in {NoCheckpoint, "lastCommit", "lastClean", "theoremStatingNode"}
    /\ response.nextMode \in TaskModes
    /\ response.humanChoice \in HumanSignals
    /\ response.clearHumanInput \in BOOLEAN
    /\ response.nextWorkerContextMode \in WorkerContextModes
    /\ response.paperFocusRanges \in PaperFocusRangeSeqValues
    /\ response.workStyleHint \in WorkerWorkStyleHints
    /\ response.allowNewObligations \in BOOLEAN
    /\ response.mustCloseActive \in BOOLEAN
    \* Reviewer-named Sound verifier targets (kernel
    \* `ReviewResponse::request_sound_verifier_nodes`).
    /\ response.requestSoundVerifierNodes \subseteq Nodes
    \* Worker-response protected-target signal (kernel
    \* `WorkerResponse::protected_semantic_change_nodes`). Worker lifts
    \* changes from its sandbox into the reviewer-tracked set on Valid
    \* acceptance; ProtectedReapproval producer side.
    /\ response.protectedSemanticChangeNodes \subseteq Nodes
    \* StuckMathAudit response carriers (kernel `StuckMathAuditResponse`
    \* in model.rs: `confirm_need_input`, `cone_clean_node`, `audit_plan`).
    \* `coneClean` is either NoNode or a coarse node; the action
    \* `AcceptAuthorizedConeCleanReset` further requires membership in
    \* `coarseDagNodes` at acceptance time. `auditPlan` is union-typed
    \* over the NoAuditPlan sentinel and the records-set form (see
    \* `AuditPlanValues`); the live-plan invariant is enforced at the
    \* state level (`auditPlan \in AuditPlanValues` above), not on the
    \* response carrier.
    /\ response.confirmNeedInput \in BOOLEAN
    /\ response.coneClean \in Nodes \cup {NoNode}
    /\ response.auditPlan \in AuditPlanValues
    \* Reviewer-response audit-plan mutations (kernel
    \* `apply_review_audit_plan_actions`: `dismiss_audit_plan` /
    \* `dismissed_tasks`). The kernel-side `dismissed_tasks` is a
    \* `Vec<TaskDismissal>` (id + reason); the spec abstracts away the
    \* reason and models the carrier as a subset of the bounded
    \* `AuditTaskIds` universe, matching the action's usage
    \* (`\E taskId \in response.dismissedTasks: taskId \in DOMAIN auditPlan.tasks`).
    /\ response.dismissAuditPlan \in BOOLEAN
    /\ response.dismissedTasks \subseteq AuditTaskIds
    /\ (response.status = "none") <=> (response = NoResponse)

PendingTaskConsistent ==
    pendingTask = NoPendingTask
    \/
    /\ stage \in {"Start", "Worker"}
    /\ pendingTask.taskBlockers \subseteq GlobalBlockers
    /\ pendingTask.node = activeNode
    /\ pendingTask.mode = CurrentMode
    /\ pendingTask.orphanCleanupNodes \subseteq OrphanNodes(currentCoverage, presentNodes)
    /\ pendingTask.authorizedNodes \subseteq presentNodes

WrapperRequestConsistent ==
    /\ IF inFlightRequest.kind # "none" THEN
            /\ inFlightRequest.kind = RequestKindForStage(stage)
            /\ inFlightRequest = RequestRecord(inFlightRequest.id, RequestKindForStage(stage))
       ELSE
            RequestKindForStage(stage) = "none" \/ response = NoResponse
    /\ (response.status = "none")
       \/
       /\ inFlightRequest.kind # "none"
       /\ response.kind = inFlightRequest.kind
       /\ response.cycle = cycle

HumanGateMatchesState ==
    (gateKind # "none") <=> (stage = "HumanGate")

NoInvalidAdvanceFlow ==
    ~(invalidAttempt /\ (stage = "HumanGate" /\ gateKind = "advance"))

HeldTargetSuspendedByCorrBlockers ==
    ~(phase = "theorem_stating" /\ CorrespondenceBlockersExist /\ heldTarget # NoNode)

DifficultyStateConsistent ==
    /\ \A n \in Nodes:
        /\ nodeDifficulty[n] \in DifficultyValues
        /\ easyAttempts[n] \in Nat
        /\ (nodeDifficulty[n] = "hard" => easyAttempts[n] = 0)

\* Cleanup invariant (#40): the protocol only enters Cleanup with an
\* empty global blocker set, and Cleanup workers are restricted to
\* edits that don't re-open verifier lanes (no .tex, no signature,
\* no sorry). The invariant says: at every protocol pause point in
\* Cleanup, the run could legally terminate. Mirror of kernel
\* `formalization_complete()` precondition + `final_cleanup_preserving_step_result`.
CleanupHasNoBlockers ==
    phase = "cleanup" => GlobalBlockers = {}

\* Patch C (LOCAL_CLOSURE_IMPL_PLAN.md §1.1): structural fix for the
\* stale-pass closure gap. A sorry-free proof_node whose local-closure
\* record is stale (its dep's statement changed since last close, or
\* equivalent invalidation rule §7.3) sits in `localClosureUnverified`.
\* `FormalizationComplete` cannot be true while any such node exists
\* among proof_nodes. Therefore the engine cannot flip
\* ProofFormalization → Cleanup with a stale-pass importer in the live
\* set. This invariant is the spec-level capture of the §1.1 gap closure.
\*
\* Read together with `CleanupHasNoBlockers`: the two together say that
\* every legal Cleanup-phase state corresponds to a `FormalizationComplete`
\* Proof-phase state, including closure-clean.
StalePassClosurePreventsCleanupTransition ==
    \A n \in presentNodes:
        (n \in currentProofNodes /\ n \in localClosureUnverified)
            => ~FormalizationComplete

\* ----------------------------------------------------------------------
\* Cleanup-v2 invariants (2026-05-14, plan §16/§2)
\* ----------------------------------------------------------------------
\*
\* Together with the existing `CleanupHasNoBlockers` and
\* `StalePassClosurePreventsCleanupTransition`, these capture the design's
\* happy-to-stop contract: every Cleanup-phase boundary corresponds to a
\* fully-formalized state, task transitions are monotonic toward
\* terminal status, and no edit accepted during cleanup can drift a
\* protected-statement node's signature or .tex fingerprint.

\* Legal cleanup-task statuses (mirror of `model.rs CleanupTaskStatus`).
\* Used by CleanupTaskStatusTransitions to type-check the discriminant.
CleanupTaskStatusValues ==
    {"pending", "dismissed", "failed", "completed"}

\* Terminal-status discriminants (immutable once reached, per the
\* design's "Pending → terminal, terminal is sticky" rule).
CleanupTaskTerminalStatusValues ==
    {"dismissed", "failed", "completed"}

\* Cleanup task monotonicity: outside the audit sub-phase, the set of
\* Pending tasks never grows. Audit bursts may append (stage =
\* "CleanupAudit"); reviewer cycles, worker bursts, and re-audit-round
\* transitions only transition Pending → terminal.
\*
\* TLA invariants are single-state. The kernel-side two-state
\* guarantee (`apply_cleanup_review_response` + `apply_cleanup_worker_response`
\* + `apply_audit_response` only Pending → terminal transitions, never
\* terminal → Pending, never task removal) projects to two structural
\* single-state predicates that TLC can check on any populated trace:
\*
\*   (a) every task carries a `status` field that lives in the legal
\*       discriminant set, AND
\*   (b) the count of (Pending + terminal) status tasks equals the
\*       total task count — i.e., no out-of-band status, no missing
\*       status, no double-counted status.
\*
\* On the modeled traces, `cleanupAuditTasks` stays empty (the new
\* audit/reviewer-cleanup actions don't model task-record contents),
\* so both clauses are vacuous; the form is real and will fire on
\* any future trace that populates the sequence with malformed
\* records. The kernel-side guarantee survives translation to TLC
\* via the structural shape check.
CleanupTasksShrinkMonotonic ==
    \* (a) Every task has a legal status field.
    /\ \A i \in DOMAIN cleanupAuditTasks:
        cleanupAuditTasks[i].status \in CleanupTaskStatusValues
    \* (b) Status counts sum to Len (no out-of-band status).
    /\ Len(cleanupAuditTasks) =
        Cardinality({i \in DOMAIN cleanupAuditTasks:
            cleanupAuditTasks[i].status \in CleanupTaskStatusValues})

\* Cleanup task status transitions follow Pending → terminal only.
\* Terminal-status tasks (Completed/Failed/Dismissed) are immutable
\* once they reach a terminal state.
\*
\* TLA single-state form: every task's `status` discriminant is in
\* the legal value set. The "terminal stays terminal" two-state
\* guarantee is enforced kernel-side by `apply_cleanup_worker_response`
\* (Pending → Completed | Failed) and `apply_cleanup_review_response`
\* (Pending → Dismissed) — neither path reads a task with a terminal
\* status and emits a Pending replacement. The TLC-checkable
\* projection is the type predicate: malformed status records (the
\* only way TLC could observe a violation) would fail this check.
\* On the modeled traces this is vacuous (empty sequence); the form
\* survives translation and a future spec extension that populates
\* tasks structurally would gain the structural enforcement.
CleanupTaskStatusTransitions ==
    \A i \in DOMAIN cleanupAuditTasks:
        cleanupAuditTasks[i].status \in CleanupTaskStatusValues

\* Cleanup audit task targets are present nodes. The kernel's
\* `legal_cleanup_task` rejects any new task whose `target_node`
\* is absent from `live.present_nodes` at the moment of task creation.
\* This is a clean single-state invariant when the trace populates
\* `cleanupAuditTasks` with structural records.
\*
\* Exemption (per design): a Substitution task that has reached
\* terminal status `completed` may legitimately reference a target
\* node that has since been deleted (the task's own substitution
\* removed it). Pending-status tasks must reference a still-present
\* target.
\*
\* The set membership check requires the spec's `presentNodes`
\* (constant within a state). Vacuous on the empty sequence; non-
\* vacuous on any future trace populating task records.
CleanupAuditTargetsPresent ==
    \A i \in DOMAIN cleanupAuditTasks:
        cleanupAuditTasks[i].status \in CleanupTaskTerminalStatusValues
            \/ cleanupAuditTasks[i].target_node \in presentNodes

\* Cleanup exit implies formalized. Every Cleanup-phase exit
\* (phase' = "complete") happens from a `FormalizationComplete`
\* state. This is the spec-level capture of the "happy to stop"
\* guarantee — the kernel re-checks `formalization_complete()` at
\* every cleanup-worker accept, and the reviewer Done arm cannot
\* transition phase = "complete" from a non-formalized state.
\* The action-side guarantee is in `apply_cleanup_worker_response`
\* (engine.rs:1252+, "cleanup invariant" block) and the reviewer
\* Done path (engine.rs:3835+, transitions to Phase::Complete).
\* Single-state form: phase = "complete" implies the kernel-level
\* FormalizationComplete still holds.
CleanupExitImpliesFormalized ==
    phase = "complete" => FormalizationComplete

\* ----------------------------------------------------------------------
\* Tier 1 invariants (state-relation coherence)
\* ----------------------------------------------------------------------
\*
\* These three invariants capture cross-field coherence properties that
\* every modeled trace must satisfy at every state. They are the
\* foundation that downstream tiers (request/response well-formedness,
\* protocol-flow legality) reduce to. Each one is a single-state
\* predicate the simulation harness checks on every visited state.

\* Tier 1.1: when a verifier verdict is "decisive" — Pass or Fail on a
\* node, Pass/Fail/Structural on soundness — the approved fingerprint
\* must equal the current fingerprint. This is the "approvedFp =
\* currentFp at the moment of pinning" rule that powers fingerprint-
\* drift detection: derived predicates like `current_*_pass(n)` return
\* true iff `status = Pass AND current_fp = approved_fp`, so a Pass
\* verdict only "sticks" while the content hasn't drifted since
\* approval. If approvedFp were ever pinned to a fingerprint the
\* verifier never produced, a Pass could survive a later edit and a
\* stale verdict would silently override fresh content.
\*
\* Two kernel-side write paths must respect this contract:
\*   1. `apply_review_blocker_adjudication` (model.rs:8327) writes
\*      approvedFp = currentFp on either verdict; the post-2026-04-26
\*      audit hardening removed the fallback to `blocker.fingerprint`
\*      when current_fp is missing — see PROCESS_SEMANTICS §4.4
\*      "Guards on adjudication" #3 ("No fallback approvedFp").
\*   2. Verifier-driven `apply_corr_updates` (engine.rs:5130) /
\*      `apply_sound_updates` (engine.rs:5172) /
\*      `apply_substantiveness_updates` (engine.rs:5098) /
\*      `apply_paper_updates` write approvedFp = currentFp on each
\*      panel-decisive verdict.
\*
\* Soundness's "structural" verdict counts as decisive in this contract
\* — it is a fail-shaped pinning, not a deferred verdict (Unknown).
\*
\* The substantiveness lane is dormant in Cleanup/Complete (returns
\* "pass" unconditionally via `CurrentSubstantivenessState`), but the
\* underlying `substantivenessStatus[n]` map is not necessarily reset
\* on phase entry. The contract here is about the fingerprint pinning
\* discipline at the moment of write; the lane-dormant cases are
\* covered because if a write ever occurred it must have respected the
\* contract.
FingerprintPinnedOnDecisiveStatus ==
    /\ \A n \in presentNodes :
         /\ corrStatus[n] \in {"pass", "fail"}
              => corrApprovedFp[n] = corrCurrentFp[n]
         /\ soundStatus[n] \in {"pass", "fail", "structural"}
              => soundApprovedFp[n] = soundCurrentFp[n]
         /\ substantivenessStatus[n] \in {"pass", "fail"}
              => substantivenessApprovedFp[n] = substantivenessCurrentFp[n]
    /\ \A t \in configuredTargets :
         paperStatus[t] \in {"pass", "fail"}
              => paperApprovedFp[t] = paperCurrentFp[t]
    \* Deviation lane: any Pass/Fail status must carry a non-empty
    \* approved fingerprint. The kernel writes `deviation_status[id]`
    \* and `deviation_approved_fingerprints[id]` atomically in
    \* `apply_deviation_updates` (engine.rs:5430-5443) and the
    \* reviewer-override path (`apply_review_blocker_adjudication`,
    \* model.rs:9572-9589), so a decisive status implies a pinned
    \* approval fp. Note this does NOT require `approvedFp =
    \* currentFp` — the live fp can drift later (via
    \* `observe_deviation_fingerprints`) without re-writing approvedFp,
    \* which is exactly how `CurrentDeviationState` (efaafa7 sticky
    \* semantics, model.rs:6576-6594) reads Unknown on drift.
    /\ \A id \in Deviations :
         (   /\ deviationFiles[id]
             /\ deviationStatus[id] \in {"pass", "fail"}
         ) => deviationApprovedFp[id] # NoFingerprint

\* Tier 1.2: GlobalBlockers is exactly the set of unresolved checks.
\* Bidirectional alignment between the lane-pass predicates and the
\* `GlobalBlockers` set: a blocker of kind K is present iff the
\* corresponding lane-pass predicate is false (and, where applicable,
\* the gating "needs" predicate is true).
\*
\* The `*BlockersFor` constructors (NodeCorrBlockersFor at 2582,
\* SubstantivenessBlockersFor at 2594, SoundBlockersFor at 2602,
\* PaperBlockersFor at 2586) already encode the forward direction by
\* construction. The reverse direction is the actual content of this
\* invariant: GlobalBlockers carries no spurious blockers — every
\* recorded blocker corresponds to a real lane miss.
\*
\* Gating:
\*   * `node_corr`: every present node — the corr lane runs unconditionally
\*     in TheoremStating + ProofFormalization (no gating predicate).
\*   * `soundness`: only `NeedsSound(n)` nodes (present /\ proof /\ open)
\*     can carry a soundness blocker; SoundBlockersFor filters on the
\*     same condition.
\*   * `substantiveness`: only fires in TheoremStating + ProofFormalization
\*     (Cleanup/Complete: dormant, lane returns "pass"). The
\*     `SubstantivenessBlockersFor` constructor short-circuits to {} in
\*     other phases, so this invariant must gate the reverse direction
\*     on the same phase predicate to stay in lockstep.
\*   * `paper_faithfulness`: every configured target.
\*
\* Blocker kind strings are the literals emitted by the constructors
\* — `"node_corr"`, `"soundness"`, `"substantiveness"`,
\* `"paper_faithfulness"`. No `"paper"` blocker kind exists (that
\* string is a request kind, not a blocker kind — see RequestKinds at
\* line 126 vs BlockerUniverse at 890).
GlobalBlockersExhaustive ==
    /\ \A n \in presentNodes :
         (~ CurrentNodeCorrPass(n))
           <=> (\E b \in GlobalBlockers :
                  b.kind = "node_corr" /\ b.object.node = n)
    /\ \A n \in presentNodes :
         (NeedsSound(n) /\ ~ CurrentSoundPass(n))
           <=> (\E b \in GlobalBlockers :
                  b.kind = "soundness" /\ b.object.node = n)
    /\ \A t \in configuredTargets :
         (~ CurrentPaperPass(t))
           <=> (\E b \in GlobalBlockers :
                  b.kind = "paper_faithfulness" /\ b.object.target = t)
    /\ phase \in {"theorem_stating", "proof_formalization"}
         => \A n \in presentNodes :
              (~ CurrentSubstantivenessPass(n))
                <=> (\E b \in GlobalBlockers :
                       b.kind = "substantiveness" /\ b.object.node = n)
    \* Deviation: any live id whose state is not Pass is a global
    \* blocker (kernel `global_blockers`, model.rs:7099-7115). Phase-
    \* independent — Cleanup and Complete inherit; the routing
    \* constraint (no worker can take the blocker in Cleanup) is a
    \* separate concern captured by `ReviewTaskBlockerInWorkerScope`.
    /\ \A id \in Deviations :
         (deviationFiles[id] /\ ~CurrentDeviationPass(id))
           <=> (\E b \in GlobalBlockers :
                  b.kind = "deviation" /\ b.object.deviation = id)

\* Tier 1.3: at every protocol resting point, the live tier equals the
\* committed mirror. The resting points are between-cycle quiescent
\* states (stage = "Start"), with no in-flight wrapper request and no
\* outstanding retry — i.e., the kernel has just finished a
\* `commit_live` and is waiting for the next `StartCycle` event.
\*
\* The kernel-side guarantee is in `commit_live` (model.rs:2004):
\* every entry in the live tier is snapshotted into its committed
\* mirror at every clean stage transition. At a true between-cycle
\* resting point, the two tiers are equal by construction.
\*
\* Antecedent shape:
\*   * `stage = "Start"` — between cycles, no panel in flight.
\*   * `retryOutcomeKind = "none"` — no carry-over retry context that
\*     would re-issue a Worker request on the same cycle's state.
\*   * `inFlightRequest.kind = "none"` — no wrapper-boundary request
\*     pending; eliminates the brief Start-stage moment when a request
\*     has been emitted but not yet picked up by stage transition.
\*
\* Live-vs-committed fields covered (matching the
\* `CommitCurrentWorktree` operator at line 5221):
\*   * structural tier: presentNodes, currentNodeKinds, currentProofNodes,
\*     currentDeps, currentTargetClaims, openNodes
\*   * fingerprint tier: corrCurrentFp, soundCurrentFp, paperCurrentFp,
\*     substantivenessCurrentFp
\*   * local-closure tier: localClosureUnverified (Patch C §7.7).
\*
\* Fields intentionally not checked here:
\*   * currentCoverage / committedCoverage — derived from
\*     currentTargetClaims; if the claims agree, coverage agrees by
\*     construction (via DeriveCoverage).
\*   * currentTargetFp / committedTargetFp — modeled but not part of
\*     the verifier-checked fingerprint contract this invariant
\*     captures.
QuiescentLiveEqualsCommitted ==
    (stage = "Start" /\ retryOutcomeKind = "none"
        /\ inFlightRequest.kind = "none")
    => /\ presentNodes        = committedPresentNodes
       /\ currentNodeKinds    = committedNodeKinds
       /\ currentProofNodes   = committedProofNodes
       /\ currentDeps         = committedDeps
       /\ currentTargetClaims = committedTargetClaims
       /\ openNodes           = committedOpenNodes
       /\ corrCurrentFp       = committedCorrCurrentFp
       /\ soundCurrentFp      = committedSoundCurrentFp
       /\ paperCurrentFp      = committedPaperCurrentFp
       /\ substantivenessCurrentFp = committedSubstantivenessCurrentFp
       /\ localClosureUnverified = committedLocalClosureUnverified

\* ----------------------------------------------------------------------
\* Tier 4 invariants (wrapper envelope and restart safety)
\* ----------------------------------------------------------------------
\*
\* These invariants pin down the wrapper request envelope as the
\* protocol-side mirror of the Rust kernel's WrapperRequest record. They
\* are the structural "restart-safety contract": after a process restart
\* the protocol can only resume a request whose payload is reconstructible
\* from observable state. If any of these were to fail in TLC, that would
\* indicate either:
\*   - a request id ever leaked out of the monotone seq counter
\*     (RequestIdMonotonic),
\*   - an in-flight request that disagrees with the deterministic
\*     RequestRecord(...) reconstruction from observable state
\*     (RequestPayloadDerivable, the keystone),
\*   - an in-flight request claiming to have been issued in a future
\*     protocol cycle (InFlightCycleBounded).
\*
\* Together they say: if the supervisor crashes mid-cycle and is
\* restarted, the (id, stage, current observable state) tuple is enough
\* to rebuild the exact request envelope that was in flight. There is no
\* hidden request-side state. Mirror of PROCESS_SEMANTICS.md §10
\* ("Runtime restart mid-cycle") and §3 ("in-flight request, cycle
\* skeleton"), and the kernel-side `WrapperRequest::rebuild_from_state`
\* contract (kernel/src/wrapper/request.rs).

\* RequestIdMonotonic (Tier 4.1): the in-flight request id is always a
\* legal handle into the request-seq history. Specifically:
\*   - the id field always lies in 0..requestSeq, and
\*   - id = 0 exactly when no request is in flight (kind = "none"),
\*     matching NoRequest at spec line 796 which sets id |-> 0, and
\*   - id is a positive issued seq value (1..requestSeq) whenever a
\*     request is in flight.
\* Together this rules out:
\*   - a "live" inFlightRequest with id 0 (would be ambiguous with
\*     NoRequest at restart time),
\*   - a "cleared" inFlightRequest with id > 0 (would imply stale
\*     payload leaking past clear),
\*   - an in-flight request whose id is past the high-water seq counter
\*     (would imply a future id was minted without bumping requestSeq).
\* Mirror of kernel `WrapperRequest::id` invariant: every issued request
\* bumps the seq counter monotonically and stores the post-bump value.
RequestIdMonotonic ==
    /\ inFlightRequest.id \in 0..requestSeq
    /\ (inFlightRequest.kind = "none") => inFlightRequest.id = 0
    /\ (inFlightRequest.kind # "none") => inFlightRequest.id \in 1..requestSeq

\* RequestPayloadDerivable (Tier 4.2): the keystone restart-safety
\* property. Whenever a request is in flight, its payload is exactly
\* what RequestRecord(inFlightRequest.id, RequestKindForStage(stage))
\* would compute from current observable state — i.e., the protocol's
\* mirror of the kernel's "rebuild the request envelope from state"
\* contract.
\*
\* RequestRecord at spec line 4459 is a pure function of (reqId, kind)
\* and current observable state (cycle, phase, activeNode, heldTarget,
\* CurrentMode, blockers, configured/blocked targets, verify nodes/
\* lanes/bindings, allowed decisions/resets/overrides, currentPresent/
\* Proof/Deps/Coverage, reviewer comments, evidence lanes, etc. — all
\* of these are observable variables of the spec). RequestKindForStage
\* at spec line 4528 derives the kind discriminant from `stage`, which
\* is also observable. So the right-hand side reconstructs the entire
\* request envelope from (inFlightRequest.id, stage, current state).
\* The id itself is bounded into the legal seq window by
\* RequestIdMonotonic (Tier 4.1).
\*
\* The existing WrapperRequestConsistent at spec line 4886 already
\* asserts this equality as part of a larger conjunction also covering
\* the kind/stage match and the response/cycle pairing. We pull the
\* derivability clause out and name it explicitly, because it is the
\* load-bearing restart-safety predicate: future restructurings of
\* WrapperRequestConsistent must not silently weaken this clause.
\*
\* If this ever fails in TLC, the trace exhibits a state from which a
\* mid-cycle restart cannot deterministically rebuild the request
\* envelope — i.e., the kernel-side `WrapperRequest::rebuild_from_state`
\* would either disagree with the persisted record or be undefined.
RequestPayloadDerivable ==
    inFlightRequest.kind # "none"
        => inFlightRequest = RequestRecord(inFlightRequest.id, RequestKindForStage(stage))

\* InFlightCycleBounded (Tier 4.3): a weak monotonicity sanity check on
\* the request envelope's `cycle` field. No in-flight request may claim
\* to come from a future protocol cycle.
\*
\* The RequestRecord constructor at spec line 4459 stamps every request
\* with cycle |-> cycle (current state). Combined with
\* RequestPayloadDerivable, this implies the stronger
\* inFlightRequest.cycle = cycle while a request is in flight. We keep
\* InFlightCycleBounded as the strictly weaker <= form intentionally:
\*   - it is the actual semantic restart-safety contract (a request
\*     persisted at cycle k must not deserialize as claiming a cycle
\*     past current),
\*   - it survives even if a future spec refactor relaxes the
\*     RequestPayloadDerivable equality on cycle (e.g. lets some action
\*     bump `cycle` while a request is in flight before consuming the
\*     response),
\*   - it is independently TLC-checkable without re-evaluating the
\*     full RequestRecord projection.
\* If RequestRecord were ever changed to drop the `.cycle` field, this
\* invariant must be revisited together with PROCESS_SEMANTICS.md §3.
InFlightCycleBounded ==
    inFlightRequest.kind # "none" => inFlightRequest.cycle <= cycle

\* ----------------------------------------------------------------------
\* Tier 2 invariants (phase dormancy and lifecycle gates)
\* ----------------------------------------------------------------------
\*
\* These invariants enforce the supervisor's "no cross-phase drift"
\* contract: every conditional field sits in its no-op value outside
\* its home phase, every stage discriminator matches its predicate
\* expression, and every cleanup-audit lifecycle counter stays inside
\* its design-bound numeric envelope. Read together with the cleanup-v2
\* cluster above, they say: the protocol cannot leak intra-phase
\* state outside its home phase, and the cleanup audit lane cannot
\* outrun its round/burst caps.
\*
\* See PROCESS_SEMANTICS.md §§4.6, 4.7, 4.8, 4.9, 4.10 (cleanup, audit,
\* anchor lifecycles) for the prose contract that these check.

\* === Tier 2 invariants (phase dormancy and lifecycle gates) ===

\* Phase dormancy: every conditional field sits in its no-op value
\* outside its home phase. Three home-phase regions are encoded:
\*
\*   - theorem_stating: heldTarget/targetEditMode are only meaningful
\*     during target selection. Already enforced by TypeOK lines
\*     4602-4603 -- this invariant is the named single-statement
\*     restatement that documents the contract independently of the
\*     type predicate. Duplication with TypeOK is intentional: if a
\*     future spec extension relaxes TypeOK for these fields, this
\*     invariant remains as the semantic contract.
\*
\*   - proof_formalization: proofEditMode/activeCoarseNode/
\*     cyclesInCoarseRepairMode/forceReviewAfterConeClean are only
\*     meaningful during proof formalization. TypeOK lines 4604,
\*     4662, 4665 already encode the first three; this invariant
\*     gathers them in one named statement and adds the
\*     forceReviewAfterConeClean dormancy. The cone-clean latch
\*     (`forceReviewAfterConeClean`) is set TRUE by
\*     `AcceptStuckMathAuditConeClean` (mirror of kernel
\*     `apply_audit_authorized_theorem_stating_node_reset`) and
\*     cleared on the next `StartCycle` after dispatching the routing
\*     Reviewer; the dormancy clause keeps the latch confined to
\*     proof_formalization/cleanup.
\*
\*   - cleanup or complete: the cleanup-v2 audit lane state
\*     (cleanupAuditTasks, cleanupAuditRound, cleanupActiveTask,
\*     cleanupAuditBurstCount) only mutates inside cleanup-phase
\*     audit actions. The audit actions guard on stage =
\*     "CleanupAudit"; that stage is currently not in StageValues
\*     (line 81-91), so the actions are dead code in the modeled
\*     traces and the audit state stays at its Init values
\*     (tasks <<>>, round 1, activeTask NoTask, burst 0) outside
\*     cleanup. The invariant captures the design intent: outside
\*     cleanup/complete the audit lane is dormant. Note that
\*     cleanupAuditRound's "dormant" value is 1 (the Init value),
\*     not 0 -- the cleanup audit is 1-indexed (see Init line 5145
\*     and ReviewerCleanupReAudit line 5953).
\*
\*   - complete: the terminal phase. stage must be "Complete" (only
\*     reached via ReviewDoneCleanup line 11371) and GlobalBlockers
\*     must be empty (by induction from CleanupHasNoBlockers in the
\*     pre-transition state, since ReviewDoneCleanup leaves
\*     paperStatus/corrStatus/soundStatus/substantivenessStatus and
\*     all approved fingerprints unchanged).
\*
\* `auditPlan` / `supersededAuditPlan` are now modeled (this round) as
\* the AuditPlanVars cluster; producer is `AcceptStuckMathAudit*`
\* (publishes the plan on Valid audit response), consumers are
\* `RecordAuditPlan` (whole-plan dismissal) and `DismissAuditPlanTask`
\* (per-task dismissal). `stuckMathAuditActive` is modeled (2026-05-31)
\* as part of the StuckMathAuditVars cluster — see the
\* `StuckMathAuditLatchWellFormed` invariant below for its
\* well-formed clause additions.
PhaseDormancyContract ==
    /\ phase # "theorem_stating"
        => /\ heldTarget = NoNode
           /\ targetEditMode = "global"
    /\ phase # "proof_formalization"
        => /\ proofEditMode = "local"
           /\ activeCoarseNode = NoNode
           /\ cyclesInCoarseRepairMode = 0
           /\ forceReviewAfterConeClean = FALSE
    /\ phase \notin {"cleanup", "complete"}
        => /\ cleanupAuditTasks = << >>
           /\ cleanupAuditRound = 1
           /\ cleanupActiveTask = NoTask
           /\ cleanupAuditBurstCount = 0
    /\ phase = "complete"
        => /\ stage = "Complete"
           /\ GlobalBlockers = {}
           /\ stuckMathAuditActive = FALSE
           /\ stuckMathAuditNeedInputAudit = NoNeedInputAuditContext

\* Stage ownership: gate/task/request discriminants match the stages
\* they are predicated on.
\*
\*   - (gateKind # "none") <=> (stage = "HumanGate") is a restatement
\*     of HumanGateMatchesState (line 4898-4899); kept for symmetry
\*     within the Tier 2 cluster.
\*
\*   - pendingTask # NoPendingTask => stage \in {"Start", "Worker"} is
\*     a one-direction restatement of PendingTaskConsistent (line
\*     4876-4884): PendingTaskConsistent is a disjunction
\*     (NoPendingTask OR stage \in {Start,Worker} /\ ...), so the
\*     contrapositive of the second disjunct is exactly this
\*     implication. Kept for explicit single-name visibility.
\*
\*   - The third clause is narrowed from the original bidirectional
\*     spec because the converse `inFlightRequest.kind = "none" =>
\*     stage \in {"Start", "Complete"}` does not hold: after an
\*     Accept* action clears inFlightRequest and transitions to e.g.
\*     stage = "Worker", the state has kind = "none" but stage =
\*     "Worker" (until the next IssueWorkerRequest fires). The
\*     forward direction `stage \in {"Start", "Complete"} =>
\*     inFlightRequest.kind = "none"` does hold: every Start-bound
\*     transition (lines 5952, 8520, 8891, 9672, 9850, 10612, 11594)
\*     pairs with ClearArtifacts or sets inFlightRequest to NoRequest
\*     explicitly, and Complete is set only in ReviewDoneCleanup
\*     (line 11371) which clears inFlightRequest via
\*     ClearArtifactsAndRecordHistory.
StageOwnership ==
    /\ (gateKind # "none") <=> (stage = "HumanGate")
    /\ pendingTask # NoPendingTask => stage \in {"Start", "Worker"}
    /\ stage \in {"Start", "Complete"} => inFlightRequest.kind = "none"

\* Stuck-math-audit latch well-formed:
\*   - `forceReviewAfterConeClean`: cone-clean review-force latch can
\*     only be lit in phases where a cone-clean review is meaningful
\*     (proof_formalization or cleanup). Set by
\*     `AcceptStuckMathAuditConeClean` (mirror of kernel
\*     `apply_audit_authorized_theorem_stating_node_reset`) and
\*     cleared on the next `StartCycle`.
\*   - `stuckMathAuditActive` / `stuckMathAuditNeedInputAudit`: dispatch
\*     latch + NeedInput audit context, added 2026-05-31. The
\*     well-formed invariant:
\*       (1) Active latch implies stage \in {StuckMathAudit, HumanGate,
\*           Reviewer, Start, Worker} — the latch persists across the
\*           dispatch and its handoff (see kernel comment "kernel does
\*           NOT clear the latch here; only the plan is published" in
\*           apply_stuck_math_audit_response).
\*       (2) NeedInput audit context implies the latch is on
\*           (kernel `refresh_stuck_math_audit_latch` invariant at
\*           model.rs:10601).
\*       (3) Burst-retry counter is bounded by
\*           STUCK_MATH_AUDIT_BURST_RETRY_LIMIT.
\*
\* The `auditPlan` lane mutations (publication on Valid audit response,
\* per-task and whole-plan reviewer-side dismissals) are modeled by
\* `AcceptStuckMathAudit*` / `RecordAuditPlan` / `DismissAuditPlanTask`.
StuckMathAuditLatchWellFormed ==
    /\ forceReviewAfterConeClean => phase \in {"proof_formalization", "cleanup"}
    /\ stuckMathAuditNeedInputAudit # NoNeedInputAuditContext
        => stuckMathAuditActive
    /\ stuckMathAuditBurstRetryCount <= 1  \* STUCK_MATH_AUDIT_BURST_RETRY_LIMIT
    /\ stage = "StuckMathAudit" => stuckMathAuditActive

\* Coarse anchor well-formed: the active-coarse-anchor mechanism's
\* lifecycle invariants. Several clauses overlap TypeOK lines
\* 4661-4665 -- kept here as a single named aggregator for the
\* mechanism (proposal v32) so the contract is visible at one
\* invariant name rather than spread across the type predicate.
\*
\*   - activeCoarseNode membership: already enforced by TypeOK 4661.
\*   - coarseDagNodes nonempty implies phase has advanced past
\*     theorem_stating: coarseDagNodes is set exactly once in
\*     HumanApproveAdvance (line 11599, theorem_stating ->
\*     proof_formalization transition) and never cleared, so non-
\*     emptiness pins phase to {proof_formalization, cleanup,
\*     complete}.
\*   - cycle counter dormancy: already TypeOK 4665.
\*   - KernelHintedNext is empty when change is not allowed: this is
\*     true by the definition of KernelHintedNextActiveCoarseNodes
\*     (line 2687-2695, the IF branch returns {} on the negated
\*     condition). Kept as a documented consequence so a future
\*     refactor of the helper's structure cannot silently break the
\*     invariant.
\*   - active-node in cone: TypeOK line 4600 invokes ActiveNodeLegal,
\*     which (per the proof_formalization branch, line 2862-2874)
\*     enforces `node \in CoarseLegalActiveSet`. This clause aggregates
\*     that consequence at the named-invariant level.
CoarseAnchorWellFormed ==
    /\ activeCoarseNode \in (coarseDagNodes \cup {NoNode})
    /\ (coarseDagNodes # {}) => phase \in {"proof_formalization", "cleanup", "complete"}
    /\ (activeCoarseNode = NoNode) => cyclesInCoarseRepairMode = 0
    /\ ~ ActiveCoarseChangeAllowed => KernelHintedNextActiveCoarseNodes = {}
    /\ /\ phase = "proof_formalization"
       /\ activeNode # NoNode
       /\ activeCoarseNode # NoNode
       => activeNode \in CoarseLegalActiveSet

\* global_repair_mode S8 anchor-change safety invariant. The kernel
\* (model.rs ~6500) gates ActiveCoarseChangeAllowed on
\* `ever_shallow_coarse_closed_regressed().is_empty()`: a non-empty
\* regression set forbids re-selecting an anchor (the global-repair
\* lane is supposed to fix the regression before the reviewer picks
\* a new anchor). Spec-side restatement:
\*
\*   When `EverShallowCoarseClosedRegressed` is non-empty,
\*   `KernelHintedNextActiveCoarseNodes` must be empty (the reviewer
\*   can pick no anchor candidates), which keeps the anchor-change
\*   lock held until the regression set drains.
\*
\* This restates the safety predicate at the spec level so any
\* future refactor of `ActiveCoarseChangeAllowed` /
\* `KernelHintedNextActiveCoarseNodes` keeps the global_repair_mode
\* contract intact. Mirrors model.rs `ever_shallow_coarse_closed`
\* + the kernel call at ~6500 that AND-folds it into the change
\* gate.
AnchorChangeForbiddenDuringGlobalRepair ==
    EverShallowCoarseClosedRegressed # {} =>
        KernelHintedNextActiveCoarseNodes = {}

\* Cleanup audit round bound: the round and burst counters stay in
\* their design-bounded ranges.
\*
\*   - cleanupAuditRound \in 1..2: the audit lane is 1-indexed
\*     (Init line 5145 = 1) with a hard cap of 2 rounds
\*     (ReviewerCleanupReAudit guard at line 5950 is `< 2`, and the
\*     post-state is `cleanupAuditRound + 1`).
\*   - cleanupAuditBurstCount \in 0..5: bursts start at 0 (Init line
\*     5144) with a hard cap of 5 per round (AcceptCleanupAudit*
\*     guards at lines 5713/5730 are `< 5`, and the post-state is
\*     `+ 1`; ReviewerCleanupReAudit at line 5954 resets to 0 on
\*     round change).
\*
\* The original Tier 2 design proposed a biconditional
\* `(cleanupAuditRound = 0) <=> (cleanupAuditTasks = << >>)`. That
\* fails at Init: cleanupAuditRound = 1 (the audit lane is 1-indexed,
\* not 0-indexed) and cleanupAuditTasks = << >>. The biconditional
\* clause is omitted here because it does not correspond to the
\* actual round numbering; the bound clauses above retain the
\* counter-envelope guarantee that motivated it.
CleanupAuditRoundBound ==
    /\ cleanupAuditRound \in 1..2
    /\ cleanupAuditBurstCount \in 0..5

Init ==
    /\ phase = "theorem_stating"
    /\ stage = "Start"
    /\ cycle = 0
    /\ attempt = 0
    /\ requestSeq = 0
    /\ invalidAttempt = FALSE
    /\ retryOutcomeKind = "none"
    /\ gateKind = "none"
    /\ gateFromInvalidAttempt = FALSE
    /\ activeNode = NoNode
    /\ heldTarget = NoNode
    /\ targetEditMode = "global"
    /\ proofEditMode = "local"
    /\ configuredTargets = InitialConfiguredTargets
    /\ approvedConfiguredTargets = {}
    /\ currentNodeKinds = InitialNodeKinds
    /\ committedNodeKinds = InitialNodeKinds
    /\ currentProofNodes = {}
    /\ committedProofNodes = {}
    /\ currentDeps = DefaultNodeSetMap
    /\ committedDeps = DefaultNodeSetMap
    /\ currentTargetClaims = DefaultTargetClaimMap
    /\ committedTargetClaims = DefaultTargetClaimMap
    /\ presentNodes = InitialPresentNodes
    /\ committedPresentNodes = InitialPresentNodes
    /\ openNodes = {}
    /\ committedOpenNodes = {}
    \* Patch C (LOCAL_CLOSURE_IMPL_PLAN.md §7.2): both tiers start empty.
    \* Migration (§7.10) populates these post-deploy; the abstract spec
    \* models that as the worker-acceptance actions choosing a fresh
    \* unverified set non-deterministically.
    /\ localClosureUnverified = {}
    /\ committedLocalClosureUnverified = {}
    /\ currentCoverage = DefaultCoverage
    /\ committedCoverage = DefaultCoverage
    /\ approvedCoverage = DefaultCoverage
    /\ paperStatus = DefaultPaper
    /\ paperCurrentFp = DefaultTargetFp
    /\ committedPaperCurrentFp = DefaultTargetFp
    /\ paperApprovedFp = DefaultTargetFp
    /\ substantivenessStatus = DefaultSubstantiveness
    /\ substantivenessCurrentFp = DefaultFp
    /\ committedSubstantivenessCurrentFp = DefaultFp
    /\ substantivenessApprovedFp = DefaultFp
    /\ currentTargetFp = DefaultFp
    /\ committedTargetFp = DefaultFp
    /\ approvedTargetFp = DefaultFp
    /\ coarseDagNodes = {}
    /\ corrStatus = InitialCorr
    /\ corrCurrentFp = DefaultFp
    /\ committedCorrCurrentFp = DefaultFp
    /\ corrApprovedFp = DefaultFp
    /\ soundStatus = DefaultSound
    /\ soundCurrentFp = DefaultFp
    /\ committedSoundCurrentFp = DefaultFp
    /\ soundApprovedFp = DefaultFp
    \* Sound assessment store: all nodes start at `fresh_unknown` (no
    \* stored assessment), no reviewer-requested verifier dispatches
    \* queued, no reverification context.
    /\ soundAssessmentStatus = DefaultSoundAssessmentStatus
    /\ reviewerRequestedSoundVerifierNodes = {}
    /\ soundReverificationContext = NoSoundReverificationContext
    \* Deviation lane (2026-05-27/28). Init: no deviations alive, all
    \* maps at default. The lifecycle (creation, fingerprint drift,
    \* worker retire, verifier verdict) is exercised by the new
    \* env/worker actions in this module.
    /\ deviationFiles = DefaultDeviationFiles
    /\ committedDeviationFiles = DefaultDeviationFiles
    /\ deviationStatus = DefaultDeviationStatus
    /\ deviationCurrentFp = DefaultDeviationFp
    /\ committedDeviationCurrentFp = DefaultDeviationFp
    /\ deviationApprovedFp = DefaultDeviationFp
    /\ nodeDeviationClaims = DefaultNodeDeviationClaims
    /\ committedNodeDeviationClaims = DefaultNodeDeviationClaims
    /\ lastCleanDeviationFiles = DefaultDeviationFiles
    /\ lastCleanDeviationStatus = DefaultDeviationStatus
    /\ lastCleanDeviationApprovedFp = DefaultDeviationFp
    /\ lastCleanNodeDeviationClaims = DefaultNodeDeviationClaims
    /\ latestDeviationReviewIds = {}
    /\ latestDeviationEvidenceLanes = {}
    /\ nodeDifficulty = DefaultDifficulty
    /\ easyAttempts = DefaultEasyAttempts
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
    /\ hasEverBeenClean = FALSE
    /\ pendingTask = NoPendingTask
    \* Cleanup-v2 (2026-05-14). Empty list, "", counters 0, round 1,
    \* active=NoTask, force_done=FALSE. Mirrors `Default for
    \* ProtocolState` (model.rs).
    /\ cleanupAuditTasks = <<>>
    /\ cleanupAuditScratchpad = ""
    /\ cleanupAuditBurstCount = 0
    /\ cleanupAuditRound = 1
    /\ cleanupConsecutiveInvalidWorkers = 0
    /\ cleanupActiveTask = NoTask
    /\ cleanupForceDone = FALSE
    \* Cone-clean (2026-05-19): cleared at boot; only set TRUE by the
    \* StuckMathAudit-authorized reset, which is currently unmodeled.
    /\ forceReviewAfterConeClean = FALSE
    \* Active coarse anchor (proposal v32): no anchor at boot. Seeded
    \* on first proof-formalization review via ReviewerContinue with
    \* nextActiveCoarse # NoNode (kernel offers KernelHintedNext...
    \* candidates; reviewer picks).
    /\ activeCoarseNode = NoNode
    /\ cyclesInCoarseRepairMode = 0
    \* StuckMathAudit producer (2026-05-31): latch off at boot, no
    \* NeedInput escalation queued, retry counter zero, never
    \* dispatched. Mirrors kernel `Default for ProtocolState` —
    \* `StuckMathAuditState::default()` is all-zero, and
    \* `stuck_math_audit_burst_retry_count = 0`,
    \* `last_stuck_math_audit_dispatched_cycle = None`.
    /\ stuckMathAuditActive = FALSE
    /\ stuckMathAuditNeedInputAudit = NoNeedInputAuditContext
    /\ stuckMathAuditBurstRetryCount = 0
    /\ lastStuckMathAuditDispatchedCycle = NoCycle
    \* global_repair_mode: no Step A / Step B / decline at boot;
    \* history empty; kill-switch ON by default (mirrors kernel
    \* `global_repair_mode_enabled = true`).
    /\ pendingGlobalRepairRequest = NoGlobalRepairRequest
    /\ pendingGlobalRepairGrant = NoGlobalRepairGrant
    /\ latestGlobalRepairAuditDeclineReason = ""
    /\ latestGlobalRepairAuditDeclineCycle = NoCycle
    /\ lastReviewerGlobalRepairRequestCycle = NoCycle
    /\ everShallowCoarseClosed = {}
    /\ globalRepairModeEnabled = TRUE
    \* Post-advance routing latch off at boot; first set TRUE by
    \* HumanApproveAdvance.
    /\ postAdvanceRoutingPending = FALSE
    \* Protected-target reapproval: no nodes flagged, no semantic
    \* scope confirmation queued.
    /\ pendingProtectedReapprovalNodes = {}
    /\ pendingProtectedSemanticScopeConfirmation = NoProtectedSemanticChangeConfirmation
    \* StuckMathAudit audit-plan lane: no plan and no superseded plan
    \* at boot. First set by AcceptStuckMathAuditBackToReviewer (or its
    \* HumanGate sibling).
    /\ auditPlan = NoAuditPlan
    /\ supersededAuditPlan = NoAuditPlan
    /\ inFlightRequest = NoRequest
    /\ response = NoResponse

ClearArtifacts ==
    /\ response' = NoResponse
    /\ inFlightRequest' = NoRequest
    /\ UNCHANGED requestSeq

RecordNativeHistory ==
    /\ nativeHistoryKinds' = nativeHistoryKinds \cup {<<inFlightRequest.kind, inFlightRequest.phase>>}

ClearArtifactsAndRecordHistory ==
    /\ ClearArtifacts
    /\ RecordNativeHistory

RestoreCommittedWorktree ==
    /\ currentNodeKinds' = committedNodeKinds
    /\ currentProofNodes' = committedProofNodes
    /\ currentDeps' = committedDeps
    /\ currentTargetClaims' = committedTargetClaims
    /\ presentNodes' = committedPresentNodes
    /\ openNodes' = committedOpenNodes
    \* Patch C (LOCAL_CLOSURE_IMPL_PLAN.md §7.7): closure live tier
    \* rolls back from the committed mirror on rejection.
    /\ localClosureUnverified' = committedLocalClosureUnverified
    /\ currentCoverage' = committedCoverage
    /\ paperCurrentFp' = committedPaperCurrentFp
    /\ substantivenessCurrentFp' = committedSubstantivenessCurrentFp
    /\ currentTargetFp' = committedTargetFp
    /\ corrCurrentFp' = committedCorrCurrentFp
    /\ soundCurrentFp' = committedSoundCurrentFp

\* Faithful refinement of kernel `apply_last_clean_reset`
\* (model.rs:2389-2461). Restores structure from the committed state
\* (same as RestoreCommittedWorktree) but additionally clears current
\* fingerprints to defaults — forcing re-verification on the rewound
\* state. The action that uses this also clears status maps
\* (paperStatus / corrStatus / soundStatus) via separate prime
\* assignments. Approved fingerprints are intentionally preserved
\* (mirrors the kernel restoring approved_fp from the last_clean_*
\* mirrors).
ApplyLastCleanReset ==
    /\ currentNodeKinds' = committedNodeKinds
    /\ currentProofNodes' = committedProofNodes
    /\ currentDeps' = committedDeps
    /\ currentTargetClaims' = committedTargetClaims
    /\ presentNodes' = committedPresentNodes
    /\ openNodes' = committedOpenNodes
    \* Patch C (LOCAL_CLOSURE_IMPL_PLAN.md §7.8): LastClean restores the
    \* closure live tier from the committed mirror (the spec abstracts
    \* the kernel's separate `last_clean_*` tier into the committed tier
    \* the same way it does for verifier `*_current_fp` mirrors). The
    \* `last_clean_local_closure_mirror_ready` readiness gate (§7.8) is
    \* not modeled here because the spec already abstracts the kernel's
    \* `last_clean_verifier_mirror_ready` flag away.
    /\ localClosureUnverified' = committedLocalClosureUnverified
    /\ currentCoverage' = committedCoverage
    /\ paperCurrentFp' = DefaultTargetFp
    /\ substantivenessCurrentFp' = DefaultFp
    /\ currentTargetFp' = DefaultFp
    /\ corrCurrentFp' = DefaultFp
    /\ soundCurrentFp' = DefaultFp

CommitCurrentWorktree ==
    \* Mirrors kernel `commit_live` (model.rs:2004). cyclesSinceClean is
    \* zeroed and hasEverBeenClean set TRUE at any checkpoint where the
    \* post-commit global blocker set is empty; otherwise the counter
    \* is bumped. The kernel stores a u32 counter but only ever checks
    \* `>= 1` (model.rs:2427 gates LastClean on exactly this), so the
    \* spec caps it at 1 to keep the state space finite.
    /\ committedNodeKinds' = currentNodeKinds
    /\ committedProofNodes' = currentProofNodes
    /\ committedDeps' = currentDeps
    /\ committedTargetClaims' = currentTargetClaims
    /\ committedPresentNodes' = presentNodes
    /\ committedOpenNodes' = openNodes
    \* Patch C (LOCAL_CLOSURE_IMPL_PLAN.md §7.7, §7.8): commit_live
    \* snapshots the closure live tier into the committed mirror.
    /\ committedLocalClosureUnverified' = localClosureUnverified
    /\ committedCoverage' = currentCoverage
    /\ committedPaperCurrentFp' = paperCurrentFp
    /\ committedSubstantivenessCurrentFp' = substantivenessCurrentFp
    /\ committedTargetFp' = currentTargetFp
    /\ committedCorrCurrentFp' = corrCurrentFp
    /\ committedSoundCurrentFp' = soundCurrentFp
    /\ cyclesSinceClean' = IF GlobalBlockers = {} THEN 0 ELSE 1
    /\ hasEverBeenClean' = (hasEverBeenClean \/ (GlobalBlockers = {}))

ReviewDecisionLegal ==
    /\ response.status = "ok"
    /\ response.kind = "review"
    /\ response.cycle = cycle
    \* Option C (2026-06-04): overrideBlockers must always be empty;
    \* the override→Pass authority is retired. Replaces the prior
    \* subset check against the always-empty
    \* `RequestAllowedOverrideBlockers`.
    /\ response.overrideBlockers = {}
    /\ response.resetBlockers \subseteq RequestAllowedResetBlockers("review")
    /\ response.taskBlockers \cap response.resetBlockers = {}
    \* A whole-state reset is a pure rollback: blockers are inherited from
    \* the rewound state and the reviewer must not simultaneously adjudicate
    \* the current state's blockers. Mirror of kernel
    \* `review_response_legal` in model.rs.
    /\ response.reset # NoCheckpoint
       => /\ response.taskBlockers = {}
          /\ response.resetBlockers = {}
    /\ \A b \in response.taskBlockers :
        ReviewTaskBlockerInWorkerScope(b, response)
    \* `authorizedNodes` invariants for proof formalization
    \* Continue+Restructure/CoarseRestructure: the explicit list must
    \* be non-empty for those modes and empty for Local. Every entry
    \* must be a present node and lie in the scope envelope, extended by
    \* any pending global-repair grant that the reviewer consumes.
    \* (The runtime additionally allows protected_semantic_change_nodes
    \* outside the strict envelope; the TLA spec does not yet model
    \* that field, so the abstract check uses the strict envelope.)
    /\ /\ phase = "proof_formalization"
       /\ response.decision = "CONTINUE"
       /\ response.reset = NoCheckpoint
       => /\ response.nextMode \in {"restructure", "coarse_restructure"}
            => response.authorizedNodes # {}
          /\ response.nextMode = "local"
            => response.authorizedNodes = {}
    /\ response.authorizedNodes \subseteq presentNodes
    /\ response.authorizedNodes \subseteq
            ReviewScopeEnvelopeWithGlobalRepair(response)
    \* Audit-2 followup #5: authorizedNodes must additionally lie in
    \* the cone of the EFFECTIVE coarse anchor (prospective if the
    \* reviewer is switching, else the current `activeCoarseNode`).
    \* The scope envelope `impact_region(activeNode)` is bidirectional
    \* and admits importers outside the down-cone -- without this
    \* check the worker could be authorized to edit out-of-cone
    \* nodes, partly defeating the anchor's scope. Dormant when
    \* `coarseDagNodes = {}` or no effective anchor: the helper
    \* returns `presentNodes` and the subset check is vacuous.
    \* (TLA does not model `protectedSemanticChangeNodes`; the kernel
    \* version of this check exempts that set per the protected-
    \* closure carve-out -- this abstract spec applies the constraint
    \* uniformly.)
    /\ LET effectiveAnchor ==
              IF response.nextActiveCoarse # NoNode
                  THEN response.nextActiveCoarse
                  ELSE activeCoarseNode
       IN  response.authorizedNodes \subseteq
              (CoarseLegalActiveSetForAnchor(effectiveAnchor)
               \cup GlobalRepairGrantedNodes(response))
    /\ response.clearHumanInput => humanInputOutstanding
    /\ \A n \in Nodes:
        response.difficultyMap[n] # "same" => n \in RequestAllowedDifficultyUpdateNodes("review")
    /\ IF response.decision = "CONTINUE" THEN
            TRUE
       ELSE
            /\ response.nextWorkerContextMode = "resume"
            /\ response.paperFocusRanges = << >>
            /\ response.workStyleHint = "none"
            /\ response.allowNewObligations = TRUE
            /\ response.mustCloseActive = FALSE
    /\ IF response.decision = "CONTINUE" /\ phase = "proof_formalization" THEN
            TRUE
       ELSE
            /\ response.allowNewObligations = TRUE
            /\ response.mustCloseActive = FALSE
    /\ IF phase = "theorem_stating" THEN
            /\ response.taskBlockers \subseteq GlobalBlockers
            /\ response.reset # NoCheckpoint
               \/ response.taskBlockers \cup response.overrideBlockers \cup response.resetBlockers = GlobalBlockers
            /\ IF invalidAttempt THEN
                    /\ response.decision \in RequestAllowedDecisions("review")
                    /\ response.nextActive = NoNode
                    /\ response.reset \in RequestAllowedResets("review")
                    /\ response.nextMode \in RequestAllowedNextModes("review")
               ELSE
                    /\ response.decision \in RequestAllowedDecisions("review")
                    /\ response.reset \in RequestAllowedResets("review")
                    /\ response.nextActive = NoNode
                       \/ response.nextActive \in RequestAllowedNextActiveNodes("review")
                    /\ response.nextMode \in RequestAllowedNextModes("review")
                    \* Kernel commit 58a1bd7: AdvancePhase ignores
                    \* next_active (the next phase's request rederives
                    \* it), so a Targeted + AdvancePhase response is
                    \* legal whether or not next_active is set. Gate
                    \* skipped on AdvancePhase; Targeted-mode
                    \* next_active requirement still applies to
                    \* Continue and NeedInput.
                    /\ (response.nextMode = "targeted" /\ response.decision # "ADVANCE_PHASE")
                       => IF RequestAllowTargetedWithoutNextActive("review") THEN
                               response.nextActive = NoNode
                          ELSE
                               response.nextActive \in RequestTargetedNextActiveNodes("review")
                    /\ IF response.decision = "ADVANCE_PHASE" THEN
                            \* AdvancePhase: GlobalBlockers = {} (clean),
                            \* clear human input, reset != LastClean.
                            \* The kernel waives the next_active
                            \* requirement on AdvancePhase in BOTH Global
                            \* and Targeted modes (commit 58a1bd7). The
                            \* downstream phase re-derives next_active
                            \* from scratch. LastClean is a rewind; the
                            \* combination AdvancePhase + LastClean is
                            \* semantically incoherent (if you are
                            \* rewinding you are not advancing), and the
                            \* kernel rejects it (`review_response_rejection_reasons`
                            \* in model.rs).
                            /\ GlobalBlockers = {}
                            /\ response.reset # "lastClean"
                            /\ ~humanInputOutstanding \/ response.clearHumanInput
                       ELSE
                            TRUE
       ELSE IF phase \in {"proof_formalization", "cleanup"} THEN
            /\ response.taskBlockers \subseteq GlobalBlockers
            /\ response.reset # NoCheckpoint
               \/ response.globalRepairRequest # NoGlobalRepairRequest
               \/ response.taskBlockers \cup response.overrideBlockers \cup response.resetBlockers = GlobalBlockers
            /\ response.reset \in RequestAllowedResets("review")
            /\ response.decision \in RequestAllowedDecisions("review")
            \* Audit-2 followup #3: when the reviewer is also switching
            \* the coarse anchor (`response.nextActiveCoarse # NoNode`),
            \* `RequestAllowedNextActiveNodes` reflects the OLD anchor's
            \* cone (via `CoarseLegalActiveSet` inside `ActiveNodeLegal`)
            \* and would reject every legal one-cycle switch where
            \* `nextActive` lands in the new anchor's cone. Additionally
            \* allow base-legal candidates in the prospective cone.
            /\ \/ response.nextActive = NoNode
               \/ response.nextActive \in RequestAllowedNextActiveNodes("review")
               \/ /\ phase = "proof_formalization"
                  /\ response.nextActiveCoarse # NoNode
                  /\ response.nextActive \in
                         (ProofActiveNodeBaseLegalCandidates
                          \cap CoarseLegalActiveSetForAnchor(response.nextActiveCoarse))
            /\ response.nextMode \in RequestAllowedNextModes("review")
            \* Cleanup-v2: the reviewer must NOT nominate `nextActive`.
            \* Dispatch is task-driven (kernel resolves the worker's
            \* active node from `cleanupAuditTasks[cleanupNextTask].
            \* target_node`). Mirror of kernel
            \* `cleanup_v2_review_fields_legal` in model.rs.
            /\ phase = "cleanup" => response.nextActive = NoNode
            \* Continue with empty nextActive is legal only for the
            \* no-focus Local retry/orphan shape (proof_formalization
            \* only — cleanup-v2 has its own derivation above and
            \* `nextMode = "local"` is illegal in cleanup). Mirror of
            \* kernel `review_response_legal` in model.rs.
            /\ /\ phase = "proof_formalization"
               /\ response.decision = "CONTINUE"
               /\ response.nextActive = NoNode
               /\ response.reset = NoCheckpoint
               => \/ response.globalRepairRequest # NoGlobalRepairRequest
                  \/ /\ activeNode = NoNode
                     /\ response.taskBlockers = {}
                     /\ response.nextMode = "local"
       ELSE
            FALSE
    \* global_repair_mode (2026-06-05) — recognise the two new
    \* optional reviewer-side fields. The legality contract is:
    \*   (a) Step A and Step C are mutually exclusive.
    \*   (b) Both are gated by the kill-switch.
    \*   (c) Both require phase = proof_formalization and
    \*       response.decision = "CONTINUE" with reset = NoCheckpoint.
    \*       Retry status is deliberately NOT a gate: when the reviewer
    \*       reaches retry after Stuck/NeedsRestructure, global repair is
    \*       the universal non-protected escape hatch for an out-of-cone
    \*       required edit.
    \*   (d) Step A is rate-limited via
    \*       `lastReviewerGlobalRepairRequestCycle`: must wait
    \*       `StuckMathAuditDispatchCooldownCycles` cycles since the
    \*       previous Step A acceptance, AND must have no
    \*       in-flight `pendingGlobalRepairRequest`.
    \*   (e) Step A proposed extension nodes must be a non-empty
    \*       subset of presentNodes. (S5 dep-neighborhood cap is
    \*       enforced kernel-side on Step B; the spec abstraction
    \*       does not model the kernel's
    \*       `live_protected_statement_node_set` so the disjointness
    \*       check is deferred to the kernel validator.)
    \*   (f) Step C requires a pending grant
    \*       (`pendingGlobalRepairGrant # NoGlobalRepairGrant`) and
    \*       rejects `nextMode = "local"` (the grant only widens
    \*       Restructure/CoarseRestructure scope).
    /\ (response.globalRepairRequest # NoGlobalRepairRequest \/
        response.consumeGlobalRepairGrant)
       => /\ globalRepairModeEnabled
          /\ phase = "proof_formalization"
          /\ response.decision = "CONTINUE"
          /\ response.reset = NoCheckpoint
          /\ ~(response.globalRepairRequest # NoGlobalRepairRequest
               /\ response.consumeGlobalRepairGrant)
    /\ response.globalRepairRequest # NoGlobalRepairRequest
       => /\ response.globalRepairRequest.proposedExtensionNodes # {}
          /\ response.globalRepairRequest.proposedExtensionNodes \subseteq presentNodes
          /\ pendingGlobalRepairRequest = NoGlobalRepairRequest
          /\ response.taskBlockers = {}
          /\ response.resetBlockers = {}
          /\ response.authorizedNodes = {}
          /\ response.nextActive = NoNode
          /\ response.nextActiveCoarse = NoNode
          /\ \/ lastReviewerGlobalRepairRequestCycle = NoCycle
             \/ cycle - lastReviewerGlobalRepairRequestCycle >=
                StuckMathAuditDispatchCooldownCycles
    /\ response.consumeGlobalRepairGrant
       => /\ pendingGlobalRepairGrant # NoGlobalRepairGrant
          /\ response.nextMode # "local"
          /\ \/ response.nextActiveCoarse = NoNode
             \/ response.nextActiveCoarse = activeCoarseNode

\* ----------------------------------------------------------------------
\* === Tier 3 invariants (reviewer contract) ===
\* ----------------------------------------------------------------------
\*
\* Reviewer-response contract checks lifted from `review_response_legal`
\* (kernel/src/model.rs:2811). The kernel enforces ~60 legality rules
\* on every reviewer response; this cluster captures the contract-level
\* properties most relevant to downstream semantics — blocker-action
\* bucket shape, the Local-mode soundness carve-out, authorized-node
\* envelopes, the DONE terminal arm, and frontier containment for
\* adjudication.
\*
\* All five invariants are gated on the relevant lifecycle state
\* (response or pendingTask). Where they touch an env-produced response,
\* they additionally gate on `ReviewDecisionLegal` (defined just above)
\* so that the only states under consideration are those that would be
\* accepted by an Apply action; an illegal env-produced response is
\* short-lived (`RejectInvalidReviewArtifact` immediately clears it on
\* the next step) and is intentionally excluded from these contract
\* assertions.
\*
\* Cross-reference: `ReviewDecisionLegal` already enforces the base
\* blocker-action bucket shape (disjointness, contract-subset
\* containment, whole-state-reset mutex); the invariants below either
\* re-document those properties as standalone contract checks or assert
\* additional properties not currently covered by `ReviewDecisionLegal`
\* (Local carve-out semantics, frontier containment, DONE-terminal
\* coupling).
\*
\* Placement: this cluster sits AFTER `ReviewDecisionLegal` rather than
\* in the original Tier-1/Tier-2 invariant cluster (just above `Init`)
\* because TLA+ tooling (SANY/TLC parser) rejects forward references
\* to top-level operators; the gated invariants below would not parse
\* if placed before `ReviewDecisionLegal`. The companion re-export in
\* `SupervisorProtocolSim.tla` and the sim-cfg listing surface these
\* invariants to TLC simulation in the standard pattern.

\* 3.1 Review blocker-actions wellformedness. Mirror of
\* `review_response_rejection_reasons` blocker-action clauses
\* (kernel/src/model.rs:2503+). A legal in-flight review response
\* satisfies:
\*   * pairwise disjoint task/override/reset buckets
\*   * task blockers drawn from the request's `blockers` set
\*   * override blockers drawn from the contract's allowedOverrideBlockers
\*     (which in turn is `RequestAllowedOverrideBlockers("review")`, the
\*     adjudicable subset of GlobalBlockers — see RequestAllowedOverrideBlockers)
\*   * reset blockers drawn from the contract's allowedResetBlockers
\*     (only non-empty in theorem-stating; see RequestAllowedResetBlockers)
\*
\* No completeness clause: the kernel's rejection logic enforces only
\* subset + pairwise disjoint, NOT `task ∪ override ∪ reset =
\* inFlightRequest.blockers`. Omitted blockers remain live and persist
\* into the next cycle under their current status. The previously-asserted
\* `(reset = NoCheckpoint) => COVER` clause was retired with the
\* partial-action blocker semantics rewrite (predecessor commit 105c151
\* and this follow-up); the contract `ReviewDecisionLegal` carries the
\* `reset # NoCheckpoint => buckets = {}` mutex rule that handles the
\* whole-state rewind arm.
\*
\* The invariant is gated on `ReviewDecisionLegal` so that intermediate
\* env-produced-but-illegal responses (which `RejectInvalidReviewArtifact`
\* will clear in one step) are excluded — those are not "in the contract"
\* in any meaningful sense. The invariant is a faithful re-statement of
\* the contract for documentation and as a regression guard.
\* Option C (2026-06-04): the `overrideBlockers \subseteq
\* allowedOverrideBlockers` clause becomes the structurally trivial
\* `overrideBlockers = {}` — the override→Pass authority is retired
\* and `allowedOverrideBlockers` is always empty.
ReviewBlockerActionsWellFormed ==
    (inFlightRequest.kind = "review"
        /\ response.kind = "review"
        /\ response.status = "ok"
        /\ ReviewDecisionLegal)
        =>
            /\ response.overrideBlockers = {}
            /\ response.taskBlockers \cap response.resetBlockers = {}
            /\ response.taskBlockers \subseteq inFlightRequest.blockers
            /\ response.resetBlockers \subseteq inFlightRequest.allowedResetBlockers

\* 3.2 Local-mode soundness carve-out (PROCESS_SEMANTICS §5.1, kernel
\* `review_response_legal` at model.rs:2850). Normally `task_blockers`
\* under Local mode are rejected — Local doesn't authorize any cross-
\* node edits, so it cannot cover most blockers. The exception:
\* Soundness on the active node. Soundness auto-clears when the active
\* node becomes sorry-free (`needs_sound = false`), and closing the
\* proof IS within Local's scope (a `.lean`-proof-body edit). So
\* `Local + must_close_active + task_blockers = [active_node_soundness_id]`
\* is legal — the worker is expected to close the proof, which
\* simultaneously clears the Soundness blocker. Non-Soundness task
\* blockers under Local remain illegal.
\*
\* Spec-side observation: the abstract spec's `ReviewScopeEnvelope`
\* returns `{}` for proof-formalization Local (the ELSE arm of
\* ReviewScopeEnvelope, after the theorem-global/cleanup/restructure
\* cases fall through), so `ReviewTaskBlockerInWorkerScope` rejects
\* every node-bound task blocker under Local — strictly stronger
\* than the kernel's soundness-on-active carve-out. This makes the
\* invariant effectively vacuous on the modeled trace (the antecedent
\* `pendingTask.taskBlockers # {}` is unreachable in Local). The
\* invariant is still meaningful as a kernel-contract guard against
\* a future spec relaxation that lets the carve-out through; the
\* consequent then enforces "must be soundness on the active node".
\*
\* The clause uses `pendingTask`, which is populated by Apply actions
\* and therefore always reflects a legality-checked response. No
\* gating on `ReviewDecisionLegal` is required.
LocalModeSoundnessCarveOut ==
    (pendingTask # NoPendingTask
        /\ pendingTask.mode = "local"
        /\ pendingTask.taskBlockers # {})
        =>
            \A b \in pendingTask.taskBlockers :
                /\ b.kind = "soundness"
                /\ b.object.otype = "node"
                /\ b.object.node = pendingTask.node

\* 3.3 Authorized-nodes scope contract. The reviewer's `authorizedNodes`
\* response field (carried through to `pendingTask.authorizedNodes`)
\* must satisfy:
\*   * non-empty only for the two restructure modes
\*   * subset of presentNodes
\* — mirror of `ReviewDecisionLegal`'s proof-formalization authorized-
\* nodes block plus the kernel rule at model.rs that requires
\* Restructure/CoarseRestructure to nominate an explicit edit envelope
\* and Local/Global/Targeted/Cleanup to leave it empty.
\*
\* NOTE — the converse "restructure modes IMPLY authorizedNodes # {}"
\* clause from the prompt's pseudocode is dropped here. With
\* `response.reset # NoCheckpoint`, ReviewDecisionLegal's
\* proof-formalization authorized-nodes clause has the form
\* `(reset = NoCheckpoint /\ decision = "CONTINUE") => ...`, which
\* turns OFF the non-empty requirement for restructure modes —
\* response.authorizedNodes # {} is only forced under the no-reset
\* Continue arm. The combination "reset != NoCheckpoint + Continue +
\* restructure mode" leaves pendingTask.mode in {restructure,
\* coarse_restructure} with possibly-empty pendingTask.authorizedNodes,
\* which a strict `<=>` would flag as a violation despite being a
\* legal kernel state. The reverse direction (authorizedNodes # {}
\* implies a restructure mode) holds without exception and is the
\* useful structural guard.
\*
\* `pendingTask.authorizedNodes \subseteq presentNodes` is also in
\* PendingTaskConsistent; kept here for self-contained contract reading.
AuthorizedNodesScopeContract ==
    pendingTask # NoPendingTask =>
        /\ pendingTask.mode \in {"local", "global", "targeted", "cleanup"}
            => pendingTask.authorizedNodes = {}
        /\ pendingTask.authorizedNodes # {}
            => pendingTask.mode \in {"restructure", "coarse_restructure"}
        /\ pendingTask.authorizedNodes \subseteq presentNodes

\* 3.4 Reviewer scope-authorization completeness. In proof formalization,
\* every present, non-protected node must remain reachable through some
\* reviewer authorization path. The detailed spec does not model the live
\* paper-protected statement set, so this invariant states the non-
\* protected escape-hatch part: a pure global-repair Step A request for
\* any non-empty subset of presentNodes is legal whenever the global-repair
\* lifecycle gates are clear. There is intentionally no
\* `retryOutcomeKind = "none"` antecedent; retry status may affect
\* accounting and anchor switching, but must not make global repair
\* unavailable.
GlobalRepairStepARequestShape ==
    /\ phase = "proof_formalization"
    /\ stage = "Reviewer"
    /\ response.kind = "review"
    /\ response.status = "ok"
    /\ response.cycle = cycle
    /\ globalRepairModeEnabled
    /\ response.decision = "CONTINUE"
    /\ response.reset = NoCheckpoint
    /\ IF response.globalRepairRequest = NoGlobalRepairRequest THEN
            FALSE
       ELSE
            /\ response.globalRepairRequest.proposedExtensionNodes # {}
            /\ response.globalRepairRequest.proposedExtensionNodes \subseteq presentNodes
    /\ ~response.consumeGlobalRepairGrant
    /\ pendingGlobalRepairRequest = NoGlobalRepairRequest
    /\ response.taskBlockers = {}
    /\ response.overrideBlockers = {}
    /\ response.resetBlockers = {}
    /\ response.authorizedNodes = {}
    /\ response.nextActive = NoNode
    /\ response.nextActiveCoarse = NoNode
    /\ response.nextMode \in RequestAllowedNextModes("review")
    /\ response.clearHumanInput = FALSE
    /\ \A n \in Nodes : response.difficultyMap[n] = "same"
    /\ \/ lastReviewerGlobalRepairRequestCycle = NoCycle
       \/ cycle - lastReviewerGlobalRepairRequestCycle >=
          StuckMathAuditDispatchCooldownCycles

ReviewerScopeAuthorizationComplete ==
    GlobalRepairStepARequestShape => ReviewDecisionLegal

\* 3.5 Cleanup DONE is a terminal-shape commitment. A legal review
\* response with `decision = "DONE"` corresponds to the cleanup-phase
\* terminal arm (PROCESS_SEMANTICS §4.4 "Done" bullet): the only place
\* DONE is allowed is cleanup (`RequestAllowedDecisions` — cleanup
\* admits {CONTINUE, NEED_INPUT, DONE}, the other phases omit DONE),
\* and the blocker set must be empty by `CleanupHasNoBlockers` plus
\* the contract's GlobalBlockers gate on the reviewer's allowed-bucket
\* choices.
\*
\* Coupling worth calling out: even when the reviewer pairs DONE with
\* `reset = lastCommit` (the cleanup-Done arm's only reset option;
\* `LastClean` is rejected for cleanup -- see ReviewResetChoices's
\* `currentPhase /= "cleanup"` guard on the LastClean disjunct),
\* the partition stays empty because the bucket sources
\* (`RequestAllowedOverrideBlockers`, `RequestAllowedResetBlockers`,
\* and the per-phase `response.taskBlockers \subseteq GlobalBlockers`
\* clause) all bottom out at `GlobalBlockers = {}` in cleanup.
\*
\* `inFlightRequest.blockers = {}` follows from the same reasoning:
\* `RequestBlockers("review") = GlobalBlockers` and `CleanupHasNoBlockers`
\* forces that set to be empty whenever `phase = "cleanup"`.
\*
\* The phase = "cleanup" consequent is enforced by `RequestAllowedDecisions`
\* (DONE absent outside cleanup); writing it as a consequent makes the
\* invariant self-explaining as a "DONE implies cleanup-terminal" form.
CleanupDoneTerminal ==
    (response.kind = "review"
        /\ response.status = "ok"
        /\ response.decision = "DONE"
        /\ ReviewDecisionLegal)
        =>
            /\ phase = "cleanup"
            /\ inFlightRequest.blockers = {}
            /\ response.taskBlockers = {}
            \* Option C (2026-06-04): overrideBlockers always empty
            \* across legal review responses; ReviewDecisionLegal
            \* enforces `overrideBlockers = {}` independently of
            \* phase/decision.
            /\ response.resetBlockers = {}

\* 3.6 Adjudication-frontier containment (PROCESS_SEMANTICS §4.4 guard
\* 1, kernel `apply_review_blocker_adjudications` in model.rs). The
\* reviewer can only adjudicate blockers whose carrier (node or target)
\* is in the relevant verifier panel's `latest_*_review_*` set —
\* "what the panel just voted on", not arbitrary leftover blockers
\* from earlier cycles.
\*
\* Option C (2026-06-04): the override→Pass authority is retired, so
\* `overrideBlockers` is always empty under `ReviewDecisionLegal` and
\* the universal collapses to `taskBlockers` only. The task path is
\* NOT separately gated by frontier containment in
\* `ReviewDecisionLegal` (only by scope coverage), so this invariant
\* is the explicit task-path enforcement.
\*
\* Blocker kinds enumerated from `BlockerUniverse`: `node_corr`,
\* `substantiveness`, `soundness` (node-bound) and `paper_faithfulness`
\* (target-bound). Object discriminants come from `NodeObject` /
\* `TargetObject`: `object.otype` is `"node"` (with `object.node`)
\* or `"target"` (with `object.target`).
\*
\* Gated by `ReviewDecisionLegal` to skip env-produced illegal
\* responses; if the spec lacks a constraint that ought to forbid
\* a task-blocker outside the frontier, this invariant will surface
\* it on the simulation traces.
AdjudicationFrontierContained ==
    (response.kind = "review"
        /\ response.status = "ok"
        /\ ReviewDecisionLegal)
        =>
            \A b \in response.taskBlockers :
                /\ b.kind = "node_corr"
                    => b.object.node \in latestCorrReviewNodes
                /\ b.kind = "soundness"
                    => b.object.node \in latestSoundReviewNodes
                /\ b.kind = "substantiveness"
                    => b.object.node \in latestSubstantivenessReviewNodes
                /\ b.kind = "paper_faithfulness"
                    => b.object.target \in latestPaperReviewTargets
                \* Deviation: extend to mirror kernel
                \* `review_blocker_adjudicable` Deviation arm.
                /\ b.kind = "deviation"
                    => b.object.deviation \in latestDeviationReviewIds

\* Deviation lane invariants
\* ----------------------------------------------------------------------

\* 1. Sticky-Fail discipline: a Fail entry must have its approvedFp
\*    pinned to the (non-empty) currentFp; if the currentFp drifts off
\*    or empties, `CurrentDeviationState` must read Unknown rather
\*    than Fail. This is the spec-side reverse of efaafa7's
\*    `current_deviation_state` extension (model.rs:6582-6594).
DeviationStickyFailDiscipline ==
    \A id \in Deviations :
        (deviationFiles[id] /\ CurrentDeviationFail(id))
            => /\ deviationStatus[id] = "fail"
               /\ deviationCurrentFp[id] # NoFingerprint
               /\ deviationCurrentFp[id] = deviationApprovedFp[id]

\* 2. Substantiveness short-circuit: when a node has an unauthorized
\*    deviation claim, its substantiveness state must be Unknown —
\*    even if the underlying `substantivenessStatus[n]` says Pass.
\*    Mirror of kernel `current_substantiveness_state` early-return
\*    at model.rs:6736-6738.
DeviationUnauthorizedClaimSuppressesSubstantivenessPass ==
    \A n \in presentNodes :
        NodeHasUnauthorizedDeviationClaim(n)
            => CurrentSubstantivenessState(n) = "unknown"

\* 3. Claim carrier hygiene: every claim names a live deviation, and
\*    only present nodes hold claims. Mirror of kernel
\*    `normalize_live_structural_state` (model.rs:5316-5320) and the
\*    audit fix at 4e83783 that prunes claims for retired ids. This
\*    is partly enforced by TypeOK already (we duplicate so any drift
\*    surfaces as an invariant violation rather than a typecheck
\*    failure at the start of a simulation).
DeviationClaimsCarrierWellFormed ==
    /\ \A n \in Nodes :
        /\ nodeDeviationClaims[n] \subseteq {id \in Deviations : deviationFiles[id]}
        /\ (n \notin presentNodes) => nodeDeviationClaims[n] = {}
    /\ \A n \in Nodes :
        /\ committedNodeDeviationClaims[n] \subseteq
                {id \in Deviations : committedDeviationFiles[id]}
        /\ (n \notin committedPresentNodes)
              => committedNodeDeviationClaims[n] = {}

\* 4. Option C (2026-06-04): `ReviewerOverrideEmptyUnderDefault` is
\*    retired. The override→Pass authority is removed entirely; the
\*    structural empty-set property is now a direct consequence of
\*    `RequestAllowedOverrideBlockers ≡ {}` and is enforced by the
\*    constructive `response.overrideBlockers = {}` clause in
\*    `ReviewDecisionLegal` / `ReviewBlockerActionsWellFormed`. The
\*    invariant is kept as a TRUE-defined operator so SP!-style
\*    references in `SupervisorProtocolSim.tla` continue to compile.
\*    See REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md.
ReviewerOverrideEmptyUnderDefault == TRUE

\* 5. Deviation-blocker worker-scope routing: under the default
\*    policy the reviewer can task a Deviation blocker only to the
\*    phase/mode combos enumerated by
\*    `review_task_blocker_in_worker_scope` (model.rs:2947-2956). Any
\*    Deviation in `response.taskBlockers` outside that envelope is
\*    illegal. Cleanup / Complete are out of scope — a Failed
\*    deviation in Cleanup is a stuck state at the protocol level
\*    and must be resolved before phase entry.
DeviationTaskBlockerRoutingScope ==
    (response.kind = "review"
        /\ response.status = "ok"
        /\ ReviewDecisionLegal)
        => \A b \in response.taskBlockers :
            b.kind = "deviation"
                => \/ /\ phase = "theorem_stating"
                      /\ response.nextMode = "global"
                   \/ /\ phase = "proof_formalization"
                      /\ response.nextMode \in
                           {"easy", "local", "restructure", "coarse_restructure"}

\* 6. Deviation-deletion contract (kernel
\*    `deviation_deletion_contract_errors`,
\*    worker_normalization.rs:689-735). The spec's
\*    `WorkerRetireDeviation` already enforces that no node still
\*    claims a to-delete id (it eagerly empties claims as part of the
\*    same step). This invariant cross-checks: a retired id can have
\*    no surviving claim anywhere in the post-state.
DeviationDeletionLeavesNoClaim ==
    \A id \in Deviations :
        (~deviationFiles[id])
            => \A n \in Nodes : id \notin nodeDeviationClaims[n]

StartCycle ==
    /\ phase \in {"theorem_stating", "proof_formalization", "cleanup"}
    /\ stage = "Start"
    /\ cycle < MaxCycle
    /\ cycle' = cycle + 1
    /\ attempt' = 1
    \* Cycle-start dispatch mirrors `theorem_start_request_kind` /
    \* `proof_start_request_kind` in the kernel. Priority order is
    \* paper-target -> substantiveness -> corr -> sound -> worker.
    \* Both paper-target and substantiveness map to "VerifyPaper" because
    \* they share `RequestKind::Paper`; the engine drains target-first
    \* then per-node within the same stage. ProofFormalization uses the
    \* same priority order; only paper-target is theorem-only in practice
    \* (proof-phase configured_targets are typically all Pass), but the
    \* check is symmetric so the spec mirrors the kernel exactly.
    \*
    \* Task-blocker preemption: when the reviewer left a pending task
    \* with non-empty taskBlockers, schedule the worker first even if
    \* verifier lanes are non-empty. Otherwise a co-occurring
    \* resetBlockerIds (which marks lanes Unknown) would route to a
    \* verifier first, the kernel would clear pendingTask on issue, and
    \* the worker assignment would silently disappear. Mirrors kernel
    \* model.rs:3088-3092 (theorem_start_request_kind) and
    \* model.rs:3130-3134 (proof_start_request_kind).
    \* Cone-clean (kernel engine.rs `start_cycle`): when the prior step
    \* was the audit-authorized cone-clean reset,
    \* `forceReviewAfterConeClean` short-circuits the usual
    \* verifier/worker priority ladder and routes straight to the
    \* Reviewer. The kernel additionally clears
    \* `force_stuck_math_audit_after_rewind` and pending state on this
    \* path; those mutations are abstracted (StuckMathAudit role is
    \* unmodeled, so the spec never reaches a state with the flag set,
    \* but the consumer's routing is mirrored for future syncs).
    \*
    \* Cleanup-v2 routing (kernel engine.rs `start_cycle`): in Phase
    \* Cleanup with no active cleanup task (cleanupActiveTask = NoTask
    \* AND pendingTask = NoPendingTask), the kernel emits an Audit
    \* request when `cleanupAuditBurstCount = 0` (round-entry first
    \* burst), and otherwise routes to Reviewer (defense-in-depth path
    \* for state-load / recovery; legitimate flow re-issues Audit from
    \* `apply_audit_response` or drives Worker via `apply_cleanup_review_response`).
    /\ stage' =
        \* Post-advance routing latch (kernel engine.rs `start_cycle`):
        \* the first cycle after a human-approved phase advance
        \* dispatches a routing Reviewer so the reviewer can pick
        \* `next_active`, `next_active_coarse`, etc. Highest priority —
        \* before the cone-clean force-review, before orphan cleanup,
        \* before the verifier/worker priority ladder.
        IF phase = "proof_formalization" /\ postAdvanceRoutingPending THEN
            "Reviewer"
        ELSE IF phase = "proof_formalization" /\ forceReviewAfterConeClean THEN
            "Reviewer"
        ELSE IF OrphanCleanupNeeded THEN
            "Worker"
        ELSE IF pendingTask.taskBlockers # {} THEN
            "Worker"
        ELSE IF phase = "theorem_stating" THEN
            IF PaperVerifyTargets # {} \/ SubstantivenessVerifyNodes # {} THEN
                "VerifyPaper"
            ELSE IF CorrVerifyNodes # {}
            THEN
                "VerifyCorr"
            ELSE IF SoundVerifyNodes # {} THEN
                "VerifySound"
            ELSE
                "Worker"
        ELSE IF phase = "proof_formalization" THEN
            IF PaperVerifyTargets # {} \/ SubstantivenessVerifyNodes # {} THEN
                "VerifyPaper"
            ELSE IF CorrVerifyNodes # {} THEN
                "VerifyCorr"
            ELSE IF SoundVerifyNodes # {} THEN
                "VerifySound"
            ELSE
                "Worker"
        ELSE IF phase = "cleanup" THEN
            \* Cleanup-v2: active worker task -> Worker; first audit
            \* burst per round (no active task, burst_count = 0) ->
            \* CleanupAudit; otherwise fall through to Reviewer.
            IF cleanupActiveTask # NoTask \/ pendingTask # NoPendingTask THEN
                "Worker"
            ELSE IF cleanupAuditBurstCount = 0 THEN
                "CleanupAudit"
            ELSE
                "Reviewer"
        ELSE
            "Worker"
    \* `postAdvanceRoutingPending` is NOT cleared here — it stays set
    \* throughout the routing Review's lifetime (re-issues on
    \* malformed responses must derive `post_advance_routing: true`
    \* consistently). Cleared in `apply_proof_review_response`
    \* (mirrored in `ReviewContinueProof` / `ReviewNeedInputProof` and
    \* any reset arm that routes through the proof-review handler).
    /\ postAdvanceRoutingPending' = postAdvanceRoutingPending
    /\ forceReviewAfterConeClean' =
        IF phase = "proof_formalization" /\ forceReviewAfterConeClean THEN
            FALSE
        ELSE
            forceReviewAfterConeClean
    /\ invalidAttempt' = FALSE
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
    /\ latestCorrEvidenceLanes' = latestCorrEvidenceLanes
    /\ latestSoundEvidenceLanes' = latestSoundEvidenceLanes
    /\ latestPaperReviewTargets' = latestPaperReviewTargets
    /\ latestCorrReviewNodes' = latestCorrReviewNodes
    /\ latestSoundReviewNodes' = latestSoundReviewNodes
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = latestCorrPanelSplit
    /\ latestSoundPanelSplit' = latestSoundPanelSplit
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = {}
    /\ latestSubstantivenessReviewNodes' = {}
    /\ latestSubstantivenessPanelSplit' = FALSE
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ humanInputOutstanding' = humanInputOutstanding
    /\ reviewerComments' =
        IF OrphanCleanupNeeded THEN "set" ELSE reviewerComments
    /\ heldTarget' =
        IF phase = "theorem_stating" THEN
            SelectedTheoremHeldTarget
        ELSE
            heldTarget
    /\ activeNode' =
        IF OrphanCleanupNeeded /\ phase = "proof_formalization" THEN
            NoNode
        ELSE
            activeNode
    /\ targetEditMode' =
        IF OrphanCleanupNeeded THEN
            "global"
        ELSE
            targetEditMode
    /\ proofEditMode' =
        IF OrphanCleanupNeeded /\ phase = "proof_formalization" THEN
            "coarse_restructure"
        ELSE
            proofEditMode
    /\ UNCHANGED
        <<
            phase,
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
            nodeDifficulty,
            easyAttempts,
            nativeHistoryKinds,
            cyclesSinceClean,
            hasEverBeenClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED LocalClosureVars
    /\ pendingTask' =
        IF stage' = "Worker" THEN
            IF OrphanCleanupNeeded THEN
                OrphanCleanupPendingTask(activeNode', CurrentMode', currentCoverage, presentNodes)
            ELSE
                pendingTask
        ELSE
            NoPendingTask
    /\ ClearArtifacts

EnvEditConfiguredTargets ==
    /\ phase = "theorem_stating"
    /\ stage = "Start"
    /\ \E newTargets \in TargetSetNeighbors(configuredTargets):
        /\ LET newGlobal ==
                NodeCorrBlockersFor(corrStatus, corrCurrentFp, corrApprovedFp, presentNodes)
                \cup
                PaperBlockersFor(paperStatus, paperCurrentFp, paperApprovedFp, newTargets)
                \cup
                SoundBlockersFor(soundStatus, soundCurrentFp, soundApprovedFp, presentNodes, openNodes)
           IN
                /\ configuredTargets' = newTargets
                /\ currentCoverage' = CoverageFromClaims(currentTargetClaims, presentNodes, newTargets)
                /\ committedCoverage' = CoverageFromClaims(committedTargetClaims, committedPresentNodes, newTargets)
                /\ activeNode' = NoNode
                /\ heldTarget' = NoNode
                /\ targetEditMode' = "global"
                /\ pendingTask' = NoPendingTask
                /\ UNCHANGED
                    <<
                        phase,
                        stage,
                        cycle,
                        attempt,
                        invalidAttempt,
                        gateKind,
                        gateFromInvalidAttempt,
                        proofEditMode,
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
                        nodeDifficulty,
                        easyAttempts,
                        humanInputOutstanding,
                        nativeHistoryKinds,
                        requestSeq,
                        inFlightRequest,
                        response,
                        cyclesSinceClean,
                        hasEverBeenClean,
                        forceReviewAfterConeClean
                    >>
                /\ UNCHANGED CleanupV2Vars
                /\ UNCHANGED CoarseAnchorVars
                /\ UNCHANGED StuckMathAuditVars
                /\ UNCHANGED GlobalRepairVars
                /\ UNCHANGED PostAdvanceRoutingVars
                /\ UNCHANGED ProtectedReapprovalVars
                /\ UNCHANGED AuditPlanVars
                /\ UNCHANGED SoundAssessmentVars
                /\ UNCHANGED DeviationVars
                /\ UNCHANGED PromptCarryVars
                /\ UNCHANGED LocalClosureVars

\* Audit H-2 — environmental rescission of approved-axiom policy.
\* Operator edits `APPROVED_AXIOMS.json` so the per-record
\* `approved_axioms_hash` no longer matches; the runtime CLI's
\* per-step hook (`rescind_records_with_stale_approved_axioms_hash`
\* in kernel/src/bin/runtime_cli.rs) demotes the affected record
\* to `unverified`. Modeled here as a non-deterministic env mutation:
\* a single sorry-free present proof_node is picked and moved into
\* `localClosureUnverified`; downstream `formalization_complete`
\* checks then block until a fresh probe re-verifies.
\*
\* The action is not gated on stage or in-flight request because
\* the kernel's hook runs at every `step_runtime` call; the spec
\* models the environmental change rather than the in-flight
\* protection.
EnvRescindApprovedAxiom ==
    /\ \E n \in (presentNodes \cap currentProofNodes) \ openNodes :
        /\ n \notin localClosureUnverified
        /\ localClosureUnverified' = localClosureUnverified \cup {n}
    /\ UNCHANGED committedLocalClosureUnverified
    /\ UNCHANGED VarsWithoutRequest
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED <<inFlightRequest, response, requestSeq>>

IssueWorkerRequest ==
    /\ stage = "Worker"
    /\ inFlightRequest = NoRequest
    /\ response = NoResponse
    /\ RequestSupportReady("worker")
    /\ inFlightRequest' = RequestRecord(requestSeq + 1, "worker")
    /\ requestSeq' = requestSeq + 1
    /\ UNCHANGED VarsWithoutRequest
    /\ UNCHANGED DeviationVars

IssuePaperRequest ==
    /\ stage = "VerifyPaper"
    /\ inFlightRequest = NoRequest
    /\ response = NoResponse
    /\ RequestSupportReady("paper")
    /\ inFlightRequest' = RequestRecord(requestSeq + 1, "paper")
    /\ requestSeq' = requestSeq + 1
    /\ UNCHANGED VarsWithoutRequest
    /\ UNCHANGED DeviationVars

IssueCorrRequest ==
    /\ stage = "VerifyCorr"
    /\ inFlightRequest = NoRequest
    /\ response = NoResponse
    /\ RequestSupportReady("corr")
    /\ inFlightRequest' = RequestRecord(requestSeq + 1, "corr")
    /\ requestSeq' = requestSeq + 1
    /\ UNCHANGED VarsWithoutRequest
    /\ UNCHANGED DeviationVars

IssueSoundRequest ==
    /\ stage = "VerifySound"
    /\ inFlightRequest = NoRequest
    /\ response = NoResponse
    /\ RequestSupportReady("sound")
    /\ inFlightRequest' = RequestRecord(requestSeq + 1, "sound")
    /\ requestSeq' = requestSeq + 1
    /\ UNCHANGED VarsWithoutRequest
    /\ UNCHANGED DeviationVars

IssueReviewRequest ==
    /\ stage = "Reviewer"
    /\ inFlightRequest = NoRequest
    /\ response = NoResponse
    /\ RequestSupportReady("review")
    /\ inFlightRequest' = RequestRecord(requestSeq + 1, "review")
    /\ requestSeq' = requestSeq + 1
    /\ UNCHANGED VarsWithoutRequest
    /\ UNCHANGED DeviationVars

IssueHumanGateRequest ==
    /\ stage = "HumanGate"
    /\ inFlightRequest = NoRequest
    /\ response = NoResponse
    /\ inFlightRequest' = RequestRecord(requestSeq + 1, "human_gate")
    /\ requestSeq' = requestSeq + 1
    /\ UNCHANGED VarsWithoutRequest
    /\ UNCHANGED DeviationVars

\* Protected-target reapproval producer (kernel
\* `maybe_issue_protected_reapproval` in engine.rs). Fires when
\* `pendingProtectedReapprovalNodes # {}` in ProofFormalization and the
\* run is not blocked by a global blocker, an outstanding human-input
\* gate, an in-flight retry, or an orphan-cleanup obligation. Transitions
\* stage to HumanGate with gateKind = "protected_reapproval"; clears the
\* pending task, retry context, and the semantic-scope-confirmation
\* carrier; relegalizes active fields (modeled as preserving
\* `activeNode` / `heldTarget` because the spec's ActiveNodeLegal
\* clause in TypeOK already enforces the post-state legality).
MaybeIssueProtectedReapproval ==
    /\ phase = "proof_formalization"
    /\ stage \in {"Start", "Worker", "Reviewer", "VerifyPaper", "VerifyCorr", "VerifySound"}
    /\ inFlightRequest = NoRequest
    /\ response = NoResponse
    /\ pendingProtectedReapprovalNodes # {}
    /\ GlobalBlockers = {}
    /\ ~humanInputOutstanding
    /\ retryOutcomeKind = "none"
    /\ ~OrphanCleanupNeeded
    /\ stage' = "HumanGate"
    /\ gateKind' = "protected_reapproval"
    /\ gateFromInvalidAttempt' = FALSE
    \* Kernel `maybe_issue_protected_reapproval` clears the semantic
    \* scope confirmation carrier on issue; the reapproval gate
    \* speaks for the whole pending node set, not for any single
    \* reissued semantic-scope confirmation that was queued earlier.
    /\ pendingProtectedSemanticScopeConfirmation' = NoProtectedSemanticChangeConfirmation
    /\ pendingProtectedReapprovalNodes' = pendingProtectedReapprovalNodes
    /\ pendingTask' = NoPendingTask
    /\ heldTarget' = NoNode
    /\ activeNode' =
        IF ActiveNodeLegal(phase, activeNode, presentNodes, openNodes) THEN
            activeNode
        ELSE
            NoNode
    /\ targetEditMode' = "global"
    /\ proofEditMode' = IF activeNode' = NoNode THEN "local" ELSE proofEditMode
    /\ UNCHANGED requestSeq
    /\ UNCHANGED inFlightRequest
    /\ UNCHANGED response
    /\ UNCHANGED <<
        phase,
        cycle,
        attempt,
        invalidAttempt,
        retryOutcomeKind,
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
        nodeDifficulty,
        easyAttempts,
        reviewerComments,
        latestPaperEvidenceLanes,
        latestCorrEvidenceLanes,
        latestSoundEvidenceLanes,
        latestPaperReviewTargets,
        latestCorrReviewNodes,
        latestSoundReviewNodes,
        latestPaperPanelSplit,
        latestCorrPanelSplit,
        latestSoundPanelSplit,
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
        forceReviewAfterConeClean
       >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars

\* Protected-target reapproval consumer — approve arm. Mirrors kernel
\* `apply_human_gate_response` GateKind::ProtectedReapproval::Approve
\* in engine.rs. On approve: freeze approved-target snapshot (mirror
\* of `freeze_approved_target_snapshot_from_live`), clear both pending
\* protected fields, clear retry / held-target / pending-task / human
\* input, route into ProofFormalization (or Cleanup when
\* formalization_complete + GlobalBlockers = {}). Emits a commit
\* checkpoint conceptually; spec abstracts to a commit_live mirror.
HumanApproveProtectedReapproval ==
    /\ stage = "HumanGate"
    /\ gateKind = "protected_reapproval"
    /\ response.status = "ok"
    /\ response.kind = "human_gate"
    /\ response.humanChoice = "approve"
    /\ approvedConfiguredTargets' = configuredTargets
    /\ approvedCoverage' = currentCoverage
    /\ paperApprovedFp' = paperCurrentFp
    /\ approvedTargetFp' = currentTargetFp
    /\ pendingProtectedReapprovalNodes' = {}
    /\ pendingProtectedSemanticScopeConfirmation' = NoProtectedSemanticChangeConfirmation
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ pendingTask' = NoPendingTask
    /\ invalidAttempt' = FALSE
    /\ heldTarget' = NoNode
    /\ targetEditMode' = "global"
    /\ proofEditMode' = "local"
    /\ humanInputOutstanding' = FALSE
    /\ attempt' = 0
    \* Cleanup-v2: enter_cleanup_phase triggers when formalization is
    \* complete AND there are no global blockers. Otherwise stay in
    \* proof_formalization at Stage::Start.
    /\ IF FormalizationComplete /\ GlobalBlockers = {} THEN
            /\ phase' = "cleanup"
            /\ stage' = "Start"
            \* enter_cleanup_phase resets cleanup-v2 fields; mirror that.
            /\ cleanupAuditTasks' = <<>>
            /\ cleanupAuditScratchpad' = ""
            /\ cleanupAuditBurstCount' = 0
            /\ cleanupAuditRound' = 1
            /\ cleanupConsecutiveInvalidWorkers' = 0
            /\ cleanupActiveTask' = NoTask
            /\ cleanupForceDone' = FALSE
       ELSE
            /\ phase' = "proof_formalization"
            /\ stage' = "Start"
            /\ UNCHANGED CleanupV2Vars
    /\ activeNode' = activeNode
    /\ CommitCurrentWorktree
    /\ UNCHANGED PromptCarryVars
    /\ UNCHANGED <<
        cycle,
        configuredTargets,
        presentNodes,
        openNodes,
        currentCoverage,
        currentTargetFp,
        paperStatus,
        paperCurrentFp,
        corrStatus,
        corrCurrentFp,
        corrApprovedFp,
        soundStatus,
        soundCurrentFp,
        soundApprovedFp,
        substantivenessStatus,
        substantivenessCurrentFp,
        substantivenessApprovedFp,
        coarseDagNodes,
        nodeDifficulty,
        easyAttempts,
        cyclesSinceClean,
        hasEverBeenClean,
        forceReviewAfterConeClean,
        retryOutcomeKind
       >>
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED AllStructureVars
    /\ UNCHANGED LocalClosureVars
    /\ ClearArtifactsAndRecordHistory

\* Protected-target reapproval consumer — feedback arm. Mirrors kernel
\* `apply_human_gate_response` GateKind::ProtectedReapproval::Feedback
\* in engine.rs. On feedback: keep the pending reapproval node set
\* (the gate stays unresolved until a future Approve fires), clear
\* the semantic scope confirmation carrier, route to Reviewer with
\* humanInputOutstanding = TRUE, and re-issue a Review request.
HumanFeedbackProtectedReapproval ==
    /\ stage = "HumanGate"
    /\ gateKind = "protected_reapproval"
    /\ response.status = "ok"
    /\ response.kind = "human_gate"
    /\ response.humanChoice = "feedback"
    /\ phase' = "proof_formalization"
    /\ stage' = "Reviewer"
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ invalidAttempt' = FALSE
    /\ humanInputOutstanding' = TRUE
    /\ pendingTask' = NoPendingTask
    /\ pendingProtectedReapprovalNodes' = pendingProtectedReapprovalNodes
    /\ pendingProtectedSemanticScopeConfirmation' = NoProtectedSemanticChangeConfirmation
    /\ UNCHANGED PromptCarryVars
    /\ UNCHANGED <<
        cycle,
        attempt,
        retryOutcomeKind,
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
        nodeDifficulty,
        easyAttempts,
        reviewerComments,
        latestPaperEvidenceLanes,
        latestCorrEvidenceLanes,
        latestSoundEvidenceLanes,
        latestPaperReviewTargets,
        latestCorrReviewNodes,
        latestSoundReviewNodes,
        latestPaperPanelSplit,
        latestCorrPanelSplit,
        latestSoundPanelSplit,
        previousPaperFindingLanes,
        previousCorrFindingLanes,
        previousSoundFindingLanes,
        latestSubstantivenessEvidenceLanes,
        latestSubstantivenessReviewNodes,
        latestSubstantivenessPanelSplit,
        previousSubstantivenessFindingLanes,
        nativeHistoryKinds,
        cyclesSinceClean,
        hasEverBeenClean,
        forceReviewAfterConeClean
       >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED AllStructureVars
    /\ UNCHANGED LocalClosureVars
    /\ ClearArtifactsAndRecordHistory

\* ----------------------------------------------------------------------
\* Cleanup-v2 audit lane actions (plan §16/§3, 2026-05-14)
\* ----------------------------------------------------------------------
\*
\* The audit is its own request kind (`audit`) and stage
\* (`CleanupAudit`). Three actions:
\*   - IssueCleanupAuditRequest: round entry / continuation; emits an
\*     audit-kind in-flight request.
\*   - AcceptCleanupAuditNeedToContinue: append-and-re-issue path.
\*   - AcceptCleanupAuditDone: append-and-transition-to-reviewer path.
\*
\* And two reviewer Cleanup-phase variants that mutate cleanup-v2 state:
\*   - ReviewerCleanupDismissAndDispatch: Continue with cleanup-v2 dispatch.
\*   - ReviewerCleanupReAudit: Done that requests another audit round.
\*
\* These are added in stub form: the actions exist in the action family
\* and are enumerated in CoreNext, but their detailed expansions follow
\* the structural patterns of the existing IssuePaperRequest /
\* ReviewContinueCleanup / ReviewDoneCleanup actions. The body is the
\* minimum needed for TLC to validate the cleanup-v2 variables and
\* UNCHANGED accounting.

IssueCleanupAuditRequest ==
    /\ phase = "cleanup"
    /\ stage = "CleanupAudit"
    /\ inFlightRequest = NoRequest
    /\ response = NoResponse
    /\ cleanupAuditBurstCount < 5  \* CLEANUP_AUDIT_MAX_BURSTS_PER_ROUND
    /\ inFlightRequest' = RequestRecord(requestSeq + 1, "audit")
    /\ requestSeq' = requestSeq + 1
    /\ UNCHANGED VarsWithoutRequest
    /\ UNCHANGED DeviationVars

\* StuckMathAudit request issuance (2026-05-31). Stub mirror of
\* kernel `issue_request(state, RequestKind::StuckMathAudit)`.
\* Fired from `route_need_input_to_auditor` (the four ReviewNeedInput*
\* actions set `stage = "StuckMathAudit"` and clear in-flight; this
\* action then publishes the request) and from the TheoremStating
\* Sound-stagnation preempt in `start_cycle`. Also fired on retry by
\* `retry_or_transition_stuck_math_audit_to_reviewer` when the burst
\* retry counter is still under STUCK_MATH_AUDIT_BURST_RETRY_LIMIT (1).
IssueStuckMathAuditRequest ==
    /\ stage = "StuckMathAudit"
    /\ inFlightRequest = NoRequest
    /\ response = NoResponse
    /\ stuckMathAuditActive
    /\ inFlightRequest' = RequestRecord(requestSeq + 1, "stuck_math_audit")
    /\ requestSeq' = requestSeq + 1
    /\ UNCHANGED VarsWithoutRequest
    /\ UNCHANGED DeviationVars

\* Accept an audit response with outcome = "need_to_continue". The
\* burst count increments; the task list and scratchpad may have been
\* updated by the burst (modeled here abstractly as "the kernel mutated
\* cleanupAuditTasks and cleanupAuditScratchpad"). The next state stays
\* at stage = "CleanupAudit" so the next IssueCleanupAuditRequest can
\* fire if burst_count < cap.
\*
\* Cap-hit guard (kernel `apply_audit_response`, engine.rs): the kernel
\* always increments `cleanup_audit_burst_count` on Valid response, then
\* tests `continue_audit = (outcome == NeedToContinue) && (count <
\* MAX_BURSTS_PER_ROUND)`. If the post-increment count reaches MAX, the
\* kernel routes to the reviewer via `transition_audit_to_reviewer`
\* regardless of `outcome`. Spec mirrors this by gating on
\* `cleanupAuditBurstCount + 1 < 5` here, leaving the cap-hit case to
\* `AcceptCleanupAuditDone`.
AcceptCleanupAuditNeedToContinue ==
    /\ stage = "CleanupAudit"
    /\ inFlightRequest # NoRequest
    /\ inFlightRequest.kind = "audit"
    /\ inFlightRequest.cycle = cycle
    /\ response # NoResponse
    /\ cleanupAuditBurstCount + 1 < 5  \* post-increment stays under cap
    /\ cleanupAuditBurstCount' = cleanupAuditBurstCount + 1
    /\ inFlightRequest' = NoRequest
    /\ response' = NoResponse
    /\ UNCHANGED VarsExceptCleanupV2AndRequest
    /\ UNCHANGED requestSeq
    /\ UNCHANGED <<
        cleanupAuditTasks,
        cleanupAuditScratchpad,
        cleanupAuditRound,
        cleanupConsecutiveInvalidWorkers,
        cleanupActiveTask,
        cleanupForceDone
       >>
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars

\* Accept an audit response with outcome = "audit_done" (or the burst
\* cap was hit). Transitions to stage = "Reviewer".
\*
\* `cleanupAuditScratchpad` is intentionally UNCHANGED here (including
\* the forced-Done arms in the kernel's `apply_audit_response` —
\* malformed-exhausted and validation-failure-exhausted both reach this
\* spec action via the "transition to reviewer" path). A same-round
\* prior Valid burst's accumulated auditor reasoning is meaningful
\* context for the reviewer. Only a round bump (re-audit, via
\* `apply_cleanup_review_response`) clears it.
AcceptCleanupAuditDone ==
    /\ stage = "CleanupAudit"
    /\ inFlightRequest # NoRequest
    /\ inFlightRequest.kind = "audit"
    /\ inFlightRequest.cycle = cycle
    /\ response # NoResponse
    /\ stage' = "Reviewer"
    /\ cleanupAuditBurstCount' = cleanupAuditBurstCount + 1
    /\ inFlightRequest' = NoRequest
    /\ response' = NoResponse
    \* `stage` is in VarsExceptCleanupV2AndRequest but is reassigned above;
    \* TLA conjunction semantics require us NOT to UNCHANGED it. Replace the
    \* alias with the same-named variables minus `stage`.
    /\ UNCHANGED <<
        phase,
        cycle,
        attempt,
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
        nodeDifficulty,
        easyAttempts,
        reviewerComments,
        latestPaperEvidenceLanes,
        latestCorrEvidenceLanes,
        latestSoundEvidenceLanes,
        latestPaperReviewTargets,
        latestCorrReviewNodes,
        latestSoundReviewNodes,
        latestPaperPanelSplit,
        latestCorrPanelSplit,
        latestSoundPanelSplit,
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
        forceReviewAfterConeClean
       >>
    /\ UNCHANGED requestSeq
    /\ UNCHANGED <<
        cleanupAuditTasks,
        cleanupAuditScratchpad,
        cleanupAuditRound,
        cleanupConsecutiveInvalidWorkers,
        cleanupActiveTask,
        cleanupForceDone
       >>
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars

\* ----------------------------------------------------------------------
\* StuckMathAudit response acceptance (2026-05-31)
\* ----------------------------------------------------------------------
\*
\* Mirror of kernel `apply_stuck_math_audit_response` (engine.rs) +
\* `retry_or_transition_stuck_math_audit_to_reviewer`. The five
\* outcomes of a StuckMathAudit response:
\*
\*   AcceptStuckMathAuditDispatchHumanGate:
\*       Valid response + need_input_audit context Some +
\*       confirm_need_input = TRUE. Dispatches HumanGate with
\*       gateKind = "needinput" and gateFromInvalidAttempt taken from
\*       the context. Clears the latch.
\*
\*   AcceptStuckMathAuditBackToReviewer:
\*       Valid response with either (a) no need_input_audit context
\*       and no cone-clean (the plain audit-finished arm), or (b)
\*       need_input_audit context Some and confirm_need_input = FALSE
\*       (audit decided escalation was unnecessary). Routes to
\*       Reviewer; clears the latch.
\*
\*   AcceptStuckMathAuditRetry:
\*       Malformed / validation-failure response with retry counter
\*       still under STUCK_MATH_AUDIT_BURST_RETRY_LIMIT (1). Bumps
\*       the counter and re-issues a StuckMathAudit request. Stays
\*       at stage = "StuckMathAudit".
\*
\*   AcceptStuckMathAuditRetryExhaustedDispatchHumanGate:
\*       Malformed / validation-failure response, retry counter at
\*       limit, AND need_input_audit context Some. Routes to
\*       HumanGate (NeedInput, with gateFromInvalidAttempt from the
\*       context). Mirrors the kernel's "NeedInputAuditor failed
\*       twice in a row; routing to HumanGate without a new plan"
\*       arm.
\*
\*   AcceptStuckMathAuditRetryExhaustedBackToReviewer:
\*       Malformed / validation-failure response, retry counter at
\*       limit, no need_input_audit context. Routes to Reviewer.
\*       Mirrors the kernel's "stuck math audit failed twice in a
\*       row; routing to reviewer" arm.
\*
\* All five actions clear the in-flight request and response via
\* ClearArtifactsAndRecordHistory. The downstream IssueX requests
\* (IssueHumanGateRequest / IssueReviewRequest /
\* IssueStuckMathAuditRequest) fire as separate transitions
\* (consistent with the existing spec pattern of accept-then-issue).
\*
\* `auditPlan` and `supersededAuditPlan` are mutated in lock-step with
\* the Valid acceptance arms (DispatchHumanGate / BackToReviewer /
\* ConeClean): the kernel publishes the audit plan unconditionally on
\* a Valid response. The cone-clean reset is the additional sixth
\* arm (`AcceptStuckMathAuditConeClean`), mirroring the kernel's
\* `apply_audit_authorized_theorem_stating_node_reset` branch.

AcceptStuckMathAuditDispatchHumanGate ==
    /\ stage = "StuckMathAudit"
    /\ inFlightRequest # NoRequest
    /\ inFlightRequest.kind = "stuck_math_audit"
    /\ inFlightRequest.cycle = cycle
    /\ response # NoResponse
    /\ response.kind = "stuck_math_audit"
    /\ response.status = "ok"
    /\ stuckMathAuditActive
    /\ stuckMathAuditNeedInputAudit # NoNeedInputAuditContext
    /\ response.confirmNeedInput = TRUE
    \* Kernel `apply_stuck_math_audit_response` validation:
    \* `confirm_need_input` is incompatible with `cone_clean_node`
    \* (engine.rs `stuck_math_audit_validation_failure` at the start
    \* of the response handler).
    /\ response.coneClean = NoNode
    /\ stage' = "HumanGate"
    /\ gateKind' = "needinput"
    /\ gateFromInvalidAttempt' = stuckMathAuditNeedInputAudit.gateFromInvalidAttempt
    /\ stuckMathAuditActive' = TRUE  \* latch stays on through HumanGate dispatch
    /\ stuckMathAuditNeedInputAudit' = NoNeedInputAuditContext
    /\ stuckMathAuditBurstRetryCount' = 0
    /\ pendingTask' = NoPendingTask
    \* Kernel publishes the audit plan unconditionally on a Valid
    \* response (`state.superseded_audit_plan = state.audit_plan.clone();
    \* state.audit_plan = Some(AuditPlan{...})`). The spec abstracts
    \* the plan record into the bounded `AuditPlanValues` universe.
    /\ supersededAuditPlan' = auditPlan
    /\ auditPlan' = response.auditPlan
    /\ inFlightRequest' = NoRequest
    /\ response' = NoResponse
    /\ nativeHistoryKinds' = nativeHistoryKinds \cup {<<inFlightRequest.kind, inFlightRequest.phase>>}
    /\ UNCHANGED requestSeq
    /\ UNCHANGED <<
        phase,
        cycle,
        attempt,
        invalidAttempt,
        retryOutcomeKind,
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
        nodeDifficulty,
        easyAttempts,
        reviewerComments,
        latestPaperEvidenceLanes,
        latestCorrEvidenceLanes,
        latestSoundEvidenceLanes,
        latestPaperReviewTargets,
        latestCorrReviewNodes,
        latestSoundReviewNodes,
        latestPaperPanelSplit,
        latestCorrPanelSplit,
        latestSoundPanelSplit,
        previousPaperFindingLanes,
        previousCorrFindingLanes,
        previousSoundFindingLanes,
        latestSubstantivenessEvidenceLanes,
        latestSubstantivenessReviewNodes,
        latestSubstantivenessPanelSplit,
        previousSubstantivenessFindingLanes,
        humanInputOutstanding,
        cyclesSinceClean,
        hasEverBeenClean,
        forceReviewAfterConeClean,
        lastStuckMathAuditDispatchedCycle
       >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars

AcceptStuckMathAuditBackToReviewer ==
    /\ stage = "StuckMathAudit"
    /\ inFlightRequest # NoRequest
    /\ inFlightRequest.kind = "stuck_math_audit"
    /\ inFlightRequest.cycle = cycle
    /\ response # NoResponse
    /\ response.kind = "stuck_math_audit"
    /\ response.status = "ok"
    /\ stuckMathAuditActive
    \* Either (a) confirm_need_input = FALSE with a need_input_audit
    \* context, OR (b) no need_input_audit context at all and no
    \* cone-clean (the plain audit-done arm).
    /\ \/ /\ stuckMathAuditNeedInputAudit # NoNeedInputAuditContext
          /\ response.confirmNeedInput = FALSE
       \/ stuckMathAuditNeedInputAudit = NoNeedInputAuditContext
    \* Cone-clean response routes through a different arm
    \* (`AcceptStuckMathAuditConeClean`), which applies the
    \* audit-authorized theorem-stating-node reset.
    /\ response.coneClean = NoNode
    /\ stage' = "Reviewer"
    /\ stuckMathAuditActive' = stuckMathAuditActive  \* kernel does NOT clear the latch here; only the plan is published
    /\ stuckMathAuditNeedInputAudit' = NoNeedInputAuditContext
    /\ stuckMathAuditBurstRetryCount' = 0
    \* Kernel publishes the audit plan unconditionally on a Valid
    \* response (`state.superseded_audit_plan = state.audit_plan.clone();
    \* state.audit_plan = Some(AuditPlan{...})`). The spec abstracts
    \* the plan record into the bounded `AuditPlanValues` universe.
    /\ supersededAuditPlan' = auditPlan
    /\ auditPlan' = response.auditPlan
    /\ inFlightRequest' = NoRequest
    /\ response' = NoResponse
    /\ nativeHistoryKinds' = nativeHistoryKinds \cup {<<inFlightRequest.kind, inFlightRequest.phase>>}
    /\ UNCHANGED requestSeq
    /\ UNCHANGED <<
        phase,
        cycle,
        attempt,
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
        nodeDifficulty,
        easyAttempts,
        reviewerComments,
        latestPaperEvidenceLanes,
        latestCorrEvidenceLanes,
        latestSoundEvidenceLanes,
        latestPaperReviewTargets,
        latestCorrReviewNodes,
        latestSoundReviewNodes,
        latestPaperPanelSplit,
        latestCorrPanelSplit,
        latestSoundPanelSplit,
        previousPaperFindingLanes,
        previousCorrFindingLanes,
        previousSoundFindingLanes,
        latestSubstantivenessEvidenceLanes,
        latestSubstantivenessReviewNodes,
        latestSubstantivenessPanelSplit,
        previousSubstantivenessFindingLanes,
        humanInputOutstanding,
        pendingTask,
        cyclesSinceClean,
        hasEverBeenClean,
        forceReviewAfterConeClean,
        lastStuckMathAuditDispatchedCycle
       >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars

\* StuckMathAudit cone-clean arm (kernel
\* `apply_stuck_math_audit_response` cone_clean_node branch, then
\* `apply_audit_authorized_theorem_stating_node_reset` in engine.rs).
\* Fires when the audit response carries a `coneClean` node that
\* identifies a resettable coarse node. Routes through the runtime
\* reset path: clears active fields, sets stage=Start, latches
\* forceReviewAfterConeClean=TRUE so the next StartCycle issues a
\* routing Reviewer, and (when the cleaned node IS the current
\* anchor) clears `activeCoarseNode`. The audit plan is still
\* published with its `coneClean` field carrying the node id.
AcceptStuckMathAuditConeClean ==
    /\ stage = "StuckMathAudit"
    /\ inFlightRequest # NoRequest
    /\ inFlightRequest.kind = "stuck_math_audit"
    /\ inFlightRequest.cycle = cycle
    /\ response # NoResponse
    /\ response.kind = "stuck_math_audit"
    /\ response.status = "ok"
    /\ stuckMathAuditActive
    \* Cone-clean is incompatible with confirm_need_input (validated
    \* kernel-side in `stuck_math_audit_validation_failure`).
    /\ \/ stuckMathAuditNeedInputAudit = NoNeedInputAuditContext
       \/ /\ stuckMathAuditNeedInputAudit # NoNeedInputAuditContext
          /\ response.confirmNeedInput = FALSE
    /\ response.coneClean # NoNode
    /\ response.coneClean \in coarseDagNodes
    /\ phase = "proof_formalization"
    \* `apply_audit_authorized_theorem_stating_node_reset` mutations.
    /\ stage' = "Start"
    /\ activeNode' = NoNode
    /\ heldTarget' = NoNode
    /\ targetEditMode' = "global"
    /\ proofEditMode' = "local"
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ attempt' = 0
    /\ forceReviewAfterConeClean' = TRUE
    \* Anchor clearing rule (proposal v32 audit-2 followup #2): when
    \* the cleaned node IS the current anchor, drop the anchor so the
    \* next routing Review re-seeds it. Non-anchor cone-cleans
    \* preserve the anchor.
    /\ activeCoarseNode' =
        IF activeCoarseNode = response.coneClean THEN NoNode ELSE activeCoarseNode
    /\ cyclesInCoarseRepairMode' = 0
    \* Latch clears, retry context clears, pending task clears.
    /\ stuckMathAuditActive' = stuckMathAuditActive
    /\ stuckMathAuditNeedInputAudit' = NoNeedInputAuditContext
    /\ stuckMathAuditBurstRetryCount' = 0
    /\ pendingTask' = NoPendingTask
    /\ pendingProtectedReapprovalNodes' = {}
    /\ pendingProtectedSemanticScopeConfirmation' = NoProtectedSemanticChangeConfirmation
    \* Audit plan publication mirrors the unconditional Valid arm.
    /\ supersededAuditPlan' = auditPlan
    /\ auditPlan' = response.auditPlan
    /\ inFlightRequest' = NoRequest
    /\ response' = NoResponse
    /\ nativeHistoryKinds' = nativeHistoryKinds \cup {<<inFlightRequest.kind, inFlightRequest.phase>>}
    /\ invalidAttempt' = FALSE
    /\ UNCHANGED requestSeq
    /\ UNCHANGED <<
        phase,
        cycle,
        retryOutcomeKind,
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
        nodeDifficulty,
        easyAttempts,
        reviewerComments,
        latestPaperEvidenceLanes,
        latestCorrEvidenceLanes,
        latestSoundEvidenceLanes,
        latestPaperReviewTargets,
        latestCorrReviewNodes,
        latestSoundReviewNodes,
        latestPaperPanelSplit,
        latestCorrPanelSplit,
        latestSoundPanelSplit,
        previousPaperFindingLanes,
        previousCorrFindingLanes,
        previousSoundFindingLanes,
        latestSubstantivenessEvidenceLanes,
        latestSubstantivenessReviewNodes,
        latestSubstantivenessPanelSplit,
        previousSubstantivenessFindingLanes,
        humanInputOutstanding,
        cyclesSinceClean,
        hasEverBeenClean,
        lastStuckMathAuditDispatchedCycle
       >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    \* Sound assessment cluster: the cone-clean reset clears
    \* `sound_assessments` kernel-side via the eventual structural
    \* filter pass after the worktree restoration; the spec abstracts
    \* that as a downstream env action and leaves the store UNCHANGED
    \* on this transition.
    /\ UNCHANGED SoundAssessmentVars

\* Reviewer-side audit-plan whole-plan dismissal (kernel
\* `apply_review_audit_plan_actions` in engine.rs). Fires at reviewer
\* response acceptance when `response.dismissAuditPlan = TRUE`: the
\* live `auditPlan` is moved into `supersededAuditPlan` (preserving
\* the audit trail of any per-task dismissals applied in the same
\* reviewer response), and the StuckMathAudit latch is cleared so the
\* next `start_cycle` re-evaluates audit triggers from scratch.
\*
\* This action is OUT-OF-BAND from the other reviewer-decision actions
\* (`ReviewContinueProof` / `ReviewNeedInputProof` / etc.). The kernel
\* runs `apply_review_audit_plan_actions` BEFORE the phase-specific
\* handler. The spec abstracts this ordering by allowing this action
\* to fire separately when the reviewer's response carries the
\* dismissal signal, leaving the decision-side mutations to the
\* corresponding decision action.
RecordAuditPlan ==
    /\ stage = "Reviewer"
    /\ inFlightRequest # NoRequest
    /\ inFlightRequest.kind = "review"
    /\ response # NoResponse
    /\ response.kind = "review"
    /\ response.status = "ok"
    /\ response.dismissAuditPlan
    /\ auditPlan # NoAuditPlan
    \* Kernel legality gate (`review_response_audit_plan_legal`):
    \* whole-plan dismissal requires StuckMathAudit latch to be active.
    /\ stuckMathAuditActive
    /\ supersededAuditPlan' = auditPlan
    /\ auditPlan' = NoAuditPlan
    \* Clearing the StuckMathAudit latch + need-input-audit context
    \* mirrors `state.stuck_math_audit.active = false` in the
    \* kernel's `apply_review_audit_plan_actions`.
    /\ stuckMathAuditActive' = FALSE
    /\ stuckMathAuditNeedInputAudit' = NoNeedInputAuditContext
    /\ stuckMathAuditBurstRetryCount' = 0
    /\ UNCHANGED <<
        phase, stage, cycle, attempt, requestSeq,
        invalidAttempt, retryOutcomeKind, gateKind,
        gateFromInvalidAttempt, activeNode, heldTarget,
        targetEditMode, proofEditMode, configuredTargets,
        approvedConfiguredTargets, currentProofNodes,
        committedProofNodes, currentNodeKinds, committedNodeKinds,
        currentDeps, committedDeps, currentTargetClaims,
        committedTargetClaims, presentNodes, committedPresentNodes,
        openNodes, committedOpenNodes, localClosureUnverified,
        committedLocalClosureUnverified, currentCoverage,
        committedCoverage, approvedCoverage, paperStatus,
        paperCurrentFp, committedPaperCurrentFp, paperApprovedFp,
        substantivenessStatus, substantivenessCurrentFp,
        committedSubstantivenessCurrentFp, substantivenessApprovedFp,
        currentTargetFp, committedTargetFp, approvedTargetFp,
        coarseDagNodes, corrStatus, corrCurrentFp,
        committedCorrCurrentFp, corrApprovedFp, soundStatus,
        soundCurrentFp, committedSoundCurrentFp, soundApprovedFp,
        nodeDifficulty, easyAttempts, reviewerComments,
        latestPaperEvidenceLanes, latestCorrEvidenceLanes,
        latestSoundEvidenceLanes, latestPaperReviewTargets,
        latestCorrReviewNodes, latestSoundReviewNodes,
        latestPaperPanelSplit, latestCorrPanelSplit,
        latestSoundPanelSplit, previousPaperFindingLanes,
        previousCorrFindingLanes, previousSoundFindingLanes,
        latestSubstantivenessEvidenceLanes,
        latestSubstantivenessReviewNodes,
        latestSubstantivenessPanelSplit,
        previousSubstantivenessFindingLanes,
        humanInputOutstanding, nativeHistoryKinds,
        cyclesSinceClean, hasEverBeenClean, pendingTask,
        forceReviewAfterConeClean, inFlightRequest, response,
        lastStuckMathAuditDispatchedCycle
       >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED DeviationVars

\* Reviewer-side per-task audit-plan dismissal (kernel
\* `apply_review_audit_plan_actions` in engine.rs). Fires when the
\* reviewer response names a task id in `response.dismissedTasks`
\* and the live `auditPlan` carries that task. Mutates the task's
\* `dismissed` flag from "pending" to "dismissed". The transition is
\* sticky (dismissed tasks cannot return to pending).
DismissAuditPlanTask ==
    /\ stage = "Reviewer"
    /\ inFlightRequest # NoRequest
    /\ inFlightRequest.kind = "review"
    /\ response # NoResponse
    /\ response.kind = "review"
    /\ response.status = "ok"
    /\ auditPlan # NoAuditPlan
    /\ \E taskId \in response.dismissedTasks:
        /\ taskId \in DOMAIN auditPlan.tasks
        /\ auditPlan.tasks[taskId] = "pending"
        /\ auditPlan' = [auditPlan EXCEPT !.tasks[taskId] = "dismissed"]
    /\ UNCHANGED supersededAuditPlan
    /\ UNCHANGED <<
        phase, stage, cycle, attempt, requestSeq,
        invalidAttempt, retryOutcomeKind, gateKind,
        gateFromInvalidAttempt, activeNode, heldTarget,
        targetEditMode, proofEditMode, configuredTargets,
        approvedConfiguredTargets, currentProofNodes,
        committedProofNodes, currentNodeKinds, committedNodeKinds,
        currentDeps, committedDeps, currentTargetClaims,
        committedTargetClaims, presentNodes, committedPresentNodes,
        openNodes, committedOpenNodes, localClosureUnverified,
        committedLocalClosureUnverified, currentCoverage,
        committedCoverage, approvedCoverage, paperStatus,
        paperCurrentFp, committedPaperCurrentFp, paperApprovedFp,
        substantivenessStatus, substantivenessCurrentFp,
        committedSubstantivenessCurrentFp, substantivenessApprovedFp,
        currentTargetFp, committedTargetFp, approvedTargetFp,
        coarseDagNodes, corrStatus, corrCurrentFp,
        committedCorrCurrentFp, corrApprovedFp, soundStatus,
        soundCurrentFp, committedSoundCurrentFp, soundApprovedFp,
        nodeDifficulty, easyAttempts, reviewerComments,
        latestPaperEvidenceLanes, latestCorrEvidenceLanes,
        latestSoundEvidenceLanes, latestPaperReviewTargets,
        latestCorrReviewNodes, latestSoundReviewNodes,
        latestPaperPanelSplit, latestCorrPanelSplit,
        latestSoundPanelSplit, previousPaperFindingLanes,
        previousCorrFindingLanes, previousSoundFindingLanes,
        latestSubstantivenessEvidenceLanes,
        latestSubstantivenessReviewNodes,
        latestSubstantivenessPanelSplit,
        previousSubstantivenessFindingLanes,
        humanInputOutstanding, nativeHistoryKinds,
        cyclesSinceClean, hasEverBeenClean, pendingTask,
        forceReviewAfterConeClean, inFlightRequest, response
       >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED DeviationVars

\* StuckMathAudit retry (under STUCK_MATH_AUDIT_BURST_RETRY_LIMIT).
\* Mirrors `retry_or_transition_stuck_math_audit_to_reviewer` in
\* engine.rs when counter < limit: bumps counter, re-issues
\* StuckMathAudit request. The retry fires both on malformed and on
\* validation-failure responses.
AcceptStuckMathAuditRetry ==
    /\ stage = "StuckMathAudit"
    /\ inFlightRequest # NoRequest
    /\ inFlightRequest.kind = "stuck_math_audit"
    /\ inFlightRequest.cycle = cycle
    /\ response # NoResponse
    /\ response.kind = "stuck_math_audit"
    /\ \/ response.status = "malformed"
       \/ /\ response.status = "ok"
          /\ ~response.valid
    /\ stuckMathAuditBurstRetryCount < 1  \* STUCK_MATH_AUDIT_BURST_RETRY_LIMIT
    /\ stage' = "StuckMathAudit"
    /\ stuckMathAuditActive' = stuckMathAuditActive
    /\ stuckMathAuditNeedInputAudit' = stuckMathAuditNeedInputAudit
    /\ stuckMathAuditBurstRetryCount' = stuckMathAuditBurstRetryCount + 1
    /\ inFlightRequest' = NoRequest
    /\ response' = NoResponse
    /\ nativeHistoryKinds' = nativeHistoryKinds \cup {<<inFlightRequest.kind, inFlightRequest.phase>>}
    /\ UNCHANGED requestSeq
    /\ UNCHANGED <<
        phase,
        cycle,
        attempt,
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
        nodeDifficulty,
        easyAttempts,
        reviewerComments,
        latestPaperEvidenceLanes,
        latestCorrEvidenceLanes,
        latestSoundEvidenceLanes,
        latestPaperReviewTargets,
        latestCorrReviewNodes,
        latestSoundReviewNodes,
        latestPaperPanelSplit,
        latestCorrPanelSplit,
        latestSoundPanelSplit,
        previousPaperFindingLanes,
        previousCorrFindingLanes,
        previousSoundFindingLanes,
        latestSubstantivenessEvidenceLanes,
        latestSubstantivenessReviewNodes,
        latestSubstantivenessPanelSplit,
        previousSubstantivenessFindingLanes,
        humanInputOutstanding,
        pendingTask,
        cyclesSinceClean,
        hasEverBeenClean,
        forceReviewAfterConeClean,
        lastStuckMathAuditDispatchedCycle
       >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars

\* StuckMathAudit retry-exhausted, fall back to Reviewer.
\* Mirrors `retry_or_transition_stuck_math_audit_to_reviewer` in
\* engine.rs when counter >= limit AND `stuck_math_audit.need_input_audit`
\* is None: clears the burst-retry counter, routes back to Reviewer.
\*
\* Auto-decline of an in-flight `pendingGlobalRepairRequest` (commit
\* "global_repair / NeedInput mutex enforcement"): a retry-exhaust on a
\* GR-audit dispatch leaves `pendingGlobalRepairRequest = Some` until
\* this action's atomic clear, which preserves the mutex invariant
\* (`pendingGlobalRepairRequest # NoGlobalRepairRequest /\
\* stuckMathAuditNeedInputAudit # NoNeedInputAuditContext` is forbidden
\* by TypeOK; the next Reviewer cycle's `ReviewNeedInputProof` would
\* otherwise wedge the configuration).
AcceptStuckMathAuditRetryExhaustedBackToReviewer ==
    /\ stage = "StuckMathAudit"
    /\ inFlightRequest # NoRequest
    /\ inFlightRequest.kind = "stuck_math_audit"
    /\ inFlightRequest.cycle = cycle
    /\ response # NoResponse
    /\ response.kind = "stuck_math_audit"
    /\ \/ response.status = "malformed"
       \/ /\ response.status = "ok"
          /\ ~response.valid
    /\ stuckMathAuditBurstRetryCount >= 1  \* STUCK_MATH_AUDIT_BURST_RETRY_LIMIT
    /\ stuckMathAuditNeedInputAudit = NoNeedInputAuditContext
    /\ stage' = "Reviewer"
    /\ stuckMathAuditActive' = stuckMathAuditActive
    /\ stuckMathAuditNeedInputAudit' = NoNeedInputAuditContext
    /\ stuckMathAuditBurstRetryCount' = 0
    \* GR-audit retry-exhaust auto-decline. Symmetric pair of the
    \* `stuckMathAuditNeedInputAudit' = NoNeedInputAuditContext` clear
    \* in `RequestGlobalRepairAudit`; both keep the
    \* `pendingGlobalRepairRequest # NoGlobalRepairRequest` /
    \* `stuckMathAuditNeedInputAudit # NoNeedInputAuditContext` mutex
    \* invariant intact across audit-lane transitions.
    /\ pendingGlobalRepairRequest' = NoGlobalRepairRequest
    /\ pendingGlobalRepairGrant' =
           IF pendingGlobalRepairRequest # NoGlobalRepairRequest
                THEN NoGlobalRepairGrant
                ELSE pendingGlobalRepairGrant
    /\ latestGlobalRepairAuditDeclineReason' =
           IF pendingGlobalRepairRequest # NoGlobalRepairRequest
                THEN "declined"
                ELSE latestGlobalRepairAuditDeclineReason
    /\ latestGlobalRepairAuditDeclineCycle' =
           IF pendingGlobalRepairRequest # NoGlobalRepairRequest
                THEN cycle
                ELSE latestGlobalRepairAuditDeclineCycle
    /\ lastReviewerGlobalRepairRequestCycle' = lastReviewerGlobalRepairRequestCycle
    /\ everShallowCoarseClosed' = everShallowCoarseClosed
    /\ globalRepairModeEnabled' = globalRepairModeEnabled
    /\ inFlightRequest' = NoRequest
    /\ response' = NoResponse
    /\ nativeHistoryKinds' = nativeHistoryKinds \cup {<<inFlightRequest.kind, inFlightRequest.phase>>}
    /\ UNCHANGED requestSeq
    /\ UNCHANGED <<
        phase,
        cycle,
        attempt,
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
        nodeDifficulty,
        easyAttempts,
        reviewerComments,
        latestPaperEvidenceLanes,
        latestCorrEvidenceLanes,
        latestSoundEvidenceLanes,
        latestPaperReviewTargets,
        latestCorrReviewNodes,
        latestSoundReviewNodes,
        latestPaperPanelSplit,
        latestCorrPanelSplit,
        latestSoundPanelSplit,
        previousPaperFindingLanes,
        previousCorrFindingLanes,
        previousSoundFindingLanes,
        latestSubstantivenessEvidenceLanes,
        latestSubstantivenessReviewNodes,
        latestSubstantivenessPanelSplit,
        previousSubstantivenessFindingLanes,
        humanInputOutstanding,
        pendingTask,
        cyclesSinceClean,
        hasEverBeenClean,
        forceReviewAfterConeClean,
        lastStuckMathAuditDispatchedCycle
       >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars

\* StuckMathAudit retry-exhausted with NeedInput context, fall back to
\* HumanGate. Mirrors the kernel's "NeedInputAuditor failed twice in a
\* row; routing to HumanGate without a new plan" arm in
\* `retry_or_transition_stuck_math_audit_to_reviewer`. The NeedInput
\* lane is mutex-exclusive with the GR lane (mutex invariant in
\* TypeOK), so this action never observes a non-default
\* `pendingGlobalRepairRequest` and leaves it UNCHANGED.
AcceptStuckMathAuditRetryExhaustedDispatchHumanGate ==
    /\ stage = "StuckMathAudit"
    /\ inFlightRequest # NoRequest
    /\ inFlightRequest.kind = "stuck_math_audit"
    /\ inFlightRequest.cycle = cycle
    /\ response # NoResponse
    /\ response.kind = "stuck_math_audit"
    /\ \/ response.status = "malformed"
       \/ /\ response.status = "ok"
          /\ ~response.valid
    /\ stuckMathAuditBurstRetryCount >= 1  \* STUCK_MATH_AUDIT_BURST_RETRY_LIMIT
    /\ stuckMathAuditNeedInputAudit # NoNeedInputAuditContext
    /\ stage' = "HumanGate"
    /\ gateKind' = "needinput"
    /\ gateFromInvalidAttempt' = stuckMathAuditNeedInputAudit.gateFromInvalidAttempt
    /\ stuckMathAuditActive' = stuckMathAuditActive
    /\ stuckMathAuditNeedInputAudit' = NoNeedInputAuditContext
    /\ stuckMathAuditBurstRetryCount' = 0
    /\ pendingTask' = NoPendingTask
    /\ inFlightRequest' = NoRequest
    /\ response' = NoResponse
    /\ nativeHistoryKinds' = nativeHistoryKinds \cup {<<inFlightRequest.kind, inFlightRequest.phase>>}
    /\ UNCHANGED requestSeq
    /\ UNCHANGED <<
        phase,
        cycle,
        attempt,
        invalidAttempt,
        retryOutcomeKind,
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
        nodeDifficulty,
        easyAttempts,
        reviewerComments,
        latestPaperEvidenceLanes,
        latestCorrEvidenceLanes,
        latestSoundEvidenceLanes,
        latestPaperReviewTargets,
        latestCorrReviewNodes,
        latestSoundReviewNodes,
        latestPaperPanelSplit,
        latestCorrPanelSplit,
        latestSoundPanelSplit,
        previousPaperFindingLanes,
        previousCorrFindingLanes,
        previousSoundFindingLanes,
        latestSubstantivenessEvidenceLanes,
        latestSubstantivenessReviewNodes,
        latestSubstantivenessPanelSplit,
        previousSubstantivenessFindingLanes,
        humanInputOutstanding,
        cyclesSinceClean,
        hasEverBeenClean,
        forceReviewAfterConeClean,
        lastStuckMathAuditDispatchedCycle
       >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars

\* ----------------------------------------------------------------------
\* === global_repair_mode (2026-06-05) ===
\* ----------------------------------------------------------------------
\*
\* Three-step audit-gated cone-widening dance. Mirrors the kernel
\* `global_repair_mode` implementation in model.rs / engine.rs:
\*
\*   Step A (RequestGlobalRepairAudit): the reviewer in ProofFormalization
\*       emits a `global_repair_request` on a Continue. Kernel persists
\*       `pendingGlobalRepairRequest`, routes to the StuckMathAudit lane
\*       (sets stage = "StuckMathAudit", activates the audit latch). Does
\*       NOT dispatch a worker. Subject to a cooldown against
\*       `lastReviewerGlobalRepairRequestCycle` (S10) and a no-overlap
\*       check against `liveProtectedStatementNodeSet` (the latter not
\*       modeled at the spec level; deferred to the kernel validator).
\*
\*   Step B (ApplyStuckMathAuditGlobalRepairResponse): the auditor returns
\*       a stuck_math_audit response carrying `global_repair_approve` and
\*       `global_repair_approved_extension_node_ids`. On approve, kernel
\*       sets `pendingGlobalRepairGrant` and clears
\*       `pendingGlobalRepairRequest`; on decline, kernel records
\*       `latestGlobalRepairAuditDeclineReason` and clears
\*       `pendingGlobalRepairRequest`. Either way, kernel routes back to
\*       Reviewer. The structural cap (S5) — approved ⊆ dep-neighborhood
\*       of proposed AND approved ∩ liveProtectedStatementNodeSet = {} —
\*       is enforced in the spec via the dep-neighborhood subset; the
\*       protected-set check is deferred to the kernel.
\*
\*   Step C (ConsumeGlobalRepairGrant): the reviewer Continue cycle that
\*       follows a Step B approve sets `consume_global_repair_grant = TRUE`
\*       AND submits `authorizedNodes` that lie in the union of the
\*       reviewer scope envelope and the grant's
\*       `approvedExtensionNodes`. Treated here as a worker-dispatch
\*       cycle (mirrors ReviewContinueProof) with two differences:
\*         (i) the grant is cleared on the same step (the spec abstracts
\*             the kernel's "clear on worker-acceptance" rule, which in
\*             the spec is the same atomic moment because the worker
\*             dispatch is atomic);
\*         (ii) `cyclesInCoarseRepairMode` is incremented (NOT reset)
\*             even when the anchor stayed put — the Step C burst does
\*             NOT count as anchor progress (S11).

\* Step A. Reviewer Continue with a non-NoGlobalRepairRequest
\* `globalRepairRequest`. Routes to the StuckMathAudit lane (mirrors
\* `route_global_repair_request_to_auditor` in engine.rs). Mutates
\* `pendingGlobalRepairRequest`, `lastReviewerGlobalRepairRequestCycle`,
\* clears any prior `latestGlobalRepairAuditDeclineReason`, and
\* activates the StuckMathAudit latch (parallel of the NeedInput
\* route_need_input_to_auditor pattern).
RequestGlobalRepairAudit ==
    /\ phase = "proof_formalization"
    /\ stage = "Reviewer"
    /\ ReviewDecisionLegal
    /\ response.decision = "CONTINUE"
    /\ response.globalRepairRequest # NoGlobalRepairRequest
    /\ ~response.consumeGlobalRepairGrant
    /\ globalRepairModeEnabled
    \* No double-dispatch: must not already have an in-flight Step A.
    /\ pendingGlobalRepairRequest = NoGlobalRepairRequest
    \* Cooldown gate (S10).
    /\ \/ lastReviewerGlobalRepairRequestCycle = NoCycle
       \/ cycle - lastReviewerGlobalRepairRequestCycle >=
          StuckMathAuditDispatchCooldownCycles
    \* State: package the reviewer ask into the persisted slot, route
    \* to the audit lane, clear any prior decline reason.
    /\ pendingGlobalRepairRequest' =
           [proposedExtensionNodes |->
                response.globalRepairRequest.proposedExtensionNodes,
            dispatchedAtCycle |-> cycle]
    /\ lastReviewerGlobalRepairRequestCycle' = cycle
    /\ latestGlobalRepairAuditDeclineReason' = ""
    /\ latestGlobalRepairAuditDeclineCycle' = NoCycle
    /\ pendingGlobalRepairGrant' = pendingGlobalRepairGrant
    /\ everShallowCoarseClosed' = everShallowCoarseClosed
    /\ globalRepairModeEnabled' = globalRepairModeEnabled
    /\ stage' = "StuckMathAudit"
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ stuckMathAuditActive' = TRUE
    /\ stuckMathAuditNeedInputAudit' = NoNeedInputAuditContext
    /\ stuckMathAuditBurstRetryCount' = 0
    /\ lastStuckMathAuditDispatchedCycle' = cycle
    /\ pendingTask' = NoPendingTask
    /\ reviewerComments' = response.comments
    /\ latestPaperEvidenceLanes' = {}
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = {}
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ nodeDifficulty' = ApplyDifficultyUpdates(nodeDifficulty, response.difficultyMap)
    /\ easyAttempts' = ApplyDifficultyAttemptResets(nodeDifficulty, easyAttempts, response.difficultyMap)
    /\ humanInputOutstanding' =
           IF response.clearHumanInput THEN FALSE ELSE humanInputOutstanding
    /\ invalidAttempt' = FALSE
    \* Post-advance routing latch cleared at entry to
    \* `apply_proof_review_response` (kernel engine.rs) — before the
    \* Step A short-circuit. The reviewer's post_advance_routing
    \* contract is satisfied by the in-flight Review having carried
    \* the flag; subsequent dispatches derive `post_advance_routing:
    \* false`.
    /\ postAdvanceRoutingPending' = FALSE
    /\ UNCHANGED <<
           phase,
           cycle,
           attempt,
           retryOutcomeKind,
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
           cyclesSinceClean,
           hasEverBeenClean,
           forceReviewAfterConeClean
       >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ ClearArtifactsAndRecordHistory

\* Step B. Auditor returns a stuck_math_audit response carrying a
\* global_repair decision (approve / decline). Mirrors the
\* `Step B` block added to `apply_stuck_math_audit_response` in
\* engine.rs. Approves set `pendingGlobalRepairGrant`; declines set
\* `latestGlobalRepairAuditDeclineReason`. Either way, clears
\* `pendingGlobalRepairRequest` and routes back to Reviewer.
\*
\* Structural cap (S5): on approve, the approved extension nodes
\* must be a subset of the union of `ImpactRegion(seed, presentNodes)`
\* over each `seed` in the original Step A proposal. The protected-set
\* disjointness (S11) is enforced kernel-side and is not modeled here
\* (the spec does not model `liveProtectedStatementNodeSet`).
ApplyStuckMathAuditGlobalRepairResponse ==
    /\ stage = "StuckMathAudit"
    /\ inFlightRequest # NoRequest
    /\ inFlightRequest.kind = "stuck_math_audit"
    /\ inFlightRequest.cycle = cycle
    /\ response # NoResponse
    /\ response.kind = "stuck_math_audit"
    /\ response.status = "ok"
    /\ stuckMathAuditActive
    /\ pendingGlobalRepairRequest # NoGlobalRepairRequest
    /\ LET allowed ==
            UNION { ImpactRegion(seed, presentNodes) :
                    seed \in pendingGlobalRepairRequest.proposedExtensionNodes }
           approvedSet == response.globalRepairApprovedExtensionNodes
       IN
           \* Either approve with non-empty cap-respecting set, or decline.
           /\ \/ /\ response.globalRepairApprove
                 /\ approvedSet # {}
                 /\ approvedSet \subseteq allowed
                 /\ pendingGlobalRepairGrant' =
                        [approvedExtensionNodes |-> approvedSet,
                         dispatchedAtCycle |->
                             pendingGlobalRepairRequest.dispatchedAtCycle]
                 /\ latestGlobalRepairAuditDeclineReason' = ""
                 /\ latestGlobalRepairAuditDeclineCycle' = NoCycle
              \/ /\ ~response.globalRepairApprove
                 /\ pendingGlobalRepairGrant' = NoGlobalRepairGrant
                 /\ latestGlobalRepairAuditDeclineReason' = "declined"
                 /\ latestGlobalRepairAuditDeclineCycle' = cycle
    /\ pendingGlobalRepairRequest' = NoGlobalRepairRequest
    /\ lastReviewerGlobalRepairRequestCycle' = lastReviewerGlobalRepairRequestCycle
    /\ everShallowCoarseClosed' = everShallowCoarseClosed
    /\ globalRepairModeEnabled' = globalRepairModeEnabled
    \* Route back to Reviewer. Latch stays on (parallel of the existing
    \* AcceptStuckMathAuditBackToReviewer behaviour) so subsequent
    \* StuckMathAudit invariants stay coherent; the reviewer's next
    \* Continue cycle (which may consume the grant) does not depend on
    \* the latch.
    /\ stage' = "Reviewer"
    /\ stuckMathAuditActive' = stuckMathAuditActive
    /\ stuckMathAuditNeedInputAudit' = stuckMathAuditNeedInputAudit
    /\ stuckMathAuditBurstRetryCount' = 0
    /\ lastStuckMathAuditDispatchedCycle' = lastStuckMathAuditDispatchedCycle
    /\ inFlightRequest' = NoRequest
    /\ response' = NoResponse
    /\ nativeHistoryKinds' = nativeHistoryKinds \cup {<<inFlightRequest.kind, inFlightRequest.phase>>}
    /\ UNCHANGED requestSeq
    /\ UNCHANGED <<
           phase,
           cycle,
           attempt,
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
           nodeDifficulty,
           easyAttempts,
           reviewerComments,
           latestPaperEvidenceLanes,
           latestCorrEvidenceLanes,
           latestSoundEvidenceLanes,
           latestPaperReviewTargets,
           latestCorrReviewNodes,
           latestSoundReviewNodes,
           latestPaperPanelSplit,
           latestCorrPanelSplit,
           latestSoundPanelSplit,
           previousPaperFindingLanes,
           previousCorrFindingLanes,
           previousSoundFindingLanes,
           latestSubstantivenessEvidenceLanes,
           latestSubstantivenessReviewNodes,
           latestSubstantivenessPanelSplit,
           previousSubstantivenessFindingLanes,
           humanInputOutstanding,
           pendingTask,
           cyclesSinceClean,
           hasEverBeenClean,
           forceReviewAfterConeClean
       >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars

\* Step C. Reviewer Continue with `consumeGlobalRepairGrant = TRUE`.
\* Mirrors `ReviewContinueProof` (worker-dispatch path), with three
\* mechanical differences from the standard Continue arm:
\*   (i)   `pendingGlobalRepairGrant` is cleared (S6: clear on worker-
\*         acceptance; in the spec the dispatch is atomic, so the
\*         clear coincides with the action).
\*   (ii)  `cyclesInCoarseRepairMode` is incremented even when the
\*         anchor stays put (S11: the burst does NOT count as
\*         anchor progress).
\*   (iii) `nextMode = "local"` is rejected — already enforced
\*         in `ReviewDecisionLegal`. Anchor switches are rejected; the
\*         grant widens authorizedNodes for this dispatch while the
\*         active coarse anchor stays sticky.
\*
\* Retry context is preserved. A grant consumed after Stuck /
\* NeedsRestructure / Invalid dispatches the retry worker directly in the
\* same cycle, just like `ReviewContinueProof`; it does not force a clean
\* checkpoint boundary before the widened repair burst.
\*
\* The legality gates inherited from `ReviewDecisionLegal` already
\* enforce: kill-switch on, phase = proof_formalization, decision =
\* CONTINUE with reset = NoCheckpoint, mutex with global_repair_request,
\* and `pendingGlobalRepairGrant # NoGlobalRepairGrant`.
ConsumeGlobalRepairGrant ==
    /\ phase = "proof_formalization"
    /\ stage = "Reviewer"
    /\ ReviewDecisionLegal
    /\ response.decision = "CONTINUE"
    /\ response.consumeGlobalRepairGrant
    \* The cone gate exemption is contributed by the grant: the action
    \* admits any `authorizedNodes` lying in the union of the standard
    \* scope envelope and the grant's approvedExtensionNodes. (The base
    \* `ReviewDecisionLegal` clause on `authorizedNodes ⊆ envelope ∪
    \* CoarseLegalActiveSetForAnchor(...)` is loose-coupled in the spec
    \* abstraction — we additionally cap to presentNodes here for
    \* TypeOK safety.)
    /\ response.authorizedNodes \subseteq
           (ReviewScopeEnvelope(response.nextMode, response.nextActive)
            \cup pendingGlobalRepairGrant.approvedExtensionNodes)
    /\ response.authorizedNodes \subseteq presentNodes
    /\ LET chosenActive ==
              IF response.nextActive # NoNode
                  THEN response.nextActive
                  ELSE activeNode
       IN
        /\ activeNode' = chosenActive
        /\ heldTarget' = NoNode
        /\ targetEditMode' = "global"
        /\ proofEditMode' =
               IF chosenActive = NoNode THEN "local" ELSE response.nextMode
    /\ pendingTask' =
        [
            taskBlockers |-> response.taskBlockers,
            node |-> activeNode',
            mode |-> proofEditMode',
            orphanCleanupNodes |-> {},
            nextWorkerContextMode |-> response.nextWorkerContextMode,
            paperFocusRanges |-> response.paperFocusRanges,
            workStyleHint |-> response.workStyleHint,
            allowNewObligations |-> response.allowNewObligations,
            mustCloseActive |-> response.mustCloseActive,
            authorizedNodes |-> response.authorizedNodes,
            \* Step C presentational flag: this pending task was
            \* produced by a `consume_global_repair_grant` Continue
            \* (kernel model.rs PendingTask::consumed_global_repair_grant
            \* in engine.rs line ~4235). Propagates into the worker
            \* request via `RequestWorkerContext`.
            consumedGlobalRepairGrant |-> TRUE
        ]
    \* Worktree / fingerprint passthrough (no reset path — Step C
    \* requires reset = NoCheckpoint per ReviewDecisionLegal).
    /\ currentNodeKinds' = currentNodeKinds
    /\ currentProofNodes' = currentProofNodes
    /\ currentDeps' = currentDeps
    /\ currentTargetClaims' = currentTargetClaims
    /\ presentNodes' = presentNodes
    /\ openNodes' = openNodes
    /\ localClosureUnverified' = localClosureUnverified
    /\ currentCoverage' = currentCoverage
    /\ paperCurrentFp' = paperCurrentFp
    /\ currentTargetFp' = currentTargetFp
    /\ corrCurrentFp' = corrCurrentFp
    /\ soundCurrentFp' = soundCurrentFp
    /\ substantivenessCurrentFp' = substantivenessCurrentFp
    \* Status / approvedFp passthrough — Step C accepts the reviewer
    \* taskBlockers but does not run the full adjudication apparatus
    \* (mirrors the kernel: Step C is a normal worker-dispatch cycle
    \* whose adjudication outcome is whatever the worker burst
    \* returns; the spec abstracts that to a no-op on this step,
    \* since the env actions independently mutate status maps).
    /\ corrStatus' = corrStatus
    /\ corrApprovedFp' = corrApprovedFp
    /\ paperStatus' = paperStatus
    /\ paperApprovedFp' = paperApprovedFp
    /\ soundStatus' = soundStatus
    /\ soundApprovedFp' = soundApprovedFp
    \* Sound assessment store passthrough — Step C is a normal
    \* worker-dispatch (same as the legacy status passthrough above).
    /\ soundAssessmentStatus' = soundAssessmentStatus
    /\ reviewerRequestedSoundVerifierNodes' = reviewerRequestedSoundVerifierNodes
    /\ soundReverificationContext' = NoSoundReverificationContext
    /\ substantivenessStatus' = substantivenessStatus
    /\ substantivenessApprovedFp' = substantivenessApprovedFp
    /\ reviewerComments' = response.comments
    /\ latestPaperEvidenceLanes' = {}
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = {}
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ nodeDifficulty' = ApplyDifficultyUpdates(nodeDifficulty, response.difficultyMap)
    /\ easyAttempts' = ApplyDifficultyAttemptResets(nodeDifficulty, easyAttempts, response.difficultyMap)
    /\ phase' = phase
    /\ stage' = IF retryOutcomeKind # "none" THEN "Worker" ELSE "Start"
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ invalidAttempt' = FALSE
    /\ humanInputOutstanding' =
           IF response.clearHumanInput THEN FALSE ELSE humanInputOutstanding
    /\ attempt' = IF retryOutcomeKind # "none" THEN 1 ELSE 0
    /\ IF retryOutcomeKind # "none" THEN
            UNCHANGED
                <<
                    committedProofNodes,
                    committedNodeKinds,
                    committedDeps,
                    committedTargetClaims,
                    committedPresentNodes,
                    committedOpenNodes,
                    committedLocalClosureUnverified,
                    committedCoverage,
                    committedPaperCurrentFp,
                    committedSubstantivenessCurrentFp,
                    committedTargetFp,
                    committedCorrCurrentFp,
                    committedSoundCurrentFp,
                    cyclesSinceClean,
                    hasEverBeenClean
                >>
       ELSE
            CommitCurrentWorktree
    \* S11: anchor accounting on Step C — the burst does NOT count as
    \* anchor progress. The grant widens edit authorization but does not
    \* move the active coarse anchor; on a real anchor, the repair-mode
    \* staleness counter increments.
    /\ activeCoarseNode' = activeCoarseNode
    /\ cyclesInCoarseRepairMode' =
           IF activeCoarseNode' # activeCoarseNode \/ activeCoarseNode' = NoNode THEN
               0
           ELSE
               cyclesInCoarseRepairMode + 1
    \* S6: clear the grant on worker-acceptance (atomically with this
    \* dispatch in the spec abstraction). All other GlobalRepairVars
    \* are preserved.
    /\ pendingGlobalRepairGrant' = NoGlobalRepairGrant
    /\ pendingGlobalRepairRequest' = pendingGlobalRepairRequest
    /\ latestGlobalRepairAuditDeclineReason' = latestGlobalRepairAuditDeclineReason
    /\ latestGlobalRepairAuditDeclineCycle' = latestGlobalRepairAuditDeclineCycle
    /\ lastReviewerGlobalRepairRequestCycle' = lastReviewerGlobalRepairRequestCycle
    /\ everShallowCoarseClosed' = everShallowCoarseClosed
    /\ globalRepairModeEnabled' = globalRepairModeEnabled
    \* Post-advance routing latch cleared at entry to
    \* `apply_proof_review_response` (kernel engine.rs).
    /\ postAdvanceRoutingPending' = FALSE
    /\ UNCHANGED <<
           cycle,
           configuredTargets,
           approvedConfiguredTargets,
           approvedCoverage,
           approvedTargetFp,
           coarseDagNodes,
           retryOutcomeKind
       >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ ClearArtifactsAndRecordHistory

\* Reviewer-issued bulk-dismiss + optional dispatch. Subset of
\* ReviewContinueCleanup that mutates cleanup-v2 state. Kept as a
\* separate action so the cleanup-v2 reviewer surface is enumerated
\* in CoreNext.
ReviewerCleanupDismissAndDispatch ==
    /\ phase = "cleanup"
    /\ stage = "Reviewer"
    /\ inFlightRequest # NoRequest
    /\ inFlightRequest.kind = "review"
    /\ response # NoResponse
    /\ response.decision = "CONTINUE"
    /\ stage' = "Worker"
    /\ inFlightRequest' = NoRequest
    /\ response' = NoResponse
    \* `stage` reassigned above — enumerate everything else from
    \* VarsExceptCleanupV2AndRequest except `stage`.
    /\ UNCHANGED <<
        phase,
        cycle,
        attempt,
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
        nodeDifficulty,
        easyAttempts,
        reviewerComments,
        latestPaperEvidenceLanes,
        latestCorrEvidenceLanes,
        latestSoundEvidenceLanes,
        latestPaperReviewTargets,
        latestCorrReviewNodes,
        latestSoundReviewNodes,
        latestPaperPanelSplit,
        latestCorrPanelSplit,
        latestSoundPanelSplit,
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
        forceReviewAfterConeClean
       >>
    /\ UNCHANGED requestSeq
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars

\* Reviewer Done with re-audit request, when cleanupAuditRound < cap.
\* Kernel: `apply_cleanup_review_response` Done arm branches on
\* `response.cleanup_request_reaudit && cleanup_audit_round <
\* CLEANUP_AUDIT_MAX_ROUNDS && !cleanup_force_done` (engine.rs).
\* `cleanup_force_done` overrides re-audit requests — force-Done
\* mandates Phase::Complete regardless. The spec abstracts the
\* `cleanup_request_reaudit` choice as nondeterminism between this
\* action and `ReviewDoneCleanup`.
ReviewerCleanupReAudit ==
    /\ phase = "cleanup"
    /\ stage = "Reviewer"
    /\ inFlightRequest # NoRequest
    /\ inFlightRequest.kind = "review"
    /\ response # NoResponse
    /\ response.decision = "DONE"
    /\ cleanupAuditRound < 2  \* CLEANUP_AUDIT_MAX_ROUNDS
    /\ ~cleanupForceDone
    /\ stage' = "Start"
    /\ cleanupAuditRound' = cleanupAuditRound + 1
    /\ cleanupAuditBurstCount' = 0
    /\ cleanupAuditScratchpad' = ""
    /\ cleanupActiveTask' = NoTask  \* NoTask sentinel
    /\ inFlightRequest' = NoRequest
    /\ response' = NoResponse
    \* Everything except `stage` (reassigned above).
    /\ UNCHANGED <<
        phase,
        cycle,
        attempt,
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
        nodeDifficulty,
        easyAttempts,
        reviewerComments,
        latestPaperEvidenceLanes,
        latestCorrEvidenceLanes,
        latestSoundEvidenceLanes,
        latestPaperReviewTargets,
        latestCorrReviewNodes,
        latestSoundReviewNodes,
        latestPaperPanelSplit,
        latestCorrPanelSplit,
        latestSoundPanelSplit,
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
        forceReviewAfterConeClean
       >>
    /\ UNCHANGED requestSeq
    /\ UNCHANGED <<
        cleanupAuditTasks,
        cleanupConsecutiveInvalidWorkers,
        cleanupForceDone
       >>
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars

EnvStageWorkerValid ==
    /\ stage = "Worker"
    /\ inFlightRequest.kind = "worker"
    /\ inFlightRequest.cycle = cycle
    /\ response = NoResponse
    /\ LET choice ==
            \/ response' =
                WorkerResponse(
                    cycle,
                    "valid",
                    presentNodes,
                    openNodes,
                    NormalizedWorkerCoverage(
                        currentTargetClaims,
                        DefaultTargetClaimUpdate,
                        presentNodes,
                        configuredTargets
                    ),
                    corrCurrentFp,
                    paperCurrentFp,
                    soundCurrentFp,
                    currentTargetFp
                )
            \/ \E newPresent \in PresentNeighbors(presentNodes):
                /\ \E newOpen \in OpenNeighbors(newPresent, openNodes \cap newPresent):
                    /\ \E diffMap \in DifficultyUpdateNeighbors(newPresent):
                        /\ <<newPresent, newOpen>> # <<presentNodes, openNodes>>
                        /\ response' =
                            [WorkerResponse(
                                cycle,
                                "valid",
                                newPresent,
                                newOpen,
                                NormalizedWorkerCoverage(
                                    currentTargetClaims,
                                    DefaultTargetClaimUpdate,
                                    newPresent,
                                    configuredTargets
                                ),
                                corrCurrentFp,
                                paperCurrentFp,
                                soundCurrentFp,
                                currentTargetFp
                            ) EXCEPT !.difficultyMap = diffMap]
            \/ \E newCorrCurrent \in (FpNeighbors(corrCurrentFp) \ {corrCurrentFp}):
                /\ \E diffMap \in DifficultyUpdateNeighbors(presentNodes):
                    /\ response' =
                        [WorkerResponse(
                            cycle,
                            "valid",
                            presentNodes,
                            openNodes,
                            NormalizedWorkerCoverage(
                                currentTargetClaims,
                                DefaultTargetClaimUpdate,
                                presentNodes,
                                configuredTargets
                            ),
                            newCorrCurrent,
                            paperCurrentFp,
                            soundCurrentFp,
                            currentTargetFp
                        ) EXCEPT !.difficultyMap = diffMap]
            \/ \E newPaperCurrent \in (TargetFpNeighbors(paperCurrentFp) \ {paperCurrentFp}):
                /\ \E diffMap \in DifficultyUpdateNeighbors(presentNodes):
                    /\ response' =
                        [WorkerResponse(
                            cycle,
                            "valid",
                            presentNodes,
                            openNodes,
                            NormalizedWorkerCoverage(
                                currentTargetClaims,
                                DefaultTargetClaimUpdate,
                                presentNodes,
                                configuredTargets
                            ),
                            corrCurrentFp,
                            newPaperCurrent,
                            soundCurrentFp,
                            currentTargetFp
                        ) EXCEPT !.difficultyMap = diffMap]
            \/ \E newSoundCurrent \in (FpNeighbors(soundCurrentFp) \ {soundCurrentFp}):
                /\ \E diffMap \in DifficultyUpdateNeighbors(presentNodes):
                    /\ response' =
                        [WorkerResponse(
                            cycle,
                            "valid",
                            presentNodes,
                            openNodes,
                            NormalizedWorkerCoverage(
                                currentTargetClaims,
                                DefaultTargetClaimUpdate,
                                presentNodes,
                                configuredTargets
                            ),
                            corrCurrentFp,
                            paperCurrentFp,
                            newSoundCurrent,
                            currentTargetFp
                        ) EXCEPT !.difficultyMap = diffMap]
            \/ \E newTargetFp \in (FpNeighbors(currentTargetFp) \ {currentTargetFp}):
                /\ \E diffMap \in DifficultyUpdateNeighbors(presentNodes):
                    /\ response' =
                        [WorkerResponse(
                            cycle,
                            "valid",
                            presentNodes,
                            openNodes,
                            NormalizedWorkerCoverage(
                                currentTargetClaims,
                                DefaultTargetClaimUpdate,
                                presentNodes,
                                configuredTargets
                            ),
                            corrCurrentFp,
                            paperCurrentFp,
                            soundCurrentFp,
                            newTargetFp
                        ) EXCEPT !.difficultyMap = diffMap]
            \/ \E diffMap \in (DifficultyUpdateNeighbors(presentNodes) \ {DefaultDifficultyUpdate}):
                /\ response' =
                    [WorkerResponse(
                        cycle,
                        "valid",
                        presentNodes,
                        openNodes,
                        NormalizedWorkerCoverage(
                            currentTargetClaims,
                            DefaultTargetClaimUpdate,
                            presentNodes,
                            configuredTargets
                        ),
                        corrCurrentFp,
                        paperCurrentFp,
                        soundCurrentFp,
                        currentTargetFp
                    ) EXCEPT !.difficultyMap = diffMap]
       IN choice
    \* Closure gates are explicit reviewer choices, independent of difficulty.
    \* `mustCloseActive` requires the active node to close; disallowing new
    \* obligations requires newly introduced helpers to be Lean-closed.
    /\ \/ ~inFlightRequest.workerContext.mustCloseActive
       \/ /\ activeNode # NoNode
          /\ activeNode \notin response'.open
    /\ \/ inFlightRequest.workerContext.allowNewObligations
       \/ (response'.present \ presentNodes) \cap response'.open = {}
    /\ UNCHANGED
        <<
            phase,
            stage,
            cycle,
            attempt,
            invalidAttempt,
            gateKind,
            gateFromInvalidAttempt,
            activeNode,
            heldTarget,
            targetEditMode,
            proofEditMode,
            configuredTargets,
            approvedConfiguredTargets,
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
            nodeDifficulty,
            easyAttempts,
            humanInputOutstanding,
            nativeHistoryKinds,
            requestSeq,
            pendingTask,
            inFlightRequest,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED PromptCarryVars
    /\ UNCHANGED AllStructureVars
    /\ UNCHANGED LocalClosureVars

EnvStageWorkerInvalid ==
    /\ stage = "Worker"
    /\ inFlightRequest.kind = "worker"
    /\ inFlightRequest.cycle = cycle
    /\ response = NoResponse
    /\ LET choice ==
            \/ \E newPresent \in PresentNeighbors(presentNodes):
                /\ \E newOpen \in OpenNeighbors(newPresent, openNodes \cap newPresent):
                    /\ <<newPresent, newOpen>> # <<presentNodes, openNodes>>
                    /\ response' =
                        WorkerResponse(
                            cycle,
                            "invalid",
                            newPresent,
                            newOpen,
                            NormalizedWorkerCoverage(
                                currentTargetClaims,
                                DefaultTargetClaimUpdate,
                                newPresent,
                                configuredTargets
                            ),
                            corrCurrentFp,
                            paperCurrentFp,
                            soundCurrentFp,
                            currentTargetFp
                        )
            \/ \E newCorrCurrent \in (FpNeighbors(corrCurrentFp) \ {corrCurrentFp}):
                /\ response' =
                    WorkerResponse(
                        cycle,
                        "invalid",
                        presentNodes,
                        openNodes,
                        NormalizedWorkerCoverage(
                            currentTargetClaims,
                            DefaultTargetClaimUpdate,
                            presentNodes,
                            configuredTargets
                        ),
                        newCorrCurrent,
                        paperCurrentFp,
                        soundCurrentFp,
                        currentTargetFp
                    )
            \/ \E newPaperCurrent \in (TargetFpNeighbors(paperCurrentFp) \ {paperCurrentFp}):
                /\ response' =
                    WorkerResponse(
                        cycle,
                        "invalid",
                        presentNodes,
                        openNodes,
                        NormalizedWorkerCoverage(
                            currentTargetClaims,
                            DefaultTargetClaimUpdate,
                            presentNodes,
                            configuredTargets
                        ),
                        corrCurrentFp,
                        newPaperCurrent,
                        soundCurrentFp,
                        currentTargetFp
                    )
            \/ \E newSoundCurrent \in (FpNeighbors(soundCurrentFp) \ {soundCurrentFp}):
                /\ response' =
                    WorkerResponse(
                        cycle,
                        "invalid",
                        presentNodes,
                        openNodes,
                        NormalizedWorkerCoverage(
                            currentTargetClaims,
                            DefaultTargetClaimUpdate,
                            presentNodes,
                            configuredTargets
                        ),
                        corrCurrentFp,
                        paperCurrentFp,
                        newSoundCurrent,
                        currentTargetFp
                    )
            \/ \E newTargetFp \in (FpNeighbors(currentTargetFp) \ {currentTargetFp}):
                /\ response' =
                    WorkerResponse(
                        cycle,
                        "invalid",
                        presentNodes,
                        openNodes,
                        NormalizedWorkerCoverage(
                            currentTargetClaims,
                            DefaultTargetClaimUpdate,
                            presentNodes,
                            configuredTargets
                        ),
                        corrCurrentFp,
                        paperCurrentFp,
                        soundCurrentFp,
                        newTargetFp
                    )
       IN choice
    /\ UNCHANGED
        <<
            phase,
            stage,
            cycle,
            attempt,
            invalidAttempt,
            gateKind,
            gateFromInvalidAttempt,
            activeNode,
            heldTarget,
            targetEditMode,
            proofEditMode,
            configuredTargets,
            approvedConfiguredTargets,
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
            nodeDifficulty,
            easyAttempts,
            humanInputOutstanding,
            nativeHistoryKinds,
            requestSeq,
            pendingTask,
            inFlightRequest,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED PromptCarryVars
    /\ UNCHANGED AllStructureVars
    /\ UNCHANGED LocalClosureVars

EnvStageWorkerStuck ==
    /\ phase = "theorem_stating"
    /\ stage = "Worker"
    /\ inFlightRequest.kind = "worker"
    /\ inFlightRequest.cycle = cycle
    /\ response = NoResponse
    /\ response' =
        WorkerResponse(
            cycle,
            "stuck",
            presentNodes,
            openNodes,
            NormalizedWorkerCoverage(
                currentTargetClaims,
                DefaultTargetClaimUpdate,
                presentNodes,
                configuredTargets
            ),
            corrCurrentFp,
            paperCurrentFp,
            soundCurrentFp,
            currentTargetFp
        )
    /\ UNCHANGED
        <<
            phase,
            stage,
            cycle,
            attempt,
            invalidAttempt,
            gateKind,
            gateFromInvalidAttempt,
            activeNode,
            heldTarget,
            targetEditMode,
            proofEditMode,
            configuredTargets,
            approvedConfiguredTargets,
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
            nodeDifficulty,
            easyAttempts,
            humanInputOutstanding,
            nativeHistoryKinds,
            requestSeq,
            pendingTask,
            inFlightRequest,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED PromptCarryVars
    /\ UNCHANGED AllStructureVars
    /\ UNCHANGED LocalClosureVars

EnvStageWorkerMalformed ==
    /\ stage = "Worker"
    /\ inFlightRequest.kind = "worker"
    /\ inFlightRequest.cycle = cycle
    /\ response = NoResponse
    /\ response' =
        [NoResponse EXCEPT
            !.status = "malformed",
            !.kind = "worker",
            !.cycle = cycle,
            !.present = presentNodes,
            !.open = openNodes,
            !.coverage = currentCoverage,
            !.corrCurrent = corrCurrentFp,
            !.paperCurrent = paperCurrentFp,
            !.soundCurrent = soundCurrentFp,
            !.targetFp = currentTargetFp
        ]
    /\ UNCHANGED
        <<
            phase,
            stage,
            cycle,
            attempt,
            invalidAttempt,
            gateKind,
            gateFromInvalidAttempt,
            activeNode,
            heldTarget,
            targetEditMode,
            proofEditMode,
            configuredTargets,
            approvedConfiguredTargets,
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
            nodeDifficulty,
            easyAttempts,
            humanInputOutstanding,
            nativeHistoryKinds,
            requestSeq,
            pendingTask,
            inFlightRequest,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED PromptCarryVars
    /\ UNCHANGED AllStructureVars
    /\ UNCHANGED LocalClosureVars

AcceptValidWorkerTheorem ==
    /\ phase = "theorem_stating"
    /\ stage = "Worker"
    /\ response.status = "ok"
    /\ response.kind = "worker"
    /\ WorkerFinalOutcome = "valid"
    /\ response.cycle = cycle
    /\ currentNodeKinds' =
        ApplyNodeKindUpdates(currentNodeKinds, response.nodeKindMap, response.present)
    /\ currentProofNodes' =
        ApplyProofNodeUpdates(currentProofNodes, response.proofNodeMap, response.present)
    /\ currentDeps' =
        ApplyNodeSetUpdates(currentDeps, response.depMap, response.present)
    /\ currentTargetClaims' =
        ApplyTargetClaimUpdates(currentTargetClaims, response.targetClaimMap, response.present, configuredTargets)
    /\ presentNodes' = response.present
    /\ openNodes' = response.open
    \* Patch C (LOCAL_CLOSURE_IMPL_PLAN.md §7.9): TheoremStating
    \* statement edits invalidate consumers' records (§7.3 rule 6).
    \* Probes may revalidate. The post-action unverified set is chosen
    \* non-deterministically from the TypeOK-legal universe.
    /\ localClosureUnverified' \in
        LocalClosureUnverifiedNeighbors(presentNodes', openNodes', currentProofNodes')
    /\ currentCoverage' = CoverageFromClaims(currentTargetClaims', presentNodes', configuredTargets)
    /\ corrCurrentFp' = response.corrCurrent
    /\ paperCurrentFp' = response.paperCurrent
    /\ soundCurrentFp' = response.soundCurrent
    /\ currentTargetFp' = response.targetFp
    /\ nodeDifficulty' = ApplyDifficultyUpdates(nodeDifficulty, response.difficultyMap)
    /\ easyAttempts' = ApplyDifficultyAttemptResets(nodeDifficulty, easyAttempts, response.difficultyMap)
    /\ activeNode' =
        IF /\ WorkerSemanticDelta
           /\ OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            NoNode
        ELSE IF ActiveNodeLegal(phase, activeNode, presentNodes', openNodes') THEN
            activeNode
        ELSE
            NoNode
    /\ heldTarget' =
        IF /\ WorkerSemanticDelta
           /\ OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            NoNode
        ELSE IF /\ heldTarget # NoNode
           /\ heldTarget \in presentNodes'
           /\ heldTarget \in openNodes'
           /\ heldTarget \in currentProofNodes'
        THEN
            heldTarget
        ELSE
            NoNode
    /\ targetEditMode' =
        IF /\ WorkerSemanticDelta
           /\ OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            "global"
        ELSE IF activeNode' = NoNode THEN "global" ELSE targetEditMode
    /\ proofEditMode' = "local"
    /\ reviewerComments' =
        IF /\ WorkerSemanticDelta
           /\ OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            "set"
        ELSE
            reviewerComments
    /\ latestPaperEvidenceLanes' = {}
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = {}
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = FALSE
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ stage' =
        IF /\ WorkerSemanticDelta
           /\ OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            "Worker"
        ELSE IF WorkerSemanticDelta
        THEN
            "VerifyPaper"
        ELSE
            "Reviewer"
    /\ invalidAttempt' = FALSE
    /\ attempt' = attempt
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ humanInputOutstanding' = humanInputOutstanding
    /\ pendingTask' =
        IF stage' = "Worker" /\ reviewerComments' = "set" THEN
            OrphanCleanupPendingTask(NoNode, "global", currentCoverage', presentNodes')
        ELSE
            NoPendingTask
    /\ UNCHANGED
        <<
            cycle,
            phase,
            configuredTargets,
            approvedConfiguredTargets,
            committedProofNodes,
            committedNodeKinds,
            committedDeps,
            committedTargetClaims,
            committedPresentNodes,
            committedOpenNodes,
            localClosureUnverified,
            committedLocalClosureUnverified,
            committedCoverage,
            approvedCoverage,
            paperStatus,
            committedPaperCurrentFp,
            paperApprovedFp,
            substantivenessStatus,
            substantivenessCurrentFp,
            committedSubstantivenessCurrentFp,
            substantivenessApprovedFp,
            committedTargetFp,
            approvedTargetFp,
            coarseDagNodes,
            corrStatus,
            committedCorrCurrentFp,
            corrApprovedFp,
            soundStatus,
            committedSoundCurrentFp,
            soundApprovedFp,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ ClearArtifactsAndRecordHistory

AcceptInvalidWorkerTheorem ==
    /\ phase = "theorem_stating"
    /\ stage = "Worker"
    /\ response.kind = "worker"
    /\ response.cycle = cycle
    /\ (response.status = "malformed"
        \/ /\ response.status = "ok"
           /\ WorkerFinalOutcome = "invalid")
    /\ RestoreCommittedWorktree
    /\ nodeDifficulty' = nodeDifficulty
    /\ easyAttempts' = easyAttempts
    /\ activeNode' =
        IF ActiveNodeLegal(phase, activeNode, committedPresentNodes, committedOpenNodes) THEN
            activeNode
        ELSE
            NoNode
    /\ heldTarget' =
        IF /\ heldTarget # NoNode
           /\ heldTarget \in committedPresentNodes
           /\ heldTarget \in committedOpenNodes
           /\ heldTarget \in committedProofNodes
        THEN
            heldTarget
        ELSE
            NoNode
    /\ targetEditMode' =
        IF activeNode' = NoNode THEN "global" ELSE targetEditMode
    /\ proofEditMode' = "local"
    /\ reviewerComments' =
        IF /\ CurrentRetryAttempt("invalid") < WorkerRetryThreshold(phase, "invalid")
           /\ OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            "set"
        ELSE
            reviewerComments
    /\ latestPaperEvidenceLanes' = {}
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = {}
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = FALSE
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ stage' =
        IF CurrentRetryAttempt("invalid") < WorkerRetryThreshold(phase, "invalid") THEN
            "Worker"
        ELSE
            "Reviewer"
    /\ attempt' =
        IF CurrentRetryAttempt("invalid") < WorkerRetryThreshold(phase, "invalid")
        THEN
            CurrentRetryAttempt("invalid") + 1
        ELSE
            CurrentRetryAttempt("invalid")
    \* The worker retry after an invalid result remains in invalid-attempt context
    \* so prompts and downstream recovery can treat it as a retry of the same
    \* invalid episode until the reviewer resolves it.
    /\ invalidAttempt' = TRUE
    /\ retryOutcomeKind' = "invalid"
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ humanInputOutstanding' = humanInputOutstanding
    /\ pendingTask' =
        IF /\ CurrentRetryAttempt("invalid") < WorkerRetryThreshold(phase, "invalid")
           /\ OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            OrphanCleanupPendingTask(NoNode, "global", currentCoverage', presentNodes')
        ELSE
            NoPendingTask
    /\ UNCHANGED
        <<
            cycle,
            phase,
            configuredTargets,
            approvedConfiguredTargets,
            committedProofNodes,
            committedNodeKinds,
            committedDeps,
            committedTargetClaims,
            committedPresentNodes,
            committedOpenNodes,
            localClosureUnverified,
            committedLocalClosureUnverified,
            committedCoverage,
            approvedCoverage,
            paperStatus,
            committedPaperCurrentFp,
            paperApprovedFp,
            substantivenessStatus,
            substantivenessCurrentFp,
            committedSubstantivenessCurrentFp,
            substantivenessApprovedFp,
            committedTargetFp,
            approvedTargetFp,
            coarseDagNodes,
            corrStatus,
            committedCorrCurrentFp,
            corrApprovedFp,
            soundStatus,
            committedSoundCurrentFp,
            soundApprovedFp,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ ClearArtifactsAndRecordHistory

AcceptStuckWorkerTheorem ==
    /\ phase = "theorem_stating"
    /\ stage = "Worker"
    /\ response.status = "ok"
    /\ response.kind = "worker"
    /\ WorkerFinalOutcome = "stuck"
    /\ response.cycle = cycle
    /\ stage' =
        IF CurrentRetryAttempt("stuck") < WorkerRetryThreshold(phase, "stuck") THEN
            "Worker"
        ELSE
            "Reviewer"
    /\ attempt' =
        IF CurrentRetryAttempt("stuck") < WorkerRetryThreshold(phase, "stuck")
        THEN
            CurrentRetryAttempt("stuck") + 1
        ELSE
            CurrentRetryAttempt("stuck")
    \* invalidAttempt is the Invalid-retry-context flag; Stuck retries are
    \* a separate retry category and the kernel sets invalid_attempt=false
    \* for them (continue_worker_retry / begin_retry_review only set TRUE
    \* for RetryOutcomeKind::Invalid).
    /\ invalidAttempt' = FALSE
    /\ retryOutcomeKind' = "stuck"
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ reviewerComments' =
        IF /\ CurrentRetryAttempt("stuck") < WorkerRetryThreshold(phase, "stuck")
           /\ OrphanNodes(currentCoverage, presentNodes) # {}
        THEN
            "set"
        ELSE
            reviewerComments
    /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = latestPaperReviewTargets
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ humanInputOutstanding' = humanInputOutstanding
    /\ pendingTask' =
        IF /\ CurrentRetryAttempt("stuck") < WorkerRetryThreshold(phase, "stuck")
           /\ OrphanNodes(currentCoverage, presentNodes) # {}
        THEN
            OrphanCleanupPendingTask(NoNode, "global", currentCoverage, presentNodes)
        ELSE
            NoPendingTask
    /\ nodeDifficulty' = nodeDifficulty
    /\ easyAttempts' = easyAttempts
    /\ UNCHANGED
        <<
            phase,
            cycle,
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
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ ClearArtifactsAndRecordHistory

AcceptNeedsRestructureWorkerTheorem ==
    /\ phase = "theorem_stating"
    /\ stage = "Worker"
    /\ response.status = "ok"
    /\ response.kind = "worker"
    /\ WorkerFinalOutcome = "needs_restructure"
    /\ response.cycle = cycle
    /\ stage' = "Reviewer"
    /\ attempt' = CurrentRetryAttempt("needs_restructure")
    /\ invalidAttempt' = FALSE
    /\ retryOutcomeKind' = "needs_restructure"
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ reviewerComments' =
        IF /\ WorkerStructuralDelta
           /\ OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            "set"
        ELSE
            reviewerComments
    /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = latestPaperReviewTargets
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ humanInputOutstanding' = humanInputOutstanding
    /\ pendingTask' = NoPendingTask
    /\ nodeDifficulty' = nodeDifficulty
    /\ easyAttempts' = easyAttempts
    /\ UNCHANGED
        <<
            phase,
            cycle,
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
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ ClearArtifactsAndRecordHistory

AcceptValidWorkerProof ==
    /\ phase = "proof_formalization"
    /\ stage = "Worker"
    /\ response.status = "ok"
    /\ response.kind = "worker"
    /\ WorkerFinalOutcome = "valid"
    /\ response.cycle = cycle
    /\ currentNodeKinds' =
        ApplyNodeKindUpdates(currentNodeKinds, response.nodeKindMap, response.present)
    /\ currentProofNodes' = ApplyProofNodeUpdates(currentProofNodes, response.proofNodeMap, response.present)
    /\ currentDeps' = ApplyNodeSetUpdates(currentDeps, response.depMap, response.present)
    /\ currentTargetClaims' = ApplyTargetClaimUpdates(currentTargetClaims, response.targetClaimMap, response.present, configuredTargets)
    /\ presentNodes' = response.present
    /\ openNodes' = response.open
    \* Patch C (LOCAL_CLOSURE_IMPL_PLAN.md §7.0, §7.3): worker delta
    \* may transition nodes sorryd→sorry-free (records get probed),
    \* sorry-free→sorryd (records pruned), or change a helper's
    \* statement (consumers invalidated). Probe outcome is non-
    \* deterministic at TLA level; the post-action set is any
    \* TypeOK-legal closure-unverified set.
    /\ localClosureUnverified' \in
        LocalClosureUnverifiedNeighbors(presentNodes', openNodes', currentProofNodes')
    /\ currentCoverage' = CoverageFromClaims(currentTargetClaims', presentNodes', configuredTargets)
    /\ corrCurrentFp' = response.corrCurrent
    /\ paperCurrentFp' = response.paperCurrent
    /\ soundCurrentFp' = response.soundCurrent
    /\ currentTargetFp' = response.targetFp
    /\ activeNode' =
        IF ActiveNodeLegal(phase, activeNode, presentNodes', openNodes') THEN
            activeNode
        ELSE
            NoNode
    /\ nodeDifficulty' = ApplyDifficultyUpdates(nodeDifficulty, response.difficultyMap)
    /\ easyAttempts' =
        ApplyDifficultyAfterSuccess(nodeDifficulty, easyAttempts, response.difficultyMap, activeNode')
    /\ heldTarget' = NoNode
    /\ targetEditMode' = "global"
    /\ proofEditMode' =
        IF activeNode' = NoNode THEN "local" ELSE proofEditMode
    /\ reviewerComments' =
        IF /\ WorkerSemanticDelta
           /\ OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            "set"
        ELSE
            reviewerComments
    /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = latestPaperReviewTargets
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ stage' =
        IF /\ WorkerSemanticDelta
           /\ OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            "Worker"
        ELSE IF WorkerSemanticDelta
        THEN
            "VerifyPaper"
        ELSE
            "Reviewer"
    /\ invalidAttempt' = FALSE
    /\ attempt' = attempt
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ humanInputOutstanding' = humanInputOutstanding
    /\ pendingTask' =
        IF /\ WorkerSemanticDelta
           /\ OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            OrphanCleanupPendingTask(NoNode, "coarse_restructure", currentCoverage', presentNodes')
        ELSE
            NoPendingTask
    /\ UNCHANGED
        <<
            cycle,
            phase,
            configuredTargets,
            approvedConfiguredTargets,
            committedProofNodes,
            committedDeps,
            committedTargetClaims,
            committedPresentNodes,
            committedOpenNodes,
            localClosureUnverified,
            committedLocalClosureUnverified,
            committedCoverage,
            approvedCoverage,
            paperStatus,
            committedPaperCurrentFp,
            paperApprovedFp,
            substantivenessStatus,
            substantivenessCurrentFp,
            committedSubstantivenessCurrentFp,
            substantivenessApprovedFp,
            committedTargetFp,
            approvedTargetFp,
            coarseDagNodes,
            corrStatus,
            committedCorrCurrentFp,
            corrApprovedFp,
            soundStatus,
            committedSoundCurrentFp,
            soundApprovedFp,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    \* Protected-target reapproval (kernel `apply_proof_worker_response`
    \* Valid arm in engine.rs): the worker's
    \* `response.protected_semantic_change_nodes` is extended into
    \* `pending_protected_reapproval_nodes`. The spec abstracts the
    \* response field as a non-deterministic subset of presentNodes';
    \* the resulting set must remain a subset of presentNodes' so
    \* TypeOK survives the structural mutation.
    /\ \E added \in SUBSET presentNodes':
           pendingProtectedReapprovalNodes' = pendingProtectedReapprovalNodes \cup added
    /\ pendingProtectedSemanticScopeConfirmation' = pendingProtectedSemanticScopeConfirmation
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ ClearArtifactsAndRecordHistory

AcceptInvalidWorkerProofRetry ==
    /\ phase = "proof_formalization"
    /\ stage = "Worker"
    /\ response.kind = "worker"
    /\ response.cycle = cycle
    /\ (response.status = "malformed"
        \/ /\ response.status = "ok"
           /\ WorkerFinalOutcome = "invalid")
    /\ attempt < ProofInvalidReviewThreshold
    /\ RestoreCommittedWorktree
    /\ nodeDifficulty' =
        ProofFailureDifficulty(
            nodeDifficulty,
            easyAttempts,
            IF ActiveNodeLegal(phase, activeNode, committedPresentNodes, committedOpenNodes) THEN activeNode ELSE NoNode
        )
    /\ easyAttempts' =
        ProofFailureEasyAttempts(
            nodeDifficulty,
            easyAttempts,
            IF ActiveNodeLegal(phase, activeNode, committedPresentNodes, committedOpenNodes) THEN activeNode ELSE NoNode
        )
    /\ activeNode' =
        IF OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            NoNode
        ELSE IF ActiveNodeLegal(phase, activeNode, committedPresentNodes, committedOpenNodes) THEN
            activeNode
        ELSE
            NoNode
    /\ heldTarget' = NoNode
    /\ targetEditMode' = "global"
    /\ proofEditMode' =
        IF OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            "coarse_restructure"
        ELSE IF activeNode' = NoNode THEN
            "local"
        ELSE
            proofEditMode
    /\ stage' = "Worker"
    /\ invalidAttempt' = FALSE
    /\ retryOutcomeKind' = "invalid"
    /\ attempt' = CurrentRetryAttempt("invalid") + 1
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ reviewerComments' =
        IF OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            "set"
        ELSE
            reviewerComments
    /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = latestPaperReviewTargets
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ humanInputOutstanding' = humanInputOutstanding
    /\ pendingTask' =
        IF OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            OrphanCleanupPendingTask(NoNode, "coarse_restructure", currentCoverage', presentNodes')
        ELSE
            NoPendingTask
    /\ UNCHANGED
        <<
            phase,
            cycle,
            configuredTargets,
            approvedConfiguredTargets,
            committedPresentNodes,
            committedOpenNodes,
            localClosureUnverified,
            committedLocalClosureUnverified,
            committedCoverage,
            paperStatus,
            committedPaperCurrentFp,
            committedTargetFp,
            approvedCoverage,
            paperApprovedFp,
            substantivenessStatus,
            substantivenessCurrentFp,
            committedSubstantivenessCurrentFp,
            substantivenessApprovedFp,
            approvedTargetFp,
            coarseDagNodes,
            corrStatus,
            committedCorrCurrentFp,
            corrApprovedFp,
            soundStatus,
            committedSoundCurrentFp,
            soundApprovedFp,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ ClearArtifactsAndRecordHistory

AcceptInvalidWorkerProofEscalate ==
    /\ phase = "proof_formalization"
    /\ stage = "Worker"
    /\ response.kind = "worker"
    /\ response.cycle = cycle
    /\ (response.status = "malformed"
        \/ /\ response.status = "ok"
           /\ WorkerFinalOutcome = "invalid")
    /\ attempt >= ProofInvalidReviewThreshold
    /\ RestoreCommittedWorktree
    /\ nodeDifficulty' =
        ProofFailureDifficulty(
            nodeDifficulty,
            easyAttempts,
            IF ActiveNodeLegal(phase, activeNode, committedPresentNodes, committedOpenNodes) THEN activeNode ELSE NoNode
        )
    /\ easyAttempts' =
        ProofFailureEasyAttempts(
            nodeDifficulty,
            easyAttempts,
            IF ActiveNodeLegal(phase, activeNode, committedPresentNodes, committedOpenNodes) THEN activeNode ELSE NoNode
        )
    /\ activeNode' =
        IF OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            NoNode
        ELSE IF ActiveNodeLegal(phase, activeNode, committedPresentNodes, committedOpenNodes) THEN
            activeNode
        ELSE
            NoNode
    /\ heldTarget' = NoNode
    /\ targetEditMode' = "global"
    /\ proofEditMode' =
        IF OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            "coarse_restructure"
        ELSE IF activeNode' = NoNode THEN
            "local"
        ELSE
            proofEditMode
    /\ stage' = "Reviewer"
    /\ invalidAttempt' = TRUE
    /\ retryOutcomeKind' = "invalid"
    /\ attempt' = CurrentRetryAttempt("invalid")
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ reviewerComments' =
        IF OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            "set"
        ELSE
            reviewerComments
    /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = latestPaperReviewTargets
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ humanInputOutstanding' = humanInputOutstanding
    /\ pendingTask' =
        IF OrphanNodes(currentCoverage', presentNodes') # {}
        THEN
            OrphanCleanupPendingTask(NoNode, "coarse_restructure", currentCoverage', presentNodes')
        ELSE
            NoPendingTask
    /\ UNCHANGED
        <<
            phase,
            cycle,
            configuredTargets,
            approvedConfiguredTargets,
            committedPresentNodes,
            committedOpenNodes,
            localClosureUnverified,
            committedLocalClosureUnverified,
            committedCoverage,
            paperStatus,
            committedPaperCurrentFp,
            committedTargetFp,
            approvedCoverage,
            paperApprovedFp,
            substantivenessStatus,
            substantivenessCurrentFp,
            committedSubstantivenessCurrentFp,
            substantivenessApprovedFp,
            approvedTargetFp,
            coarseDagNodes,
            corrStatus,
            committedCorrCurrentFp,
            corrApprovedFp,
            soundStatus,
            committedSoundCurrentFp,
            soundApprovedFp,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ ClearArtifactsAndRecordHistory

AcceptStuckWorkerProofRetry ==
    /\ phase = "proof_formalization"
    /\ stage = "Worker"
    /\ response.status = "ok"
    /\ response.kind = "worker"
    /\ WorkerFinalOutcome = "stuck"
    /\ response.cycle = cycle
    /\ CurrentRetryAttempt("stuck") < WorkerRetryThreshold(phase, "stuck")
    /\ RestoreCommittedWorktree
    /\ activeNode' =
        IF ActiveNodeLegal(phase, activeNode, committedPresentNodes, committedOpenNodes) THEN
            activeNode
        ELSE
            NoNode
    /\ heldTarget' = NoNode
    /\ targetEditMode' = "global"
    /\ proofEditMode' =
        IF activeNode' = NoNode THEN "local" ELSE proofEditMode
    /\ stage' = "Worker"
    /\ invalidAttempt' = FALSE
    /\ retryOutcomeKind' = "stuck"
    /\ attempt' = CurrentRetryAttempt("stuck") + 1
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ reviewerComments' = reviewerComments
    /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = latestPaperReviewTargets
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ humanInputOutstanding' = humanInputOutstanding
    /\ pendingTask' = NoPendingTask
    /\ nodeDifficulty' = nodeDifficulty
    /\ easyAttempts' = easyAttempts
    /\ UNCHANGED
        <<
            phase,
            cycle,
            configuredTargets,
            approvedConfiguredTargets,
            committedProofNodes,
            committedNodeKinds,
            committedDeps,
            committedTargetClaims,
            committedPresentNodes,
            committedOpenNodes,
            localClosureUnverified,
            committedLocalClosureUnverified,
            committedCoverage,
            paperStatus,
            committedPaperCurrentFp,
            committedTargetFp,
            approvedCoverage,
            paperApprovedFp,
            substantivenessStatus,
            substantivenessCurrentFp,
            committedSubstantivenessCurrentFp,
            substantivenessApprovedFp,
            approvedTargetFp,
            coarseDagNodes,
            corrStatus,
            committedCorrCurrentFp,
            corrApprovedFp,
            soundStatus,
            committedSoundCurrentFp,
            soundApprovedFp,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ ClearArtifactsAndRecordHistory

AcceptStuckWorkerProofEscalate ==
    /\ phase = "proof_formalization"
    /\ stage = "Worker"
    /\ response.status = "ok"
    /\ response.kind = "worker"
    /\ WorkerFinalOutcome = "stuck"
    /\ response.cycle = cycle
    /\ CurrentRetryAttempt("stuck") >= WorkerRetryThreshold(phase, "stuck")
    /\ RestoreCommittedWorktree
    /\ activeNode' =
        IF ActiveNodeLegal(phase, activeNode, committedPresentNodes, committedOpenNodes) THEN
            activeNode
        ELSE
            NoNode
    /\ heldTarget' = NoNode
    /\ targetEditMode' = "global"
    /\ proofEditMode' =
        IF activeNode' = NoNode THEN "local" ELSE proofEditMode
    /\ stage' = "Reviewer"
    /\ invalidAttempt' = FALSE
    /\ retryOutcomeKind' = "stuck"
    /\ attempt' = CurrentRetryAttempt("stuck")
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ reviewerComments' = reviewerComments
    /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = latestPaperReviewTargets
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ humanInputOutstanding' = humanInputOutstanding
    /\ pendingTask' = NoPendingTask
    /\ nodeDifficulty' = nodeDifficulty
    /\ easyAttempts' = easyAttempts
    /\ UNCHANGED
        <<
            phase,
            cycle,
            configuredTargets,
            approvedConfiguredTargets,
            committedProofNodes,
            committedNodeKinds,
            committedDeps,
            committedTargetClaims,
            committedPresentNodes,
            committedOpenNodes,
            localClosureUnverified,
            committedLocalClosureUnverified,
            committedCoverage,
            paperStatus,
            committedPaperCurrentFp,
            committedTargetFp,
            approvedCoverage,
            paperApprovedFp,
            substantivenessStatus,
            substantivenessCurrentFp,
            committedSubstantivenessCurrentFp,
            substantivenessApprovedFp,
            approvedTargetFp,
            coarseDagNodes,
            corrStatus,
            committedCorrCurrentFp,
            corrApprovedFp,
            soundStatus,
            committedSoundCurrentFp,
            soundApprovedFp,
            attempt,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ ClearArtifactsAndRecordHistory

AcceptNeedsRestructureWorkerProof ==
    /\ phase = "proof_formalization"
    /\ stage = "Worker"
    /\ response.status = "ok"
    /\ response.kind = "worker"
    /\ WorkerFinalOutcome = "needs_restructure"
    /\ response.cycle = cycle
    /\ RestoreCommittedWorktree
    /\ activeNode' =
        IF ActiveNodeLegal(phase, activeNode, committedPresentNodes, committedOpenNodes) THEN
            activeNode
        ELSE
            NoNode
    /\ heldTarget' = NoNode
    /\ targetEditMode' = "global"
    /\ proofEditMode' =
        IF activeNode' = NoNode THEN "local" ELSE proofEditMode
    /\ stage' = "Reviewer"
    /\ invalidAttempt' = FALSE
    /\ retryOutcomeKind' = "needs_restructure"
    /\ attempt' = CurrentRetryAttempt("needs_restructure")
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ reviewerComments' = reviewerComments
    /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = latestPaperReviewTargets
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ humanInputOutstanding' = humanInputOutstanding
    /\ pendingTask' = NoPendingTask
    /\ nodeDifficulty' = nodeDifficulty
    /\ easyAttempts' = easyAttempts
    /\ UNCHANGED
        <<
            phase,
            cycle,
            configuredTargets,
            approvedConfiguredTargets,
            committedProofNodes,
            committedNodeKinds,
            committedDeps,
            committedTargetClaims,
            committedPresentNodes,
            committedOpenNodes,
            localClosureUnverified,
            committedLocalClosureUnverified,
            committedCoverage,
            paperStatus,
            committedPaperCurrentFp,
            committedTargetFp,
            approvedCoverage,
            paperApprovedFp,
            substantivenessStatus,
            substantivenessCurrentFp,
            committedSubstantivenessCurrentFp,
            substantivenessApprovedFp,
            approvedTargetFp,
            coarseDagNodes,
            corrStatus,
            committedCorrCurrentFp,
            corrApprovedFp,
            soundStatus,
            committedSoundCurrentFp,
            soundApprovedFp,
            attempt,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ ClearArtifactsAndRecordHistory

AcceptValidWorkerCleanup ==
    /\ phase = "cleanup"
    /\ stage = "Worker"
    /\ response.status = "ok"
    /\ response.kind = "worker"
    /\ WorkerFinalOutcome = "valid"
    /\ response.cycle = cycle
    /\ UNCHANGED
        <<
            phase,
            cycle,
            attempt,
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
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    \* Cleanup-v2 (kernel `mark_cleanup_task_completed`, engine.rs):
    \* Valid worker outcome transitions the active task Pending ->
    \* Completed (abstracted: cleanupActiveTask' = NoTask), and resets
    \* cleanupConsecutiveInvalidWorkers to 0. cleanupForceDone remains
    \* sticky once latched.
    /\ cleanupActiveTask' = NoTask
    /\ cleanupConsecutiveInvalidWorkers' = 0
    /\ UNCHANGED <<
            cleanupAuditTasks,
            cleanupAuditScratchpad,
            cleanupAuditBurstCount,
            cleanupAuditRound,
            cleanupForceDone
        >>
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ activeNode' =
        IF ActiveNodeLegal(phase, activeNode, presentNodes, openNodes) THEN
            activeNode
        ELSE
            NoNode
    /\ heldTarget' = NoNode
    /\ targetEditMode' = "global"
    /\ proofEditMode' = "local"
    /\ reviewerComments' = reviewerComments
    /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = latestPaperReviewTargets
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ humanInputOutstanding' = humanInputOutstanding
    /\ stage' = "Reviewer"
    /\ invalidAttempt' = FALSE
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ pendingTask' = NoPendingTask
    /\ nodeDifficulty' = nodeDifficulty
    /\ easyAttempts' = easyAttempts
    /\ ClearArtifactsAndRecordHistory

AcceptInvalidWorkerCleanup ==
    /\ phase = "cleanup"
    /\ stage = "Worker"
    /\ response.kind = "worker"
    /\ response.cycle = cycle
    /\ (response.status = "malformed"
        \/ /\ response.status = "ok"
           /\ WorkerFinalOutcome = "invalid")
    /\ RestoreCommittedWorktree
    /\ activeNode' =
        IF ActiveNodeLegal(phase, activeNode, committedPresentNodes, committedOpenNodes) THEN
            activeNode
        ELSE
            NoNode
    /\ heldTarget' = NoNode
    /\ targetEditMode' = "global"
    /\ proofEditMode' = "local"
    /\ stage' = "Reviewer"
    /\ invalidAttempt' = TRUE
    /\ retryOutcomeKind' = "invalid"
    /\ attempt' = CurrentRetryAttempt("invalid")
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ reviewerComments' = reviewerComments
    /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = latestPaperReviewTargets
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ humanInputOutstanding' = humanInputOutstanding
    /\ pendingTask' = NoPendingTask
    /\ nodeDifficulty' = nodeDifficulty
    /\ easyAttempts' = easyAttempts
    \* Cleanup-v2 (kernel `mark_cleanup_task_failed`, engine.rs):
    \*  - active cleanup task transitions Pending -> Failed (modeled
    \*    abstractly as cleanupActiveTask' = NoTask; the underlying
    \*    task-list mutation is not modeled at element-level fidelity).
    \*  - cleanupConsecutiveInvalidWorkers increments by 1.
    \*  - cleanupForceDone latches TRUE once the counter reaches
    \*    CLEANUP_CONSECUTIVE_INVALID_THRESHOLD (3 in the kernel).
    \*  - cleanupActiveTask clears so the next Reviewer cycle picks
    \*    a fresh task or auto-Dones.
    /\ cleanupActiveTask' = NoTask
    /\ cleanupConsecutiveInvalidWorkers' = cleanupConsecutiveInvalidWorkers + 1
    /\ cleanupForceDone' =
        IF cleanupConsecutiveInvalidWorkers + 1 >= 3 THEN
            TRUE
        ELSE
            cleanupForceDone
    /\ UNCHANGED <<
            cleanupAuditTasks,
            cleanupAuditScratchpad,
            cleanupAuditBurstCount,
            cleanupAuditRound
        >>
    /\ UNCHANGED
        <<
            phase,
            cycle,
            configuredTargets,
            approvedConfiguredTargets,
            committedProofNodes,
            committedNodeKinds,
            committedDeps,
            committedTargetClaims,
            committedPresentNodes,
            committedOpenNodes,
            localClosureUnverified,
            committedLocalClosureUnverified,
            committedCoverage,
            paperStatus,
            committedPaperCurrentFp,
            committedTargetFp,
            approvedCoverage,
            paperApprovedFp,
            substantivenessStatus,
            substantivenessCurrentFp,
            committedSubstantivenessCurrentFp,
            substantivenessApprovedFp,
            approvedTargetFp,
            coarseDagNodes,
            corrStatus,
            committedCorrCurrentFp,
            corrApprovedFp,
            soundStatus,
            committedSoundCurrentFp,
            soundApprovedFp,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ ClearArtifactsAndRecordHistory

\* The Paper request has two scenarios: target-level (carries
\* `paperVerifyTargets`) and per-node (carries `substantivenessVerifyNodes`). Per-cycle
\* scheduling ensures exactly one is non-empty per dispatch (see
\* `RequestPaperVerifyTargets` / `RequestPaperVerifyNodes`); this action's
\* response shape mirrors that bifurcation. The per-node response uses the
\* same `CorrLaneReportNeighbors` source as the target response — both
\* express "lane verdicts over a frontier as Pass/Fail/Same updates" —
\* but indexed over Nodes instead of Targets.
EnvStagePaperArtifact ==
    /\ phase \in {"theorem_stating", "proof_formalization"}
    /\ stage = "VerifyPaper"
    /\ inFlightRequest.kind = "paper"
    /\ inFlightRequest.cycle = cycle
    /\ response = NoResponse
    /\ \E laneReports \in CorrLaneReportNeighbors:
        LET targetLaneMaps == CorrTargetLaneMapsFromReports(laneReports, inFlightRequest.verifyTargets)
            nodeLaneMaps == CorrNodeLaneMapsFromReports(laneReports, inFlightRequest.substantivenessVerifyNodes)
        IN
        /\ response' =
            [
                NoResponse EXCEPT
                    !.status = "ok",
                    !.kind = "paper",
                    !.cycle = cycle,
                    !.paperLaneMaps = targetLaneMaps,
                    !.paperMap = ReconcilePaperLaneMaps(targetLaneMaps),
                    !.paperPanelSplit = PaperLaneMapsSplit(targetLaneMaps),
                    !.substantivenessLaneMaps = nodeLaneMaps,
                    !.substantivenessMap = ReconcileSubstantivenessLaneMaps(nodeLaneMaps),
                    !.substantivenessPanelSplit = SubstantivenessLaneMapsSplit(nodeLaneMaps)
            ]
        /\ UNCHANGED
            <<
                phase,
                stage,
                cycle,
                attempt,
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
                nodeDifficulty,
                easyAttempts,
                humanInputOutstanding,
                nativeHistoryKinds,
                requestSeq,
                pendingTask,
                inFlightRequest,
                cyclesSinceClean,
                hasEverBeenClean,
                forceReviewAfterConeClean
            >>
        /\ UNCHANGED CleanupV2Vars
        /\ UNCHANGED CoarseAnchorVars
        /\ UNCHANGED StuckMathAuditVars
        /\ UNCHANGED GlobalRepairVars
        /\ UNCHANGED PostAdvanceRoutingVars
        /\ UNCHANGED ProtectedReapprovalVars
        /\ UNCHANGED AuditPlanVars
        /\ UNCHANGED SoundAssessmentVars
        /\ UNCHANGED DeviationVars
        /\ UNCHANGED PromptCarryVars
        /\ UNCHANGED AllStructureVars
        /\ UNCHANGED LocalClosureVars

EnvStagePaperMalformed ==
    /\ phase \in {"theorem_stating", "proof_formalization"}
    /\ stage = "VerifyPaper"
    /\ inFlightRequest.kind = "paper"
    /\ inFlightRequest.cycle = cycle
    /\ response = NoResponse
    /\ UNCHANGED Vars

\* Target-level paper-faithfulness response (TheoremStating). Per-cycle
\* scheduling guarantees exactly one of `paperVerifyTargets` /
\* `substantivenessVerifyNodes` is non-empty per Paper request — the per-node
\* response is handled by `AcceptSubstantivenessArtifactTheorem`. The guard
\* here mirrors the kernel's `is_per_node_scenario` branch
\* (engine.rs:913) which routes to `apply_target_corr_updates` only
\* when `substantiveness_verify_nodes` is empty.
AcceptPaperArtifactTheorem ==
    /\ phase = "theorem_stating"
    /\ stage = "VerifyPaper"
    /\ response.status = "ok"
    /\ response.kind = "paper"
    /\ response.cycle = cycle
    /\ inFlightRequest.substantivenessVerifyNodes = {}
    /\ LET newPaperStatus ==
            [t \in Targets |->
                IF response.paperMap[t] = "same" THEN
                    paperStatus[t]
                ELSE
                    response.paperMap[t]
            ]
           newPaperApproved ==
            [t \in Targets |->
                IF response.paperMap[t] \in {"pass", "fail"} THEN
                    paperCurrentFp[t]
                ELSE
                    paperApprovedFp[t]
            ]
           newGlobal ==
            NodeCorrBlockersFor(corrStatus, corrCurrentFp, corrApprovedFp, presentNodes)
            \cup
            PaperBlockersFor(newPaperStatus, paperCurrentFp, newPaperApproved, configuredTargets)
            \cup
            SubstantivenessBlockersFor(substantivenessStatus, substantivenessCurrentFp, substantivenessApprovedFp, presentNodes)
            \cup
            SoundBlockersFor(soundStatus, soundCurrentFp, soundApprovedFp, presentNodes, openNodes)
           paperTargetFail ==
            \E t \in configuredTargets :
                /\ newPaperStatus[t] = "fail"
                /\ paperCurrentFp[t] = newPaperApproved[t]
           substantivenessFail ==
            \E n \in presentNodes : CurrentSubstantivenessFail(n)
           eligibleHeldTargets ==
            IF {b \in newGlobal : b.kind \in {"node_corr", "paper_faithfulness", "substantiveness"}} # {} THEN
                {}
            ELSE
                {n \in presentNodes :
                    /\ n \in currentProofNodes
                    /\ n \in openNodes
                    /\ corrStatus[n] = "pass"
                    /\ corrCurrentFp[n] = corrApprovedFp[n]
                    /\ ~(soundStatus[n] = "pass" /\ soundCurrentFp[n] = soundApprovedFp[n])}
           newHeldTarget ==
            IF eligibleHeldTargets = {} THEN
                NoNode
            ELSE IF /\ heldTarget \in eligibleHeldTargets
               /\ \A m \in eligibleHeldTargets: NodeRank[heldTarget] >= NodeRank[m]
            THEN
                heldTarget
            ELSE
                CHOOSE n \in eligibleHeldTargets:
                    \A m \in eligibleHeldTargets:
                        NodeRank[n] >= NodeRank[m]
           nonAdjudicablePaperOrSubstantivenessFrontier ==
            PostNonAdjudicablePaperOrSubstantivenessFrontier(
                newPaperStatus,
                newPaperApproved,
                substantivenessStatus,
                substantivenessApprovedFp,
                inFlightRequest.verifyTargets,
                {})
           nonAdjudicableCorrFrontier ==
            PostNonAdjudicableCorrFrontier(substantivenessStatus, substantivenessApprovedFp, {})
           nonAdjudicableSoundFrontier ==
            PostNonAdjudicableSoundFrontier(newHeldTarget, {})
       IN
            /\ paperStatus' = newPaperStatus
            /\ paperApprovedFp' = newPaperApproved
            /\ reviewerComments' = reviewerComments
            /\ latestPaperEvidenceLanes' = inFlightRequest.verifyLanes
            /\ latestCorrEvidenceLanes' = {}
            /\ latestSoundEvidenceLanes' = {}
            /\ latestPaperReviewTargets' = inFlightRequest.verifyTargets
            /\ latestCorrReviewNodes' = {}
            /\ latestSoundReviewNodes' = {}
            /\ latestPaperPanelSplit' = response.paperPanelSplit
            /\ latestCorrPanelSplit' = FALSE
            /\ latestSoundPanelSplit' = FALSE
            /\ previousPaperFindingLanes' = inFlightRequest.verifyLanes
            /\ previousCorrFindingLanes' = previousCorrFindingLanes
            /\ previousSoundFindingLanes' = previousSoundFindingLanes
            /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
            /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
            /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
            /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
            /\ heldTarget' = newHeldTarget
            \* Drain order (mirror of kernel `apply_theorem_paper_accept`,
            \* engine.rs): paper-target Fail / substantiveness Fail blockers
            \* route to Reviewer unless a non-adjudicable Unknown has a live
            \* verifier frontier; drain that verifier first. Otherwise drain
            \* target frontier first, then per-node frontier (both share
            \* VerifyPaper), then corr / sound / review. The kernel
            \* additionally enforces a max-consecutive-no-progress safety
            \* bound on the per-node drain; the TLA spec abstracts that
            \* runtime detail away — semantically the kernel re-issues until
            \* empty.
            /\ stage' =
                IF paperTargetFail \/ substantivenessFail THEN
                    IF nonAdjudicablePaperOrSubstantivenessFrontier THEN
                        "VerifyPaper"
                    ELSE IF nonAdjudicableCorrFrontier THEN
                        "VerifyCorr"
                    ELSE IF nonAdjudicableSoundFrontier THEN
                        "VerifySound"
                    ELSE
                        "Reviewer"
                ELSE IF {t \in configuredTargets :
                            ~(newPaperStatus[t] = "pass" /\ paperCurrentFp[t] = newPaperApproved[t])} # {} THEN
                    "VerifyPaper"
                ELSE IF SubstantivenessVerifyNodes # {} THEN
                    "VerifyPaper"
                ELSE IF CorrVerifyNodes # {} THEN
                    "VerifyCorr"
                ELSE IF /\ newHeldTarget # NoNode
                        /\ CurrentSoundUnknown(newHeldTarget)
                THEN
                    "VerifySound"
                ELSE
                    "Reviewer"
            /\ pendingTask' = NoPendingTask
            /\ humanInputOutstanding' = humanInputOutstanding
            /\ UNCHANGED
                <<
                    phase,
                    cycle,
                    attempt,
                    invalidAttempt,
                    retryOutcomeKind,
                    gateKind,
                    gateFromInvalidAttempt,
                    activeNode,
                    targetEditMode,
                    proofEditMode,
                    configuredTargets,
                    approvedConfiguredTargets,
                    presentNodes,
                    committedPresentNodes,
                    openNodes,
                    committedOpenNodes,
                    localClosureUnverified,
                    committedLocalClosureUnverified,
                    currentCoverage,
                    committedCoverage,
                    approvedCoverage,
                    paperCurrentFp,
                    committedPaperCurrentFp,
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
                    nodeDifficulty,
                    easyAttempts,
                    cyclesSinceClean,
                    hasEverBeenClean,
                    forceReviewAfterConeClean
                >>
            /\ UNCHANGED CleanupV2Vars
            /\ UNCHANGED CoarseAnchorVars
            /\ UNCHANGED StuckMathAuditVars
            /\ UNCHANGED GlobalRepairVars
            /\ UNCHANGED PostAdvanceRoutingVars
            /\ UNCHANGED ProtectedReapprovalVars
            /\ UNCHANGED AuditPlanVars
            /\ UNCHANGED SoundAssessmentVars
            /\ UNCHANGED DeviationVars
            /\ UNCHANGED AllStructureVars
            /\ UNCHANGED LocalClosureVars
            /\ ClearArtifactsAndRecordHistory

\* Substantiveness response (TheoremStating only). Sibling
\* of `AcceptPaperArtifactTheorem` — fires when the in-flight Paper
\* request carries the per-node frontier (`substantivenessVerifyNodes` non-empty,
\* `paperVerifyTargets` empty). Mirrors the kernel's per-node accept
\* branch (engine.rs:916-953) which writes `substantiveness_status` /
\* `substantiveness_approved_fingerprints` and accumulates per-node
\* reviewer evidence across the drain loop.
AcceptSubstantivenessArtifactTheorem ==
    /\ phase = "theorem_stating"
    /\ stage = "VerifyPaper"
    /\ response.status = "ok"
    /\ response.kind = "paper"
    /\ response.cycle = cycle
    /\ inFlightRequest.substantivenessVerifyNodes # {}
    /\ LET newSubstantivenessStatus ==
            [n \in Nodes |->
                IF response.substantivenessMap[n] = "same" THEN
                    substantivenessStatus[n]
                ELSE
                    response.substantivenessMap[n]
            ]
           newSubstantivenessApproved ==
            [n \in Nodes |->
                IF response.substantivenessMap[n] \in {"pass", "fail"} THEN
                    substantivenessCurrentFp[n]
                ELSE
                    substantivenessApprovedFp[n]
            ]
           newGlobal ==
            NodeCorrBlockersFor(corrStatus, corrCurrentFp, corrApprovedFp, presentNodes)
            \cup
            PaperBlockersFor(paperStatus, paperCurrentFp, paperApprovedFp, configuredTargets)
            \cup
            SubstantivenessBlockersFor(newSubstantivenessStatus, substantivenessCurrentFp, newSubstantivenessApproved, presentNodes)
            \cup
            SoundBlockersFor(soundStatus, soundCurrentFp, soundApprovedFp, presentNodes, openNodes)
           paperTargetFail ==
            \E t \in configuredTargets : CurrentPaperFail(t)
           substantivenessFail ==
            \E n \in presentNodes :
                /\ newSubstantivenessStatus[n] = "fail"
                /\ substantivenessCurrentFp[n] = newSubstantivenessApproved[n]
           eligibleHeldTargets ==
            IF {b \in newGlobal : b.kind \in {"node_corr", "paper_faithfulness", "substantiveness"}} # {} THEN
                {}
            ELSE
                {n \in presentNodes :
                    /\ n \in currentProofNodes
                    /\ n \in openNodes
                    /\ corrStatus[n] = "pass"
                    /\ corrCurrentFp[n] = corrApprovedFp[n]
                    /\ ~(soundStatus[n] = "pass" /\ soundCurrentFp[n] = soundApprovedFp[n])}
           newHeldTarget ==
            IF eligibleHeldTargets = {} THEN
                NoNode
            ELSE IF /\ heldTarget \in eligibleHeldTargets
               /\ \A m \in eligibleHeldTargets: NodeRank[heldTarget] >= NodeRank[m]
            THEN
                heldTarget
            ELSE
                CHOOSE n \in eligibleHeldTargets:
                    \A m \in eligibleHeldTargets:
                        NodeRank[n] >= NodeRank[m]
           remainingSubstantivenessUnknown ==
            {n \in presentNodes :
                ~(newSubstantivenessStatus[n] = "pass"
                  /\ substantivenessCurrentFp[n] = newSubstantivenessApproved[n])
                /\ ~(newSubstantivenessStatus[n] = "fail"
                     /\ substantivenessCurrentFp[n] = newSubstantivenessApproved[n])}
           nonAdjudicablePaperOrSubstantivenessFrontier ==
            PostNonAdjudicablePaperOrSubstantivenessFrontier(
                paperStatus,
                paperApprovedFp,
                newSubstantivenessStatus,
                newSubstantivenessApproved,
                {},
                inFlightRequest.substantivenessVerifyNodes)
           nonAdjudicableCorrFrontier ==
            PostNonAdjudicableCorrFrontier(newSubstantivenessStatus, newSubstantivenessApproved, {})
           nonAdjudicableSoundFrontier ==
            PostNonAdjudicableSoundFrontier(newHeldTarget, {})
       IN
            /\ substantivenessStatus' = newSubstantivenessStatus
            /\ substantivenessApprovedFp' = newSubstantivenessApproved
            /\ reviewerComments' = reviewerComments
            /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
            /\ latestCorrEvidenceLanes' = {}
            /\ latestSoundEvidenceLanes' = {}
            /\ latestPaperReviewTargets' = latestPaperReviewTargets
            /\ latestCorrReviewNodes' = {}
            /\ latestSoundReviewNodes' = {}
            /\ latestPaperPanelSplit' = latestPaperPanelSplit
            /\ latestCorrPanelSplit' = FALSE
            /\ latestSoundPanelSplit' = FALSE
            /\ previousPaperFindingLanes' = previousPaperFindingLanes
            /\ previousCorrFindingLanes' = previousCorrFindingLanes
            /\ previousSoundFindingLanes' = previousSoundFindingLanes
            /\ latestSubstantivenessEvidenceLanes' = inFlightRequest.verifyLanes
            /\ latestSubstantivenessReviewNodes' = inFlightRequest.substantivenessVerifyNodes
            /\ latestSubstantivenessPanelSplit' = response.substantivenessPanelSplit
            /\ previousSubstantivenessFindingLanes' = inFlightRequest.verifyLanes
            /\ heldTarget' = newHeldTarget
            \* Drain order: paper-target Fail (incl. previously surfaced)
            \* and substantiveness Fail route to Reviewer unless a
            \* non-adjudicable Unknown has a live verifier frontier. Otherwise
            \* drain target frontier (re-fire VerifyPaper) → per-node frontier
            \* (re-fire VerifyPaper) → corr → sound → review. Kernel safety
            \* counter is abstracted: the spec models the "right thing"
            \* semantically (re-issue until empty).
            /\ stage' =
                IF paperTargetFail \/ substantivenessFail THEN
                    IF nonAdjudicablePaperOrSubstantivenessFrontier THEN
                        "VerifyPaper"
                    ELSE IF nonAdjudicableCorrFrontier THEN
                        "VerifyCorr"
                    ELSE IF nonAdjudicableSoundFrontier THEN
                        "VerifySound"
                    ELSE
                        "Reviewer"
                ELSE IF PaperVerifyTargets # {} THEN
                    "VerifyPaper"
                ELSE IF remainingSubstantivenessUnknown # {} THEN
                    "VerifyPaper"
                ELSE IF CorrVerifyNodes # {} THEN
                    "VerifyCorr"
                ELSE IF /\ newHeldTarget # NoNode
                        /\ CurrentSoundUnknown(newHeldTarget)
                THEN
                    "VerifySound"
                ELSE
                    "Reviewer"
            /\ pendingTask' = NoPendingTask
            /\ humanInputOutstanding' = humanInputOutstanding
            /\ UNCHANGED
                <<
                    phase,
                    cycle,
                    attempt,
                    invalidAttempt,
                    retryOutcomeKind,
                    gateKind,
                    gateFromInvalidAttempt,
                    activeNode,
                    targetEditMode,
                    proofEditMode,
                    configuredTargets,
                    approvedConfiguredTargets,
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
                    substantivenessCurrentFp,
                    committedSubstantivenessCurrentFp,
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
                    nodeDifficulty,
                    easyAttempts,
                    cyclesSinceClean,
                    hasEverBeenClean,
                    forceReviewAfterConeClean
                >>
            /\ UNCHANGED CleanupV2Vars
            /\ UNCHANGED CoarseAnchorVars
            /\ UNCHANGED StuckMathAuditVars
            /\ UNCHANGED GlobalRepairVars
            /\ UNCHANGED PostAdvanceRoutingVars
            /\ UNCHANGED ProtectedReapprovalVars
            /\ UNCHANGED AuditPlanVars
            /\ UNCHANGED SoundAssessmentVars
            /\ UNCHANGED DeviationVars
            /\ UNCHANGED AllStructureVars
            /\ UNCHANGED LocalClosureVars
            /\ ClearArtifactsAndRecordHistory

\* Target-level paper response (proof-formalization). Sibling of
\* `AcceptSubstantivenessArtifactProof` — fires when the in-flight Paper
\* request carries the target frontier (per-cycle scheduling rule:
\* `paperVerifyTargets` non-empty implies `substantivenessVerifyNodes`
\* empty; both share VerifyPaper). The substantiveness lane is no longer
\* dormant in proof-formalization (helpers added by Hard restructure are
\* checked) — the per-node scenario is handled by
\* `AcceptSubstantivenessArtifactProof`.
AcceptPaperArtifactProof ==
    /\ phase = "proof_formalization"
    /\ stage = "VerifyPaper"
    /\ response.status = "ok"
    /\ response.kind = "paper"
    /\ response.cycle = cycle
    /\ inFlightRequest.substantivenessVerifyNodes = {}
    /\ LET newPaperStatus ==
            [t \in Targets |->
                IF response.paperMap[t] = "same" THEN
                    paperStatus[t]
                ELSE
                    response.paperMap[t]
            ]
           newPaperApproved ==
            [t \in Targets |->
                IF response.paperMap[t] \in {"pass", "fail"} THEN
                    paperCurrentFp[t]
                ELSE
                    paperApprovedFp[t]
            ]
           newGlobal ==
            NodeCorrBlockersFor(corrStatus, corrCurrentFp, corrApprovedFp, presentNodes)
            \cup
            PaperBlockersFor(newPaperStatus, paperCurrentFp, newPaperApproved, configuredTargets)
            \cup
            SubstantivenessBlockersFor(substantivenessStatus, substantivenessCurrentFp, substantivenessApprovedFp, presentNodes)
            \cup
            SoundBlockersFor(soundStatus, soundCurrentFp, soundApprovedFp, presentNodes, openNodes)
           paperTargetFail ==
            \E t \in configuredTargets :
                \/ currentCoverage[t] = {}
                \/ /\ newPaperStatus[t] = "fail"
                   /\ paperCurrentFp[t] = newPaperApproved[t]
           substantivenessFail ==
            \E n \in presentNodes : CurrentSubstantivenessFail(n)
           nonAdjudicablePaperOrSubstantivenessFrontier ==
            PostNonAdjudicablePaperOrSubstantivenessFrontier(
                newPaperStatus,
                newPaperApproved,
                substantivenessStatus,
                substantivenessApprovedFp,
                inFlightRequest.verifyTargets,
                {})
           nonAdjudicableCorrFrontier ==
            PostNonAdjudicableCorrFrontier(substantivenessStatus, substantivenessApprovedFp, {})
           nonAdjudicableSoundFrontier ==
            PostNonAdjudicableSoundFrontier(NoNode, {})
       IN
            /\ paperStatus' = newPaperStatus
            /\ paperApprovedFp' = newPaperApproved
            /\ reviewerComments' = reviewerComments
            /\ latestPaperEvidenceLanes' = inFlightRequest.verifyLanes
            /\ latestCorrEvidenceLanes' = {}
            /\ latestSoundEvidenceLanes' = {}
            /\ latestPaperReviewTargets' = inFlightRequest.verifyTargets
            /\ latestCorrReviewNodes' = {}
            /\ latestSoundReviewNodes' = {}
            /\ latestPaperPanelSplit' = response.paperPanelSplit
            /\ latestCorrPanelSplit' = FALSE
            /\ latestSoundPanelSplit' = FALSE
            /\ previousPaperFindingLanes' = inFlightRequest.verifyLanes
            /\ previousCorrFindingLanes' = previousCorrFindingLanes
            /\ previousSoundFindingLanes' = previousSoundFindingLanes
            /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
            /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
            /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
            /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
            /\ heldTarget' = NoNode
            /\ targetEditMode' = "global"
            /\ proofEditMode' =
                IF activeNode = NoNode THEN "local" ELSE proofEditMode
            /\ humanInputOutstanding' = humanInputOutstanding
            \* Drain order (mirror of kernel `apply_proof_paper_accept`,
            \* engine.rs): paper-target Fail OR substantiveness Fail routes
            \* to Reviewer unless a non-adjudicable Unknown has a live
            \* verifier frontier. Otherwise drain target → substantiveness →
            \* corr → cleanup-or-sound → review.
            /\ IF paperTargetFail \/ substantivenessFail THEN
                    /\ phase' = phase
                    /\ stage' =
                        IF nonAdjudicablePaperOrSubstantivenessFrontier THEN
                            "VerifyPaper"
                        ELSE IF nonAdjudicableCorrFrontier THEN
                            "VerifyCorr"
                        ELSE IF nonAdjudicableSoundFrontier THEN
                            "VerifySound"
                        ELSE
                            "Reviewer"
                    /\ attempt' = attempt
                    /\ activeNode' = activeNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
                            presentNodes,
                            committedPresentNodes,
                            openNodes,
                            committedOpenNodes,
                            localClosureUnverified,
                            committedLocalClosureUnverified,
                            currentCoverage,
                            committedCoverage,
                            approvedCoverage,
                            committedPaperCurrentFp,
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
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CoarseAnchorVars
                    /\ UNCHANGED StuckMathAuditVars
                    /\ UNCHANGED GlobalRepairVars
                    /\ UNCHANGED PostAdvanceRoutingVars
                    /\ UNCHANGED ProtectedReapprovalVars
                    /\ UNCHANGED AuditPlanVars
                    /\ UNCHANGED SoundAssessmentVars
                    /\ UNCHANGED DeviationVars
                    /\ UNCHANGED CommittedStructureVars
               ELSE IF \/ {t \in configuredTargets :
                            ~(newPaperStatus[t] = "pass" /\ paperCurrentFp[t] = newPaperApproved[t])} # {}
                       \/ SubstantivenessVerifyNodes # {} THEN
                    \* Per-cycle scheduling rule: target frontier first, then
                    \* per-node frontier; both share VerifyPaper.
                    /\ phase' = phase
                    /\ stage' = "VerifyPaper"
                    /\ attempt' = attempt
                    /\ activeNode' = activeNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
                            presentNodes,
                            committedPresentNodes,
                            openNodes,
                            committedOpenNodes,
                            localClosureUnverified,
                            committedLocalClosureUnverified,
                            currentCoverage,
                            committedCoverage,
                            approvedCoverage,
                            committedPaperCurrentFp,
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
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CoarseAnchorVars
                    /\ UNCHANGED StuckMathAuditVars
                    /\ UNCHANGED GlobalRepairVars
                    /\ UNCHANGED PostAdvanceRoutingVars
                    /\ UNCHANGED ProtectedReapprovalVars
                    /\ UNCHANGED AuditPlanVars
                    /\ UNCHANGED SoundAssessmentVars
                    /\ UNCHANGED DeviationVars
                    /\ UNCHANGED CommittedStructureVars
               ELSE IF CorrVerifyNodes # {} THEN
                    /\ phase' = phase
                    /\ stage' = "VerifyCorr"
                    /\ attempt' = attempt
                    /\ activeNode' = activeNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
                            presentNodes,
                            committedPresentNodes,
                            openNodes,
                            committedOpenNodes,
                            localClosureUnverified,
                            committedLocalClosureUnverified,
                            currentCoverage,
                            committedCoverage,
                            approvedCoverage,
                            committedPaperCurrentFp,
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
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CoarseAnchorVars
                    /\ UNCHANGED StuckMathAuditVars
                    /\ UNCHANGED GlobalRepairVars
                    /\ UNCHANGED PostAdvanceRoutingVars
                    /\ UNCHANGED ProtectedReapprovalVars
                    /\ UNCHANGED AuditPlanVars
                    /\ UNCHANGED SoundAssessmentVars
                    /\ UNCHANGED DeviationVars
                    /\ UNCHANGED CommittedStructureVars
               ELSE IF FormalizationComplete THEN
                    /\ phase' = "cleanup"
                    \* Proposal v32: cleanup phase clears coarse anchor.
                    /\ activeCoarseNode' = NoNode
                    /\ cyclesInCoarseRepairMode' = 0
                    /\ stage' = "Start"
                    /\ attempt' = 0
                    /\ activeNode' =
                        IF ActiveNodeLegal("cleanup", activeNode, presentNodes, openNodes) THEN
                            activeNode
                        ELSE
                            NoNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ CommitCurrentWorktree
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
                            presentNodes,
                            openNodes,
                            currentCoverage,
                            approvedCoverage,
                            paperStatus,
                            paperCurrentFp,
                            paperApprovedFp,
                            substantivenessStatus,
                            substantivenessCurrentFp,
                            committedSubstantivenessCurrentFp,
                            substantivenessApprovedFp,
                            currentTargetFp,
                            approvedTargetFp,
                            coarseDagNodes,
                            corrCurrentFp,
                            soundStatus,
                            soundCurrentFp,
                            soundApprovedFp,
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CurrentStructureVars
               ELSE IF SoundVerifyNodes # {} THEN
                    /\ phase' = phase
                    /\ stage' = "VerifySound"
                    /\ attempt' = attempt
                    /\ activeNode' = activeNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
                            presentNodes,
                            committedPresentNodes,
                            openNodes,
                            committedOpenNodes,
                            localClosureUnverified,
                            committedLocalClosureUnverified,
                            currentCoverage,
                            committedCoverage,
                            approvedCoverage,
                            committedPaperCurrentFp,
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
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CoarseAnchorVars
                    /\ UNCHANGED StuckMathAuditVars
                    /\ UNCHANGED GlobalRepairVars
                    /\ UNCHANGED PostAdvanceRoutingVars
                    /\ UNCHANGED ProtectedReapprovalVars
                    /\ UNCHANGED AuditPlanVars
                    /\ UNCHANGED SoundAssessmentVars
                    /\ UNCHANGED DeviationVars
                    /\ UNCHANGED CommittedStructureVars
               ELSE
                    /\ phase' = phase
                    /\ stage' = "Reviewer"
                    /\ attempt' = attempt
                    /\ activeNode' = activeNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
                            presentNodes,
                            committedPresentNodes,
                            openNodes,
                            committedOpenNodes,
                            localClosureUnverified,
                            committedLocalClosureUnverified,
                            currentCoverage,
                            committedCoverage,
                            approvedCoverage,
                            committedPaperCurrentFp,
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
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CoarseAnchorVars
                    /\ UNCHANGED StuckMathAuditVars
                    /\ UNCHANGED GlobalRepairVars
                    /\ UNCHANGED PostAdvanceRoutingVars
                    /\ UNCHANGED ProtectedReapprovalVars
                    /\ UNCHANGED AuditPlanVars
                    /\ UNCHANGED SoundAssessmentVars
                    /\ UNCHANGED DeviationVars
                    /\ UNCHANGED CommittedStructureVars
    /\ UNCHANGED CurrentStructureVars
    /\ ClearArtifactsAndRecordHistory

\* Substantiveness response (proof-formalization). Sibling of
\* `AcceptPaperArtifactProof` and `AcceptSubstantivenessArtifactTheorem` —
\* fires when the in-flight Paper request carries the per-node frontier
\* (`substantivenessVerifyNodes` non-empty, `paperVerifyTargets` empty).
\* Helper nodes added by Hard restructure participate in the
\* substantiveness lane just like theorem-stating nodes.
AcceptSubstantivenessArtifactProof ==
    /\ phase = "proof_formalization"
    /\ stage = "VerifyPaper"
    /\ response.status = "ok"
    /\ response.kind = "paper"
    /\ response.cycle = cycle
    /\ inFlightRequest.substantivenessVerifyNodes # {}
    /\ LET newSubstantivenessStatus ==
            [n \in Nodes |->
                IF response.substantivenessMap[n] = "same" THEN
                    substantivenessStatus[n]
                ELSE
                    response.substantivenessMap[n]
            ]
           newSubstantivenessApproved ==
            [n \in Nodes |->
                IF response.substantivenessMap[n] \in {"pass", "fail"} THEN
                    substantivenessCurrentFp[n]
                ELSE
                    substantivenessApprovedFp[n]
            ]
           newGlobal ==
            NodeCorrBlockersFor(corrStatus, corrCurrentFp, corrApprovedFp, presentNodes)
            \cup
            PaperBlockersFor(paperStatus, paperCurrentFp, paperApprovedFp, configuredTargets)
            \cup
            SubstantivenessBlockersFor(newSubstantivenessStatus, substantivenessCurrentFp, newSubstantivenessApproved, presentNodes)
            \cup
            SoundBlockersFor(soundStatus, soundCurrentFp, soundApprovedFp, presentNodes, openNodes)
           paperTargetFail ==
            \E t \in configuredTargets : CurrentPaperFail(t)
           substantivenessFail ==
            \E n \in presentNodes :
                /\ newSubstantivenessStatus[n] = "fail"
                /\ substantivenessCurrentFp[n] = newSubstantivenessApproved[n]
           remainingSubstantivenessUnknown ==
            {n \in presentNodes :
                ~(newSubstantivenessStatus[n] = "pass"
                  /\ substantivenessCurrentFp[n] = newSubstantivenessApproved[n])
                /\ ~(newSubstantivenessStatus[n] = "fail"
                     /\ substantivenessCurrentFp[n] = newSubstantivenessApproved[n])}
           nonAdjudicablePaperOrSubstantivenessFrontier ==
            PostNonAdjudicablePaperOrSubstantivenessFrontier(
                paperStatus,
                paperApprovedFp,
                newSubstantivenessStatus,
                newSubstantivenessApproved,
                {},
                inFlightRequest.substantivenessVerifyNodes)
           nonAdjudicableCorrFrontier ==
            PostNonAdjudicableCorrFrontier(newSubstantivenessStatus, newSubstantivenessApproved, {})
           nonAdjudicableSoundFrontier ==
            PostNonAdjudicableSoundFrontier(NoNode, {})
       IN
            /\ substantivenessStatus' = newSubstantivenessStatus
            /\ substantivenessApprovedFp' = newSubstantivenessApproved
            /\ reviewerComments' = reviewerComments
            /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
            /\ latestCorrEvidenceLanes' = {}
            /\ latestSoundEvidenceLanes' = {}
            /\ latestPaperReviewTargets' = latestPaperReviewTargets
            /\ latestCorrReviewNodes' = {}
            /\ latestSoundReviewNodes' = {}
            /\ latestPaperPanelSplit' = latestPaperPanelSplit
            /\ latestCorrPanelSplit' = FALSE
            /\ latestSoundPanelSplit' = FALSE
            /\ previousPaperFindingLanes' = previousPaperFindingLanes
            /\ previousCorrFindingLanes' = previousCorrFindingLanes
            /\ previousSoundFindingLanes' = previousSoundFindingLanes
            /\ latestSubstantivenessEvidenceLanes' = inFlightRequest.verifyLanes
            /\ latestSubstantivenessReviewNodes' = inFlightRequest.substantivenessVerifyNodes
            /\ latestSubstantivenessPanelSplit' = response.substantivenessPanelSplit
            /\ previousSubstantivenessFindingLanes' = inFlightRequest.verifyLanes
            /\ heldTarget' = NoNode
            /\ targetEditMode' = "global"
            /\ proofEditMode' =
                IF activeNode = NoNode THEN "local" ELSE proofEditMode
            /\ humanInputOutstanding' = humanInputOutstanding
            \* Drain order (mirror of kernel `apply_proof_paper_accept`):
            \* paper-target Fail OR substantiveness Fail routes to Reviewer
            \* unless a non-adjudicable Unknown has a live verifier frontier;
            \* otherwise re-fire VerifyPaper if any frontier remains, then
            \* fall through to corr / cleanup / sound / review.
            /\ IF paperTargetFail \/ substantivenessFail THEN
                    /\ phase' = phase
                    /\ stage' =
                        IF nonAdjudicablePaperOrSubstantivenessFrontier THEN
                            "VerifyPaper"
                        ELSE IF nonAdjudicableCorrFrontier THEN
                            "VerifyCorr"
                        ELSE IF nonAdjudicableSoundFrontier THEN
                            "VerifySound"
                        ELSE
                            "Reviewer"
                    /\ attempt' = attempt
                    /\ activeNode' = activeNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
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
                            substantivenessCurrentFp,
                            committedSubstantivenessCurrentFp,
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
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CoarseAnchorVars
                    /\ UNCHANGED StuckMathAuditVars
                    /\ UNCHANGED GlobalRepairVars
                    /\ UNCHANGED PostAdvanceRoutingVars
                    /\ UNCHANGED ProtectedReapprovalVars
                    /\ UNCHANGED AuditPlanVars
                    /\ UNCHANGED SoundAssessmentVars
                    /\ UNCHANGED DeviationVars
                    /\ UNCHANGED CommittedStructureVars
               ELSE IF \/ PaperVerifyTargets # {}
                       \/ remainingSubstantivenessUnknown # {} THEN
                    /\ phase' = phase
                    /\ stage' = "VerifyPaper"
                    /\ attempt' = attempt
                    /\ activeNode' = activeNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
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
                            substantivenessCurrentFp,
                            committedSubstantivenessCurrentFp,
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
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CoarseAnchorVars
                    /\ UNCHANGED StuckMathAuditVars
                    /\ UNCHANGED GlobalRepairVars
                    /\ UNCHANGED PostAdvanceRoutingVars
                    /\ UNCHANGED ProtectedReapprovalVars
                    /\ UNCHANGED AuditPlanVars
                    /\ UNCHANGED SoundAssessmentVars
                    /\ UNCHANGED DeviationVars
                    /\ UNCHANGED CommittedStructureVars
               ELSE IF CorrVerifyNodes # {} THEN
                    /\ phase' = phase
                    /\ stage' = "VerifyCorr"
                    /\ attempt' = attempt
                    /\ activeNode' = activeNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
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
                            substantivenessCurrentFp,
                            committedSubstantivenessCurrentFp,
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
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CoarseAnchorVars
                    /\ UNCHANGED StuckMathAuditVars
                    /\ UNCHANGED GlobalRepairVars
                    /\ UNCHANGED PostAdvanceRoutingVars
                    /\ UNCHANGED ProtectedReapprovalVars
                    /\ UNCHANGED AuditPlanVars
                    /\ UNCHANGED SoundAssessmentVars
                    /\ UNCHANGED DeviationVars
                    /\ UNCHANGED CommittedStructureVars
               ELSE IF FormalizationComplete THEN
                    /\ phase' = "cleanup"
                    \* Proposal v32: cleanup phase clears coarse anchor.
                    /\ activeCoarseNode' = NoNode
                    /\ cyclesInCoarseRepairMode' = 0
                    /\ stage' = "Start"
                    /\ attempt' = 0
                    /\ activeNode' =
                        IF ActiveNodeLegal("cleanup", activeNode, presentNodes, openNodes) THEN
                            activeNode
                        ELSE
                            NoNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ CommitCurrentWorktree
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
                            presentNodes,
                            openNodes,
                            currentCoverage,
                            approvedCoverage,
                            paperStatus,
                            paperCurrentFp,
                            paperApprovedFp,
                            substantivenessCurrentFp,
                            committedSubstantivenessCurrentFp,
                            currentTargetFp,
                            approvedTargetFp,
                            coarseDagNodes,
                            corrCurrentFp,
                            soundStatus,
                            soundCurrentFp,
                            soundApprovedFp,
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CurrentStructureVars
               ELSE IF SoundVerifyNodes # {} THEN
                    /\ phase' = phase
                    /\ stage' = "VerifySound"
                    /\ attempt' = attempt
                    /\ activeNode' = activeNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
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
                            substantivenessCurrentFp,
                            committedSubstantivenessCurrentFp,
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
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CoarseAnchorVars
                    /\ UNCHANGED StuckMathAuditVars
                    /\ UNCHANGED GlobalRepairVars
                    /\ UNCHANGED PostAdvanceRoutingVars
                    /\ UNCHANGED ProtectedReapprovalVars
                    /\ UNCHANGED AuditPlanVars
                    /\ UNCHANGED SoundAssessmentVars
                    /\ UNCHANGED DeviationVars
                    /\ UNCHANGED CommittedStructureVars
               ELSE
                    /\ phase' = phase
                    /\ stage' = "Reviewer"
                    /\ attempt' = attempt
                    /\ activeNode' = activeNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
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
                            substantivenessCurrentFp,
                            committedSubstantivenessCurrentFp,
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
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CoarseAnchorVars
                    /\ UNCHANGED StuckMathAuditVars
                    /\ UNCHANGED GlobalRepairVars
                    /\ UNCHANGED PostAdvanceRoutingVars
                    /\ UNCHANGED ProtectedReapprovalVars
                    /\ UNCHANGED AuditPlanVars
                    /\ UNCHANGED SoundAssessmentVars
                    /\ UNCHANGED DeviationVars
                    /\ UNCHANGED CommittedStructureVars
    /\ UNCHANGED CurrentStructureVars
    /\ ClearArtifactsAndRecordHistory

EnvStageCorrArtifact ==
    /\ phase \in {"theorem_stating", "proof_formalization"}
    /\ stage = "VerifyCorr"
    /\ inFlightRequest.kind = "corr"
    /\ inFlightRequest.cycle = cycle
    /\ response = NoResponse
    /\ \E laneReports \in CorrLaneReportNeighbors:
        LET laneMaps == CorrNodeLaneMapsFromReports(laneReports, inFlightRequest.verifyNodes)
            targetLaneMaps == CorrTargetLaneMapsFromReports(laneReports, inFlightRequest.verifyTargets)
        IN
        /\ response' =
            [
                NoResponse EXCEPT
                    !.status = "ok",
                    !.kind = "corr",
                    !.cycle = cycle,
                    !.corrLaneMaps = laneMaps,
                    !.paperLaneMaps = targetLaneMaps,
                    !.corrMap = ReconcileCorrLaneMaps(laneMaps),
                    !.paperMap = ReconcilePaperLaneMaps(targetLaneMaps),
                    !.corrPanelSplit = CorrLaneMapsSplit(laneMaps),
                    !.paperPanelSplit = PaperLaneMapsSplit(targetLaneMaps)
            ]
        /\ UNCHANGED
            <<
                phase,
                stage,
                cycle,
                attempt,
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
                nodeDifficulty,
                easyAttempts,
                humanInputOutstanding,
                nativeHistoryKinds,
                requestSeq,
                pendingTask,
                inFlightRequest,
                cyclesSinceClean,
                hasEverBeenClean,
                forceReviewAfterConeClean
            >>
        /\ UNCHANGED CleanupV2Vars
        /\ UNCHANGED CoarseAnchorVars
        /\ UNCHANGED StuckMathAuditVars
        /\ UNCHANGED GlobalRepairVars
        /\ UNCHANGED PostAdvanceRoutingVars
        /\ UNCHANGED ProtectedReapprovalVars
        /\ UNCHANGED AuditPlanVars
        /\ UNCHANGED SoundAssessmentVars
        /\ UNCHANGED DeviationVars
        /\ UNCHANGED PromptCarryVars
        /\ UNCHANGED AllStructureVars
        /\ UNCHANGED LocalClosureVars

EnvStageCorrMalformed ==
    /\ phase \in {"theorem_stating", "proof_formalization"}
    /\ stage = "VerifyCorr"
    /\ inFlightRequest.kind = "corr"
    /\ inFlightRequest.cycle = cycle
    /\ response = NoResponse
    /\ UNCHANGED Vars

AcceptCorrArtifactTheorem ==
    /\ phase = "theorem_stating"
    /\ stage = "VerifyCorr"
    /\ response.status = "ok"
    /\ response.kind = "corr"
    /\ response.cycle = cycle
    /\ LET newCorrStatus ==
            [n \in Nodes |->
                IF response.corrMap[n] = "same" THEN
                    corrStatus[n]
                ELSE
                    response.corrMap[n]
            ]
           newCorrApproved ==
            [n \in Nodes |->
                IF response.corrMap[n] \in {"pass", "fail"} THEN
                    corrCurrentFp[n]
                ELSE
                    corrApprovedFp[n]
            ]
           newPaperStatus ==
            [t \in Targets |->
                IF response.paperMap[t] = "same" THEN
                    paperStatus[t]
                ELSE
                    response.paperMap[t]
            ]
           newPaperApproved ==
            [t \in Targets |->
                IF response.paperMap[t] \in {"pass", "fail"} THEN
                    paperCurrentFp[t]
                ELSE
                    paperApprovedFp[t]
            ]
           newGlobal ==
            NodeCorrBlockersFor(newCorrStatus, corrCurrentFp, newCorrApproved, presentNodes)
            \cup
            PaperBlockersFor(newPaperStatus, paperCurrentFp, newPaperApproved, configuredTargets)
            \cup
            SubstantivenessBlockersFor(substantivenessStatus, substantivenessCurrentFp, substantivenessApprovedFp, presentNodes)
            \cup
            SoundBlockersFor(soundStatus, soundCurrentFp, soundApprovedFp, presentNodes, openNodes)
           eligibleHeldTargets ==
            {n \in presentNodes :
                /\ n \in currentProofNodes
                /\ n \in openNodes
                /\ newCorrStatus[n] = "pass"
                /\ corrCurrentFp[n] = newCorrApproved[n]
                /\ ~(soundStatus[n] = "pass" /\ soundCurrentFp[n] = soundApprovedFp[n])}
           failEscalation ==
            {b \in newGlobal : b.kind \in {"node_corr", "paper_faithfulness", "substantiveness"}} # {}
           newHeldTarget ==
            IF {b \in newGlobal : b.kind \in {"node_corr", "paper_faithfulness", "substantiveness"}} # {} THEN
                NoNode
            ELSE IF /\ heldTarget \in eligibleHeldTargets
               /\ \A m \in eligibleHeldTargets:
                    NodeRank[heldTarget] >= NodeRank[m]
            THEN
                heldTarget
            ELSE IF eligibleHeldTargets = {}
            THEN
                NoNode
            ELSE
                CHOOSE n \in eligibleHeldTargets:
                    \A m \in eligibleHeldTargets:
                        NodeRank[n] >= NodeRank[m]
           nonAdjudicablePaperOrSubstantivenessFrontier ==
            PostNonAdjudicablePaperOrSubstantivenessFrontier(
                newPaperStatus,
                newPaperApproved,
                substantivenessStatus,
                substantivenessApprovedFp,
                latestPaperReviewTargets,
                latestSubstantivenessReviewNodes)
           nonAdjudicableCorrFrontier ==
            PostNonAdjudicableCorrFrontierWithCorrMaps(
                newCorrStatus,
                newCorrApproved,
                substantivenessStatus,
                substantivenessApprovedFp,
                inFlightRequest.corrVerifyNodes)
           nonAdjudicableSoundFrontier ==
            PostNonAdjudicableSoundFrontier(newHeldTarget, {})
       IN
            /\ corrStatus' = newCorrStatus
            /\ corrApprovedFp' = newCorrApproved
            /\ paperStatus' = newPaperStatus
            /\ paperApprovedFp' = newPaperApproved
            /\ reviewerComments' = reviewerComments
            /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
            /\ latestCorrEvidenceLanes' = inFlightRequest.verifyLanes
            /\ latestSoundEvidenceLanes' = {}
            /\ latestPaperReviewTargets' = latestPaperReviewTargets
            /\ latestCorrReviewNodes' = inFlightRequest.corrVerifyNodes
            /\ latestSoundReviewNodes' = {}
            /\ latestPaperPanelSplit' = latestPaperPanelSplit
            /\ latestCorrPanelSplit' = response.corrPanelSplit
            /\ latestSoundPanelSplit' = FALSE
            /\ previousPaperFindingLanes' = previousPaperFindingLanes
            /\ previousCorrFindingLanes' = inFlightRequest.verifyLanes
            /\ previousSoundFindingLanes' = previousSoundFindingLanes
            /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
            /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
            /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
            /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
            /\ heldTarget' = newHeldTarget
            /\ stage' =
                IF failEscalation THEN
                    IF nonAdjudicablePaperOrSubstantivenessFrontier THEN
                        "VerifyPaper"
                    ELSE IF nonAdjudicableCorrFrontier THEN
                        "VerifyCorr"
                    ELSE IF nonAdjudicableSoundFrontier THEN
                        "VerifySound"
                    ELSE
                        "Reviewer"
                ELSE IF nonAdjudicablePaperOrSubstantivenessFrontier THEN
                    "VerifyPaper"
                ELSE IF nonAdjudicableCorrFrontier THEN
                    "VerifyCorr"
                ELSE IF /\ newHeldTarget # NoNode
                        /\ CurrentSoundUnknown(newHeldTarget)
                THEN
                    "VerifySound"
                ELSE
                    "Reviewer"
            /\ pendingTask' = NoPendingTask
            /\ humanInputOutstanding' = humanInputOutstanding
            /\ UNCHANGED
                <<
                    phase,
                    cycle,
                    attempt,
                    invalidAttempt,
                    gateKind,
                    gateFromInvalidAttempt,
                    activeNode,
                    targetEditMode,
                    proofEditMode,
                    configuredTargets,
                    approvedConfiguredTargets,
                    presentNodes,
                    committedPresentNodes,
                    openNodes,
                    committedOpenNodes,
                    localClosureUnverified,
                    committedLocalClosureUnverified,
                    currentCoverage,
                    committedCoverage,
                    approvedCoverage,
                    paperCurrentFp,
                    committedPaperCurrentFp,
                    currentTargetFp,
                    committedTargetFp,
                    approvedTargetFp,
                    coarseDagNodes,
                    corrCurrentFp,
                    committedCorrCurrentFp,
                    soundStatus,
                    soundCurrentFp,
                    committedSoundCurrentFp,
                    soundApprovedFp,
                    nodeDifficulty,
                    easyAttempts,
                    cyclesSinceClean,
                    hasEverBeenClean,
                    forceReviewAfterConeClean
                >>
            /\ UNCHANGED CleanupV2Vars
            /\ UNCHANGED CoarseAnchorVars
            /\ UNCHANGED StuckMathAuditVars
            /\ UNCHANGED GlobalRepairVars
            /\ UNCHANGED PostAdvanceRoutingVars
            /\ UNCHANGED ProtectedReapprovalVars
            /\ UNCHANGED AuditPlanVars
            /\ UNCHANGED SoundAssessmentVars
            /\ UNCHANGED DeviationVars
            /\ UNCHANGED AllStructureVars
            /\ UNCHANGED LocalClosureVars
            /\ ClearArtifactsAndRecordHistory

EnvStageSoundArtifact ==
    /\ phase \in {"theorem_stating", "proof_formalization"}
    /\ stage = "VerifySound"
    /\ inFlightRequest.kind = "sound"
    /\ inFlightRequest.cycle = cycle
    /\ response = NoResponse
    /\ \E laneReports \in SoundLaneReportNeighbors:
        LET laneMaps == SoundLaneMapsFromReports(laneReports, inFlightRequest.verifyNodes)
        IN
        /\ response' =
            [
                NoResponse EXCEPT
                    !.status = "ok",
                    !.kind = "sound",
                    !.cycle = cycle,
                    !.soundLaneMaps = laneMaps,
                    !.soundMap = ReconcileSoundLaneMaps(laneMaps),
                    !.soundPanelSplit = SoundLaneMapsSplit(laneMaps)
            ]
        /\ UNCHANGED
            <<
                phase,
                stage,
                cycle,
                attempt,
                invalidAttempt,
                gateKind,
                gateFromInvalidAttempt,
                activeNode,
                heldTarget,
                targetEditMode,
                proofEditMode,
                configuredTargets,
                approvedConfiguredTargets,
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
                nodeDifficulty,
                easyAttempts,
                humanInputOutstanding,
                nativeHistoryKinds,
                requestSeq,
                pendingTask,
                inFlightRequest,
                cyclesSinceClean,
                hasEverBeenClean,
                forceReviewAfterConeClean
            >>
        /\ UNCHANGED CleanupV2Vars
        /\ UNCHANGED CoarseAnchorVars
        /\ UNCHANGED StuckMathAuditVars
        /\ UNCHANGED GlobalRepairVars
        /\ UNCHANGED PostAdvanceRoutingVars
        /\ UNCHANGED ProtectedReapprovalVars
        /\ UNCHANGED AuditPlanVars
        /\ UNCHANGED SoundAssessmentVars
        /\ UNCHANGED DeviationVars
        /\ UNCHANGED PromptCarryVars
        /\ UNCHANGED AllStructureVars
        /\ UNCHANGED LocalClosureVars

EnvStageSoundMalformed ==
    /\ phase \in {"theorem_stating", "proof_formalization"}
    /\ stage = "VerifySound"
    /\ inFlightRequest.kind = "sound"
    /\ inFlightRequest.cycle = cycle
    /\ response = NoResponse
    /\ UNCHANGED Vars

AcceptSoundArtifactTheorem ==
    /\ phase = "theorem_stating"
    /\ stage = "VerifySound"
    /\ response.status = "ok"
    /\ response.kind = "sound"
    /\ response.cycle = cycle
    /\ soundStatus' =
        [n \in Nodes |->
            IF response.soundMap[n] = "same" THEN
                soundStatus[n]
            ELSE
                response.soundMap[n]
        ]
    /\ soundApprovedFp' =
        [n \in Nodes |->
            IF response.soundMap[n] \in {"pass", "fail", "structural"} THEN
                soundCurrentFp[n]
            ELSE
                soundApprovedFp[n]
        ]
    \* Mirror of kernel `apply_sound_lane_updates` (engine.rs): rich
    \* assessment status follows the legacy `soundStatus'` value with
    \* the `verifier_` prefix, except when the panel split (no consensus)
    \* — then the assessment carries `split_unknown` even on Same.
    /\ soundAssessmentStatus' =
        [n \in Nodes |->
            IF /\ response.soundMap[n] = "same"
               /\ n \in inFlightRequest.soundVerifyNodes
               /\ response.soundPanelSplit
            THEN
                "split_unknown"
            ELSE IF response.soundMap[n] = "pass" THEN
                "verifier_pass"
            ELSE IF response.soundMap[n] = "fail" THEN
                "verifier_fail"
            ELSE IF response.soundMap[n] = "structural" THEN
                "verifier_structural"
            ELSE
                soundAssessmentStatus[n]
        ]
    \* Reviewer-requested dispatch latch clears for the dispatched
    \* nodes; reverification context clears on any acceptance.
    /\ reviewerRequestedSoundVerifierNodes' =
           reviewerRequestedSoundVerifierNodes \ inFlightRequest.soundVerifyNodes
    /\ soundReverificationContext' = NoSoundReverificationContext
    /\ reviewerComments' = reviewerComments
    /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
    /\ latestCorrEvidenceLanes' = latestCorrEvidenceLanes
    /\ latestSoundEvidenceLanes' = inFlightRequest.verifyLanes
    /\ latestPaperReviewTargets' = latestPaperReviewTargets
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = latestSoundReviewNodes \cup inFlightRequest.soundVerifyNodes
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = latestCorrPanelSplit
    /\ latestSoundPanelSplit' = response.soundPanelSplit
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = inFlightRequest.verifyLanes
    /\ LET newGlobal ==
            NodeCorrBlockersFor(corrStatus, corrCurrentFp, corrApprovedFp, presentNodes)
            \cup
            PaperBlockersFor(paperStatus, paperCurrentFp, paperApprovedFp, configuredTargets)
            \cup
            SubstantivenessBlockersFor(substantivenessStatus, substantivenessCurrentFp, substantivenessApprovedFp, presentNodes)
            \cup
            SoundBlockersFor(soundStatus', soundCurrentFp, soundApprovedFp', presentNodes, openNodes)
           lowerBlockers ==
            {b \in newGlobal : b.kind \in {"node_corr", "paper_faithfulness", "substantiveness"}}
           eligibleHeldTargets ==
            IF lowerBlockers # {} THEN
                {}
            ELSE
                {n \in presentNodes :
                    /\ n \in currentProofNodes
                    /\ n \in openNodes
                    /\ corrStatus[n] = "pass"
                    /\ corrCurrentFp[n] = corrApprovedFp[n]
                    /\ ~(soundStatus'[n] = "pass" /\ soundCurrentFp[n] = soundApprovedFp'[n])}
       IN
            /\ heldTarget' =
                IF lowerBlockers # {} THEN
                    NoNode
                ELSE IF /\ heldTarget \in eligibleHeldTargets
                   /\ \A m \in eligibleHeldTargets:
                        NodeRank[heldTarget] >= NodeRank[m]
                THEN
                    heldTarget
                ELSE IF eligibleHeldTargets = {}
                THEN
                    NoNode
                ELSE
                    CHOOSE n \in eligibleHeldTargets:
                        \A m \in eligibleHeldTargets:
                            \/ NodeRank[n] > NodeRank[m]
                            \/ /\ NodeRank[n] = NodeRank[m]
                               /\ NodeOrder[n] >= NodeOrder[m]
            /\ stage' =
                IF /\ inFlightRequest.soundVerifyNodes # {}
                   /\ \A n \in inFlightRequest.soundVerifyNodes:
                        /\ soundStatus'[n] = "pass"
                        /\ soundCurrentFp[n] = soundApprovedFp'[n]
                   /\ heldTarget' # NoNode
                   /\ ~(soundStatus'[heldTarget'] = "pass"
                        /\ soundCurrentFp[heldTarget'] = soundApprovedFp'[heldTarget'])
                   /\ ~(soundStatus'[heldTarget'] \in {"fail", "structural"}
                        /\ soundCurrentFp[heldTarget'] = soundApprovedFp'[heldTarget'])
                THEN
                    "VerifySound"
                ELSE
                    "Reviewer"
    /\ pendingTask' = NoPendingTask
    /\ humanInputOutstanding' = humanInputOutstanding
    /\ nodeDifficulty' = nodeDifficulty
    /\ easyAttempts' = easyAttempts
    /\ UNCHANGED
        <<
            phase,
            cycle,
            attempt,
            invalidAttempt,
            retryOutcomeKind,
            gateKind,
            gateFromInvalidAttempt,
            activeNode,
            targetEditMode,
            proofEditMode,
            configuredTargets,
            approvedConfiguredTargets,
            presentNodes,
            committedPresentNodes,
            openNodes,
            committedOpenNodes,
            localClosureUnverified,
            committedLocalClosureUnverified,
            currentCoverage,
            committedCoverage,
            approvedCoverage,
            currentTargetFp,
            committedTargetFp,
            approvedTargetFp,
            coarseDagNodes,
            corrStatus,
            corrCurrentFp,
            committedCorrCurrentFp,
            corrApprovedFp,
            paperStatus,
            paperCurrentFp,
            committedPaperCurrentFp,
            paperApprovedFp,
            substantivenessStatus,
            substantivenessCurrentFp,
            committedSubstantivenessCurrentFp,
            substantivenessApprovedFp,
            soundCurrentFp,
            committedSoundCurrentFp,
            nodeDifficulty,
            easyAttempts,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED AllStructureVars
    /\ UNCHANGED LocalClosureVars
    /\ ClearArtifactsAndRecordHistory

AcceptCorrArtifactProof ==
    /\ phase = "proof_formalization"
    /\ stage = "VerifyCorr"
    /\ response.status = "ok"
    /\ response.kind = "corr"
    /\ response.cycle = cycle
    /\ LET newCorrStatus ==
            [n \in Nodes |->
                IF response.corrMap[n] = "same" THEN
                    corrStatus[n]
                ELSE
                    response.corrMap[n]
            ]
           newCorrApproved ==
            [n \in Nodes |->
                IF response.corrMap[n] \in {"pass", "fail"} THEN
                    corrCurrentFp[n]
                ELSE
                    corrApprovedFp[n]
            ]
           newPaperStatus ==
            [t \in Targets |->
                IF response.paperMap[t] = "same" THEN
                    paperStatus[t]
                ELSE
                    response.paperMap[t]
            ]
           newPaperApproved ==
            [t \in Targets |->
                IF response.paperMap[t] \in {"pass", "fail"} THEN
                    paperCurrentFp[t]
                ELSE
                    paperApprovedFp[t]
            ]
           newGlobal ==
            NodeCorrBlockersFor(newCorrStatus, corrCurrentFp, newCorrApproved, presentNodes)
            \cup
            PaperBlockersFor(newPaperStatus, paperCurrentFp, newPaperApproved, configuredTargets)
            \cup
            SubstantivenessBlockersFor(substantivenessStatus, substantivenessCurrentFp, substantivenessApprovedFp, presentNodes)
            \cup
            SoundBlockersFor(soundStatus, soundCurrentFp, soundApprovedFp, presentNodes, openNodes)
           failEscalation ==
            {b \in newGlobal : b.kind \in {"node_corr", "paper_faithfulness", "substantiveness"}} # {}
           nonAdjudicablePaperOrSubstantivenessFrontier ==
            PostNonAdjudicablePaperOrSubstantivenessFrontier(
                newPaperStatus,
                newPaperApproved,
                substantivenessStatus,
                substantivenessApprovedFp,
                latestPaperReviewTargets,
                latestSubstantivenessReviewNodes)
           nonAdjudicableCorrFrontier ==
            PostNonAdjudicableCorrFrontierWithCorrMaps(
                newCorrStatus,
                newCorrApproved,
                substantivenessStatus,
                substantivenessApprovedFp,
                inFlightRequest.corrVerifyNodes)
           nonAdjudicableSoundFrontier ==
            PostNonAdjudicableSoundFrontier(NoNode, {})
       IN
            /\ corrStatus' = newCorrStatus
            /\ corrApprovedFp' = newCorrApproved
            /\ paperStatus' = newPaperStatus
            /\ paperApprovedFp' = newPaperApproved
            /\ reviewerComments' = reviewerComments
            /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
            /\ latestCorrEvidenceLanes' = inFlightRequest.verifyLanes
            /\ latestSoundEvidenceLanes' = {}
            /\ latestPaperReviewTargets' = latestPaperReviewTargets
            /\ latestCorrReviewNodes' = inFlightRequest.corrVerifyNodes
            /\ latestSoundReviewNodes' = {}
            /\ latestPaperPanelSplit' = latestPaperPanelSplit
            /\ latestCorrPanelSplit' = response.corrPanelSplit
            /\ latestSoundPanelSplit' = FALSE
            /\ previousPaperFindingLanes' = previousPaperFindingLanes
            /\ previousCorrFindingLanes' = inFlightRequest.verifyLanes
            /\ previousSoundFindingLanes' = previousSoundFindingLanes
            /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
            /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
            /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
            /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
            /\ heldTarget' = NoNode
            /\ targetEditMode' = "global"
            /\ proofEditMode' =
                IF activeNode = NoNode THEN "local" ELSE proofEditMode
            /\ humanInputOutstanding' = humanInputOutstanding
            /\ IF \/ failEscalation
                  \/ nonAdjudicablePaperOrSubstantivenessFrontier
                  \/ nonAdjudicableCorrFrontier
               THEN
                    /\ phase' = phase
                    /\ stage' =
                        IF failEscalation THEN
                            IF nonAdjudicablePaperOrSubstantivenessFrontier THEN
                                "VerifyPaper"
                            ELSE IF nonAdjudicableCorrFrontier THEN
                                "VerifyCorr"
                            ELSE IF nonAdjudicableSoundFrontier THEN
                                "VerifySound"
                            ELSE
                                "Reviewer"
                        ELSE IF nonAdjudicablePaperOrSubstantivenessFrontier THEN
                            "VerifyPaper"
                        ELSE
                            "VerifyCorr"
                    /\ attempt' = attempt
                    /\ activeNode' = activeNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
                            presentNodes,
                            committedPresentNodes,
                            openNodes,
                            committedOpenNodes,
                            localClosureUnverified,
                            committedLocalClosureUnverified,
                            currentCoverage,
                            committedCoverage,
                            approvedCoverage,
                            committedPaperCurrentFp,
                            currentTargetFp,
                            committedTargetFp,
                            approvedTargetFp,
                            coarseDagNodes,
                            corrCurrentFp,
                            committedCorrCurrentFp,
                            soundStatus,
                            soundCurrentFp,
                            committedSoundCurrentFp,
                            soundApprovedFp,
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CoarseAnchorVars
                    /\ UNCHANGED StuckMathAuditVars
                    /\ UNCHANGED GlobalRepairVars
                    /\ UNCHANGED PostAdvanceRoutingVars
                    /\ UNCHANGED ProtectedReapprovalVars
                    /\ UNCHANGED AuditPlanVars
                    /\ UNCHANGED SoundAssessmentVars
                    /\ UNCHANGED DeviationVars
                    /\ UNCHANGED CommittedStructureVars
               ELSE IF FormalizationComplete THEN
                    /\ phase' = "cleanup"
                    \* Proposal v32: cleanup phase clears coarse anchor.
                    /\ activeCoarseNode' = NoNode
                    /\ cyclesInCoarseRepairMode' = 0
                    /\ stage' = "Start"
                    /\ attempt' = 0
                    /\ activeNode' =
                        IF ActiveNodeLegal("cleanup", activeNode, presentNodes, openNodes) THEN
                            activeNode
                        ELSE
                            NoNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ CommitCurrentWorktree
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
                            presentNodes,
                            openNodes,
                            currentCoverage,
                            approvedCoverage,
                            paperStatus,
                            paperCurrentFp,
                            paperApprovedFp,
                            substantivenessStatus,
                            substantivenessCurrentFp,
                            committedSubstantivenessCurrentFp,
                            substantivenessApprovedFp,
                            currentTargetFp,
                            approvedTargetFp,
                            coarseDagNodes,
                            corrCurrentFp,
                            soundStatus,
                            soundCurrentFp,
                            soundApprovedFp,
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CurrentStructureVars
               ELSE IF SoundVerifyNodes # {}
               THEN
                    /\ phase' = phase
                    /\ stage' = "VerifySound"
                    /\ attempt' = attempt
                    /\ activeNode' = activeNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
                            presentNodes,
                            committedPresentNodes,
                            openNodes,
                            committedOpenNodes,
                            localClosureUnverified,
                            committedLocalClosureUnverified,
                            currentCoverage,
                            committedCoverage,
                            approvedCoverage,
                            committedPaperCurrentFp,
                            currentTargetFp,
                            committedTargetFp,
                            approvedTargetFp,
                            coarseDagNodes,
                            corrCurrentFp,
                            committedCorrCurrentFp,
                            soundStatus,
                            soundCurrentFp,
                            committedSoundCurrentFp,
                            soundApprovedFp,
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CoarseAnchorVars
                    /\ UNCHANGED StuckMathAuditVars
                    /\ UNCHANGED GlobalRepairVars
                    /\ UNCHANGED PostAdvanceRoutingVars
                    /\ UNCHANGED ProtectedReapprovalVars
                    /\ UNCHANGED AuditPlanVars
                    /\ UNCHANGED SoundAssessmentVars
                    /\ UNCHANGED DeviationVars
                    /\ UNCHANGED CommittedStructureVars
               ELSE
                    /\ phase' = phase
                    /\ stage' = "Reviewer"
                    /\ attempt' = attempt
                    /\ activeNode' = activeNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
                            presentNodes,
                            committedPresentNodes,
                            openNodes,
                            committedOpenNodes,
                            localClosureUnverified,
                            committedLocalClosureUnverified,
                            currentCoverage,
                            committedCoverage,
                            approvedCoverage,
                            committedPaperCurrentFp,
                            currentTargetFp,
                            committedTargetFp,
                            approvedTargetFp,
                            coarseDagNodes,
                            corrCurrentFp,
                            committedCorrCurrentFp,
                            soundStatus,
                            soundCurrentFp,
                            committedSoundCurrentFp,
                            soundApprovedFp,
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CoarseAnchorVars
                    /\ UNCHANGED StuckMathAuditVars
                    /\ UNCHANGED GlobalRepairVars
                    /\ UNCHANGED PostAdvanceRoutingVars
                    /\ UNCHANGED ProtectedReapprovalVars
                    /\ UNCHANGED AuditPlanVars
                    /\ UNCHANGED SoundAssessmentVars
                    /\ UNCHANGED DeviationVars
                    /\ UNCHANGED CommittedStructureVars
    /\ UNCHANGED CurrentStructureVars
    /\ ClearArtifactsAndRecordHistory

AcceptSoundArtifactProof ==
    /\ phase = "proof_formalization"
    /\ stage = "VerifySound"
    /\ response.status = "ok"
    /\ response.kind = "sound"
    /\ response.cycle = cycle
    /\ LET newSoundStatus ==
            [n \in Nodes |->
                IF response.soundMap[n] = "same" THEN
                    soundStatus[n]
                ELSE
                    response.soundMap[n]
            ]
           newSoundApproved ==
            [n \in Nodes |->
                IF response.soundMap[n] \in {"pass", "fail", "structural"} THEN
                    soundCurrentFp[n]
                ELSE
                    soundApprovedFp[n]
            ]
           newSoundAssessment ==
            [n \in Nodes |->
                IF /\ response.soundMap[n] = "same"
                   /\ n \in inFlightRequest.soundVerifyNodes
                   /\ response.soundPanelSplit
                THEN
                    "split_unknown"
                ELSE IF response.soundMap[n] = "pass" THEN
                    "verifier_pass"
                ELSE IF response.soundMap[n] = "fail" THEN
                    "verifier_fail"
                ELSE IF response.soundMap[n] = "structural" THEN
                    "verifier_structural"
                ELSE
                    soundAssessmentStatus[n]
            ]
           newGlobal ==
            NodeCorrBlockersFor(corrStatus, corrCurrentFp, corrApprovedFp, presentNodes)
            \cup
            PaperBlockersFor(paperStatus, paperCurrentFp, paperApprovedFp, configuredTargets)
            \cup
            SoundBlockersFor(newSoundStatus, soundCurrentFp, newSoundApproved, presentNodes, openNodes)
       IN
            /\ soundStatus' = newSoundStatus
            /\ soundApprovedFp' = newSoundApproved
            \* Sound assessment store mirrors `apply_sound_lane_updates`
            \* (engine.rs); cleared per-node reviewer-requested latch
            \* and per-request reverification context (request lifecycle).
            /\ soundAssessmentStatus' = newSoundAssessment
            /\ reviewerRequestedSoundVerifierNodes' =
                   reviewerRequestedSoundVerifierNodes
                   \ inFlightRequest.soundVerifyNodes
            /\ soundReverificationContext' = NoSoundReverificationContext
            /\ reviewerComments' = reviewerComments
            /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
            /\ latestCorrEvidenceLanes' = latestCorrEvidenceLanes
            /\ latestSoundEvidenceLanes' = inFlightRequest.verifyLanes
            /\ latestPaperReviewTargets' = latestPaperReviewTargets
            /\ latestCorrReviewNodes' = latestCorrReviewNodes
            /\ latestSoundReviewNodes' = inFlightRequest.soundVerifyNodes
            /\ latestPaperPanelSplit' = latestPaperPanelSplit
            /\ latestCorrPanelSplit' = latestCorrPanelSplit
            /\ latestSoundPanelSplit' = response.soundPanelSplit
            /\ previousPaperFindingLanes' = previousPaperFindingLanes
            /\ previousCorrFindingLanes' = previousCorrFindingLanes
            /\ previousSoundFindingLanes' = inFlightRequest.verifyLanes
            /\ heldTarget' = NoNode
            /\ targetEditMode' = "global"
            /\ proofEditMode' =
                IF activeNode = NoNode THEN "local" ELSE proofEditMode
            /\ humanInputOutstanding' = humanInputOutstanding
            /\ IF /\ FormalizationComplete
                  /\ newGlobal = {}
               THEN
                    /\ phase' = "cleanup"
                    \* Proposal v32: cleanup phase clears coarse anchor.
                    /\ activeCoarseNode' = NoNode
                    /\ cyclesInCoarseRepairMode' = 0
                    /\ stage' = "Start"
                    /\ attempt' = 0
                    /\ activeNode' =
                        IF ActiveNodeLegal("cleanup", activeNode, presentNodes, openNodes) THEN
                            activeNode
                        ELSE
                            NoNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ CommitCurrentWorktree
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
                            presentNodes,
                            openNodes,
                            currentCoverage,
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
                            approvedTargetFp,
                            coarseDagNodes,
                            corrStatus,
                            corrCurrentFp,
                            corrApprovedFp,
                            soundCurrentFp,
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CurrentStructureVars
               ELSE
                    /\ phase' = phase
                    /\ stage' = "Reviewer"
                    /\ attempt' = attempt
                    /\ activeNode' = activeNode
                    /\ invalidAttempt' = FALSE
                    /\ gateKind' = "none"
                    /\ gateFromInvalidAttempt' = FALSE
                    /\ pendingTask' = NoPendingTask
                    /\ UNCHANGED <<
                            cycle,
                            configuredTargets,
                            approvedConfiguredTargets,
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
                            soundCurrentFp,
                            committedSoundCurrentFp,
                            nodeDifficulty,
                            easyAttempts,
                            forceReviewAfterConeClean
                        >>
                    /\ UNCHANGED CleanupV2Vars
                    /\ UNCHANGED CoarseAnchorVars
                    /\ UNCHANGED StuckMathAuditVars
                    /\ UNCHANGED GlobalRepairVars
                    /\ UNCHANGED PostAdvanceRoutingVars
                    /\ UNCHANGED ProtectedReapprovalVars
                    /\ UNCHANGED AuditPlanVars
                    /\ UNCHANGED DeviationVars
                    /\ UNCHANGED CommittedStructureVars
    /\ UNCHANGED CurrentStructureVars
    /\ ClearArtifactsAndRecordHistory

EnvStageReviewArtifact ==
    /\ stage = "Reviewer"
    /\ inFlightRequest.kind = "review"
    /\ inFlightRequest.cycle = cycle
    /\ response = NoResponse
    /\ \E decision \in ReviewDecisionChoices(phase, GlobalBlockers, retryOutcomeKind),
          comments \in ReviewerCommentValues,
          nextActive \in {NoNode} \cup RequestAllowedNextActiveNodes("review"),
          reset \in ReviewResetChoices(phase, retryOutcomeKind),
          tb \in IF phase \in {"theorem_stating", "proof_formalization"} THEN TaskBlockerChoices(GlobalBlockers) ELSE {{}},
          ob \in TaskBlockerChoices(RequestAllowedOverrideBlockers("review")),
          rb \in IF phase = "theorem_stating" THEN TaskBlockerChoices(RequestAllowedResetBlockers("review")) ELSE {{}},
          clearHI \in BOOLEAN,
          diffMap \in DifficultyUpdateNeighbors(presentNodes),
          nextMode \in IF phase = "theorem_stating" THEN TargetEditModes
                      ELSE IF phase = "proof_formalization" THEN ProofEditModes
                      ELSE {"cleanup"},
          ctxMode \in WorkerContextModes,
          focusRanges \in PaperFocusRangeSeqValues,
          styleHint \in WorkerWorkStyleHints,
          allowNewObligations \in BOOLEAN,
          mustCloseActive \in BOOLEAN,
          \* Pick `authorizedNodes` from the powerset of presentNodes;
          \* ReviewDecisionLegal then constrains it (subset-of-envelope,
          \* non-empty for proof Continue+Restructure/CoarseRestructure,
          \* empty for Local). Without this quantifier the env action
          \* could only ever propose `authorizedNodes = {}`, making the
          \* new explicit-restructure path unreachable in TLC. Picking
          \* from SUBSET presentNodes (rather than from
          \* ReviewScopeEnvelope(nextMode, nextActive)) avoids a
          \* forward-reference into the outer-bind variables that SANY
          \* does not resolve here; the legality gate then prunes
          \* outside-envelope choices.
          authorizedNodesChoice \in SUBSET presentNodes,
          \* global_repair_mode env-side choices (2026-06-05). The
          \* Step A request is either NoGlobalRepairRequest or a
          \* record carrying a non-empty subset of presentNodes; the
          \* Step C consume flag is a free BOOLEAN. The legality
          \* gates in ReviewDecisionLegal then prune to phase /
          \* cooldown / mutex / kill-switch consistent shapes.
          globalRepairRequestChoice \in
              {NoGlobalRepairRequest} \cup
              {[proposedExtensionNodes |-> s, dispatchedAtCycle |-> cycle] :
                  s \in (SUBSET presentNodes) \ {{}}},
          consumeGlobalRepairGrantChoice \in BOOLEAN:
        /\ IF decision = "CONTINUE" THEN
                TRUE
           ELSE
                /\ ctxMode = "resume"
                /\ focusRanges = << >>
                /\ styleHint = "none"
        /\ IF decision = "CONTINUE" /\ phase = "proof_formalization" THEN
                TRUE
           ELSE
                /\ allowNewObligations = TRUE
                /\ mustCloseActive = FALSE
        /\ response' =
            [
                NoResponse EXCEPT
                    !.status = "ok",
                    !.kind = "review",
                    !.cycle = cycle,
                    !.decision = decision,
                    !.comments = comments,
                    !.taskBlockers = tb,
                    !.overrideBlockers = ob,
                    !.resetBlockers = rb,
                    !.nextActive = nextActive,
                    !.reset = reset,
                    !.nextMode = nextMode,
                    !.difficultyMap = diffMap,
                    !.clearHumanInput = clearHI,
                    !.nextWorkerContextMode = ctxMode,
                    !.paperFocusRanges = focusRanges,
                    !.workStyleHint = styleHint,
                    !.allowNewObligations = allowNewObligations,
                    !.mustCloseActive = mustCloseActive,
                    !.authorizedNodes = authorizedNodesChoice,
                    !.globalRepairRequest = globalRepairRequestChoice,
                    !.consumeGlobalRepairGrant = consumeGlobalRepairGrantChoice
            ]
        /\ UNCHANGED
            <<
                phase,
                stage,
                cycle,
                attempt,
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
                nodeDifficulty,
                easyAttempts,
                humanInputOutstanding,
                nativeHistoryKinds,
                requestSeq,
                pendingTask,
                inFlightRequest,
                cyclesSinceClean,
                hasEverBeenClean,
                forceReviewAfterConeClean
            >>
        /\ UNCHANGED CleanupV2Vars
        /\ UNCHANGED CoarseAnchorVars
        /\ UNCHANGED StuckMathAuditVars
        /\ UNCHANGED GlobalRepairVars
        /\ UNCHANGED PostAdvanceRoutingVars
        /\ UNCHANGED ProtectedReapprovalVars
        /\ UNCHANGED AuditPlanVars
        /\ UNCHANGED SoundAssessmentVars
        /\ UNCHANGED DeviationVars
        /\ UNCHANGED PromptCarryVars
        /\ UNCHANGED AllStructureVars
        /\ UNCHANGED LocalClosureVars

ReviewContinueAfterInvalid ==
    /\ phase = "theorem_stating"
    /\ stage = "Reviewer"
    /\ retryOutcomeKind # "none"
    /\ ReviewDecisionLegal
    /\ response.decision = "CONTINUE"
    /\ LET
            resetRequested == (response.reset \in {"lastCommit", "lastClean"})
            nextNodeKinds ==
                IF resetRequested THEN committedNodeKinds ELSE currentNodeKinds
            nextProofNodes ==
                IF resetRequested THEN committedProofNodes ELSE currentProofNodes
            nextDeps ==
                IF resetRequested THEN committedDeps ELSE currentDeps
            nextTargetClaims ==
                IF resetRequested THEN committedTargetClaims ELSE currentTargetClaims
            nextPresent ==
                IF resetRequested THEN committedPresentNodes ELSE presentNodes
            nextOpen ==
                IF resetRequested THEN committedOpenNodes ELSE openNodes
            nextTargetFp ==
                IF response.reset = "lastClean" THEN DefaultFp
                ELSE IF resetRequested THEN committedTargetFp ELSE currentTargetFp
            nextCorrCurrent ==
                IF response.reset = "lastClean" THEN DefaultFp
                ELSE IF resetRequested THEN committedCorrCurrentFp ELSE corrCurrentFp
            nextSubstantivenessCurrent ==
                IF response.reset = "lastClean" THEN DefaultFp
                ELSE IF resetRequested THEN committedSubstantivenessCurrentFp ELSE substantivenessCurrentFp
            nextSoundCurrent ==
                IF response.reset = "lastClean" THEN DefaultFp
                ELSE IF resetRequested THEN committedSoundCurrentFp ELSE soundCurrentFp
            resetCorrStatus == ApplyReviewCorrStatusResets(corrStatus, response.resetBlockers)
            resetTargetCorrStatus == ApplyReviewPaperStatusResets(paperStatus, response.resetBlockers)
            resetSubstantivenessStatus == ApplyReviewSubstantivenessStatusResets(substantivenessStatus, response.resetBlockers)
            resetSoundStatus == ApplyReviewSoundStatusResets(soundStatus, response.resetBlockers)
            nextCorrStatus ==
                IF response.reset = "lastClean" THEN [n \in Nodes |-> "unknown"]
                ELSE ApplyReviewCorrStatusAdjudications(
                    resetCorrStatus,
                    response.taskBlockers
                )
            nextTargetCorrStatus ==
                IF response.reset = "lastClean" THEN [t \in Targets |-> "unknown"]
                ELSE ApplyReviewPaperStatusAdjudications(
                    resetTargetCorrStatus,
                    response.taskBlockers
                )
            nextSubstantivenessStatus ==
                IF response.reset = "lastClean" THEN [n \in Nodes |-> "unknown"]
                ELSE ApplyReviewSubstantivenessStatusAdjudications(
                    resetSubstantivenessStatus,
                    response.taskBlockers
                )
            nextSoundStatus ==
                IF response.reset = "lastClean" THEN [n \in Nodes |-> "unknown"]
                ELSE ApplyReviewSoundStatusAdjudications(
                    resetSoundStatus,
                    response.taskBlockers
                )
            nextGlobal ==
                NodeCorrBlockersFor(nextCorrStatus, nextCorrCurrent, corrApprovedFp, nextPresent)
                \cup
                PaperBlockersFor(nextTargetCorrStatus, IF resetRequested THEN committedPaperCurrentFp ELSE paperCurrentFp, paperApprovedFp, configuredTargets)
                \cup
                SubstantivenessBlockersFor(nextSubstantivenessStatus, nextSubstantivenessCurrent, substantivenessApprovedFp, nextPresent)
                \cup
                SoundBlockersFor(nextSoundStatus, nextSoundCurrent, soundApprovedFp, nextPresent, nextOpen)
            nextCorrBlockersExist ==
                \/ {n \in nextPresent : ~(nextCorrStatus[n] = "pass" /\ nextCorrCurrent[n] = corrApprovedFp[n])} # {}
                \/ {t \in configuredTargets :
                        ~(nextTargetCorrStatus[t] = "pass"
                          /\ (IF resetRequested THEN committedPaperCurrentFp ELSE paperCurrentFp)[t] = paperApprovedFp[t])} # {}
            eligibleHeldTargets ==
                {n \in nextPresent :
                    /\ n \in nextProofNodes
                    /\ n \in nextOpen
                    /\ nextCorrStatus[n] = "pass"
                    /\ nextCorrCurrent[n] = corrApprovedFp[n]
                    /\ ~(nextSoundStatus[n] = "pass" /\ nextSoundCurrent[n] = soundApprovedFp[n])}
            nextHeld ==
                IF nextCorrBlockersExist THEN
                    NoNode
                ELSE IF /\ heldTarget \in eligibleHeldTargets
                   /\ \A m \in eligibleHeldTargets:
                        NodeRank[heldTarget] >= NodeRank[m]
                THEN
                    heldTarget
                ELSE IF eligibleHeldTargets = {} THEN
                    NoNode
                ELSE
                    CHOOSE n \in eligibleHeldTargets:
                        \A m \in eligibleHeldTargets:
                            \/ NodeRank[n] > NodeRank[m]
                            \/ /\ NodeRank[n] = NodeRank[m]
                               /\ NodeOrder[n] >= NodeOrder[m]
            chosenActive ==
                IF response.nextActive # NoNode THEN
                    response.nextActive
                ELSE
                    activeNode
            nextStage ==
                IF \/ {n \in nextPresent : ~(nextCorrStatus[n] \in {"pass", "fail"} /\ nextCorrCurrent[n] = corrApprovedFp[n])} # {}
                   \/ {t \in configuredTargets :
                           ~(nextTargetCorrStatus[t] \in {"pass", "fail"}
                             /\ (IF resetRequested THEN committedPaperCurrentFp ELSE paperCurrentFp)[t] = paperApprovedFp[t])} # {}
                THEN
                    "VerifyCorr"
                ELSE IF /\ nextHeld # NoNode
                        /\ ~(nextSoundStatus[nextHeld] \in {"pass", "fail"}
                              /\ nextSoundCurrent[nextHeld] = soundApprovedFp[nextHeld])
                THEN
                    "VerifySound"
                ELSE
                    "Worker"
       IN
            /\ currentNodeKinds' = nextNodeKinds
            /\ currentProofNodes' = nextProofNodes
            /\ currentDeps' = nextDeps
            /\ currentTargetClaims' = nextTargetClaims
            /\ presentNodes' = nextPresent
            /\ openNodes' = nextOpen
            \* Patch C (LOCAL_CLOSURE_IMPL_PLAN.md §7.7, §7.8): a Review
            \* with `reset = lastClean` restores closure live tier from
            \* the committed mirror (which proxies the kernel's
            \* `last_clean_*` mirror at this abstraction level); a
            \* without-reset Continue may invalidate consumers via
            \* whatever structural delta the worker queued (rare in
            \* Review, but possible). Non-deterministic over the
            \* TypeOK universe in the no-reset case.
            /\ localClosureUnverified' \in
                IF resetRequested THEN
                    {committedLocalClosureUnverified \cap ((nextPresent \cap nextProofNodes) \ nextOpen)}
                ELSE
                    LocalClosureUnverifiedNeighbors(nextPresent, nextOpen, nextProofNodes)
            /\ currentCoverage' = CoverageFromClaims(currentTargetClaims', presentNodes', configuredTargets)
            /\ paperStatus' = nextTargetCorrStatus
            /\ paperApprovedFp' = ApplyReviewPaperApprovedFpAdjudications(
                   ApplyReviewPaperApprovedFpResets(paperApprovedFp, response.resetBlockers),
                   IF resetRequested THEN committedPaperCurrentFp ELSE paperCurrentFp,
                   response.taskBlockers)
            /\ paperCurrentFp' =
                IF resetRequested THEN committedPaperCurrentFp ELSE paperCurrentFp
            /\ substantivenessStatus' = nextSubstantivenessStatus
            /\ substantivenessApprovedFp' = ApplyReviewSubstantivenessApprovedFpAdjudications(
                   ApplyReviewSubstantivenessApprovedFpResets(substantivenessApprovedFp, response.resetBlockers),
                   nextSubstantivenessCurrent, response.taskBlockers)
            /\ substantivenessCurrentFp' = nextSubstantivenessCurrent
            /\ currentTargetFp' = nextTargetFp
            /\ corrStatus' = nextCorrStatus
            /\ corrApprovedFp' = ApplyReviewCorrApprovedFpAdjudications(
                   ApplyReviewCorrApprovedFpResets(corrApprovedFp, response.resetBlockers),
                   nextCorrCurrent, response.taskBlockers)
            /\ corrCurrentFp' = nextCorrCurrent
            /\ soundStatus' = nextSoundStatus
            /\ soundApprovedFp' = ApplyReviewSoundApprovedFpAdjudications(
                   ApplyReviewSoundApprovedFpResets(soundApprovedFp, response.resetBlockers),
                   nextSoundCurrent, response.taskBlockers)
            /\ soundCurrentFp' = nextSoundCurrent
            \* Sound assessment store mirror (same shape as the proof-
            \* formalization Continue arm: resets then adjudications,
            \* LastClean clears to fresh_unknown).
            /\ soundAssessmentStatus' =
                   IF response.reset = "lastClean" THEN DefaultSoundAssessmentStatus
                   ELSE ApplyReviewSoundAssessmentAdjudications(
                       ApplyReviewSoundAssessmentResets(soundAssessmentStatus,
                                                        response.resetBlockers),
                       response.taskBlockers)
            /\ reviewerRequestedSoundVerifierNodes' =
                   IF response.reset = "lastClean" THEN {}
                   ELSE IF response.reset = NoCheckpoint THEN
                       reviewerRequestedSoundVerifierNodes
                           \cup (response.requestSoundVerifierNodes \cap presentNodes)
                   ELSE
                       reviewerRequestedSoundVerifierNodes
            /\ soundReverificationContext' = NoSoundReverificationContext
            /\ activeNode' =
                IF ActiveNodeLegal(phase, chosenActive, presentNodes', openNodes') THEN chosenActive ELSE NoNode
            /\ heldTarget' = nextHeld
            /\ targetEditMode' =
                IF activeNode' = NoNode THEN "global" ELSE response.nextMode
            /\ proofEditMode' = "local"
            /\ reviewerComments' = response.comments
            /\ latestPaperEvidenceLanes' = {}
            /\ latestCorrEvidenceLanes' = {}
            /\ latestSoundEvidenceLanes' = {}
            /\ latestPaperReviewTargets' = {}
            /\ latestCorrReviewNodes' = {}
            /\ latestSoundReviewNodes' = {}
            /\ latestPaperPanelSplit' = latestPaperPanelSplit
            /\ latestCorrPanelSplit' = FALSE
            /\ latestSoundPanelSplit' = FALSE
            /\ previousPaperFindingLanes' = previousPaperFindingLanes
            /\ previousCorrFindingLanes' = previousCorrFindingLanes
            /\ previousSoundFindingLanes' = previousSoundFindingLanes
            /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
            /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
            /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
            /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
            /\ nodeDifficulty' = ApplyDifficultyUpdates(nodeDifficulty, response.difficultyMap)
            /\ easyAttempts' = ApplyDifficultyAttemptResets(nodeDifficulty, easyAttempts, response.difficultyMap)
            /\ humanInputOutstanding' =
                IF response.clearHumanInput THEN FALSE ELSE humanInputOutstanding
            /\ pendingTask' =
                IF nextStage = "Worker" THEN
                    [
                        taskBlockers |-> response.taskBlockers \cap nextGlobal,
                        node |-> activeNode',
                        mode |-> targetEditMode',
                        orphanCleanupNodes |-> {},
                        nextWorkerContextMode |-> response.nextWorkerContextMode,
                        paperFocusRanges |-> response.paperFocusRanges,
                        workStyleHint |-> response.workStyleHint,
                        allowNewObligations |-> TRUE,
                        mustCloseActive |-> FALSE,
                        authorizedNodes |-> {},
                        consumedGlobalRepairGrant |-> FALSE
                    ]
                ELSE
                    NoPendingTask
            /\ stage' = nextStage
            /\ attempt' = 1
            /\ invalidAttempt' = FALSE
            /\ gateKind' = "none"
            /\ gateFromInvalidAttempt' = FALSE
            /\ UNCHANGED
                <<
                    phase,
                    cycle,
                    configuredTargets,
                    approvedConfiguredTargets,
                    committedPresentNodes,
                    committedOpenNodes,
                    localClosureUnverified,
                    committedLocalClosureUnverified,
                    committedCoverage,
                    approvedCoverage,
                    committedPaperCurrentFp,
                    committedSubstantivenessCurrentFp,
                    committedTargetFp,
                    approvedTargetFp,
                    coarseDagNodes,
                    committedCorrCurrentFp,
                    committedSoundCurrentFp,
                    cyclesSinceClean,
                    hasEverBeenClean,
                    forceReviewAfterConeClean
                >>
            /\ UNCHANGED CleanupV2Vars
            /\ UNCHANGED CoarseAnchorVars
            /\ UNCHANGED StuckMathAuditVars
            /\ UNCHANGED GlobalRepairVars
            /\ UNCHANGED PostAdvanceRoutingVars
            /\ UNCHANGED ProtectedReapprovalVars
            /\ UNCHANGED AuditPlanVars
            /\ UNCHANGED DeviationVars
            /\ UNCHANGED CommittedStructureVars
            /\ ClearArtifactsAndRecordHistory

\* Mirror of kernel `apply_theorem_review_response` NEED_INPUT arm in
\* engine.rs (TheoremStating + retry case calling
\* `route_need_input_to_auditor`). Reviewer NEED_INPUT no longer
\* dispatches HumanGate directly: it activates the StuckMathAudit
\* latch and issues a `stuck_math_audit` request. The follow-up
\* AcceptStuckMathAudit* actions decide whether to surface a
\* HumanGate (`confirm_need_input`) or route back to Reviewer (audit
\* judged escalation unnecessary) or retry-then-escalate
\* (`retry_or_transition_stuck_math_audit_to_reviewer`).
ReviewNeedInputAfterInvalid ==
    /\ phase = "theorem_stating"
    /\ stage = "Reviewer"
    /\ retryOutcomeKind # "none"
    /\ ReviewDecisionLegal
    /\ response.decision = "NEED_INPUT"
    /\ LET
            nextCorrStatus == ApplyReviewCorrStatusResets(corrStatus, response.resetBlockers)
            nextTargetCorrStatus == ApplyReviewPaperStatusResets(paperStatus, response.resetBlockers)
            nextSubstantivenessStatus == ApplyReviewSubstantivenessStatusResets(substantivenessStatus, response.resetBlockers)
            nextSoundStatus == ApplyReviewSoundStatusResets(soundStatus, response.resetBlockers)
            nextGlobal ==
                NodeCorrBlockersFor(nextCorrStatus, corrCurrentFp, corrApprovedFp, presentNodes)
                \cup
                PaperBlockersFor(nextTargetCorrStatus, paperCurrentFp, paperApprovedFp, configuredTargets)
                \cup
                SubstantivenessBlockersFor(nextSubstantivenessStatus, substantivenessCurrentFp, substantivenessApprovedFp, presentNodes)
                \cup
                SoundBlockersFor(nextSoundStatus, soundCurrentFp, soundApprovedFp, presentNodes, openNodes)
       IN
            \* Mirror of `route_need_input_to_auditor`: switch to
            \* `Stage::StuckMathAudit`, clear gate (HumanGate is gated
            \* later by `apply_stuck_math_audit_response`), activate
            \* the latch and pin gateFromInvalidAttempt into the
            \* need-input audit context.
            /\ stage' = "StuckMathAudit"
            /\ gateKind' = "none"
            /\ gateFromInvalidAttempt' = FALSE
            /\ reviewerComments' = ""
            /\ latestPaperEvidenceLanes' = {}
            /\ latestCorrEvidenceLanes' = {}
            /\ latestSoundEvidenceLanes' = {}
            /\ latestPaperReviewTargets' = {}
            /\ latestCorrReviewNodes' = {}
            /\ latestSoundReviewNodes' = {}
            /\ latestPaperPanelSplit' = latestPaperPanelSplit
            /\ latestCorrPanelSplit' = FALSE
            /\ latestSoundPanelSplit' = FALSE
            /\ previousPaperFindingLanes' = previousPaperFindingLanes
            /\ previousCorrFindingLanes' = previousCorrFindingLanes
            /\ previousSoundFindingLanes' = previousSoundFindingLanes
            /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
            /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
            /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
            /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
            /\ paperStatus' = nextTargetCorrStatus
            /\ paperApprovedFp' = ApplyReviewPaperApprovedFpResets(paperApprovedFp, response.resetBlockers)
            /\ substantivenessStatus' = nextSubstantivenessStatus
            /\ substantivenessApprovedFp' = ApplyReviewSubstantivenessApprovedFpResets(substantivenessApprovedFp, response.resetBlockers)
            /\ corrStatus' = nextCorrStatus
            /\ corrApprovedFp' = ApplyReviewCorrApprovedFpResets(corrApprovedFp, response.resetBlockers)
            /\ soundStatus' = nextSoundStatus
            /\ soundApprovedFp' = ApplyReviewSoundApprovedFpResets(soundApprovedFp, response.resetBlockers)
            \* Sound assessment store mirror. NeedInput is sticky-fail
            \* (no task_blockers); LastClean clears to fresh_unknown.
            /\ soundAssessmentStatus' =
                   IF response.reset = "lastClean" THEN DefaultSoundAssessmentStatus
                   ELSE ApplyReviewSoundAssessmentResets(soundAssessmentStatus,
                                                         response.resetBlockers)
            /\ reviewerRequestedSoundVerifierNodes' =
                   IF response.reset = "lastClean" THEN {}
                   ELSE reviewerRequestedSoundVerifierNodes
            /\ soundReverificationContext' = NoSoundReverificationContext
            /\ nodeDifficulty' = ApplyDifficultyUpdates(nodeDifficulty, response.difficultyMap)
            /\ easyAttempts' = ApplyDifficultyAttemptResets(nodeDifficulty, easyAttempts, response.difficultyMap)
            \* StuckMathAudit latch activation: mirrors
            \* `route_need_input_to_auditor` in engine.rs setting
            \* `stuck_math_audit.active = true`, recording the
            \* `NeedInputAuditContext` (with `gate_from_invalid_attempt`
            \* derived from `retry_outcome_kind == Invalid`), zeroing
            \* the burst-retry counter and noting the dispatch cycle.
            /\ stuckMathAuditActive' = TRUE
            /\ stuckMathAuditNeedInputAudit' =
                [gateFromInvalidAttempt |-> (retryOutcomeKind = "invalid")]
            /\ stuckMathAuditBurstRetryCount' = 0
            /\ lastStuckMathAuditDispatchedCycle' = cycle
    /\ humanInputOutstanding' =
        IF response.clearHumanInput THEN FALSE ELSE humanInputOutstanding
    /\ pendingTask' = NoPendingTask
    /\ UNCHANGED
        <<
            phase,
            cycle,
            attempt,
            invalidAttempt,
            activeNode,
            heldTarget,
            targetEditMode,
            proofEditMode,
            configuredTargets,
            approvedConfiguredTargets,
            presentNodes,
            committedPresentNodes,
            openNodes,
            committedOpenNodes,
            localClosureUnverified,
            committedLocalClosureUnverified,
            currentCoverage,
            committedCoverage,
            approvedCoverage,
            paperCurrentFp,
            committedPaperCurrentFp,
            substantivenessCurrentFp,
            committedSubstantivenessCurrentFp,
            currentTargetFp,
            committedTargetFp,
            approvedTargetFp,
            coarseDagNodes,
            corrCurrentFp,
            committedCorrCurrentFp,
            soundCurrentFp,
            committedSoundCurrentFp,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED AllStructureVars
    /\ UNCHANGED LocalClosureVars
    /\ ClearArtifactsAndRecordHistory

RejectInvalidReviewArtifact ==
    /\ stage = "Reviewer"
    /\ response.kind = "review"
    /\ \/ response.status = "malformed"
       \/ /\ response.status = "ok"
          /\ ~ReviewDecisionLegal
    /\ stage' = "Reviewer"
    /\ reviewerComments' = reviewerComments
    /\ latestPaperEvidenceLanes' = latestPaperEvidenceLanes
    /\ latestCorrEvidenceLanes' = latestCorrEvidenceLanes
    /\ latestSoundEvidenceLanes' = latestSoundEvidenceLanes
    /\ latestPaperReviewTargets' = latestPaperReviewTargets
    /\ latestCorrReviewNodes' = latestCorrReviewNodes
    /\ latestSoundReviewNodes' = latestSoundReviewNodes
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = latestCorrPanelSplit
    /\ latestSoundPanelSplit' = latestSoundPanelSplit
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ UNCHANGED
        <<
            phase,
            cycle,
            attempt,
            invalidAttempt,
            gateKind,
            gateFromInvalidAttempt,
            activeNode,
            heldTarget,
            targetEditMode,
            proofEditMode,
            configuredTargets,
            approvedConfiguredTargets,
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
            nodeDifficulty,
            easyAttempts,
            humanInputOutstanding,
            pendingTask,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED AllStructureVars
    /\ UNCHANGED LocalClosureVars
    /\ ClearArtifactsAndRecordHistory

ReviewContinueAfterValid ==
    /\ phase = "theorem_stating"
    /\ stage = "Reviewer"
    /\ ~invalidAttempt
    /\ ReviewDecisionLegal
    /\ response.decision = "CONTINUE"
    /\ LET
            resetCorrStatus == ApplyReviewCorrStatusResets(corrStatus, response.resetBlockers)
            resetTargetCorrStatus == ApplyReviewPaperStatusResets(paperStatus, response.resetBlockers)
            resetSubstantivenessStatus == ApplyReviewSubstantivenessStatusResets(substantivenessStatus, response.resetBlockers)
            resetSoundStatus == ApplyReviewSoundStatusResets(soundStatus, response.resetBlockers)
            nextCorrStatus ==
                IF response.reset = "lastClean" THEN [n \in Nodes |-> "unknown"]
                ELSE ApplyReviewCorrStatusAdjudications(
                    resetCorrStatus,
                    response.taskBlockers
                )
            nextTargetCorrStatus ==
                IF response.reset = "lastClean" THEN [t \in Targets |-> "unknown"]
                ELSE ApplyReviewPaperStatusAdjudications(
                    resetTargetCorrStatus,
                    response.taskBlockers
                )
            nextSubstantivenessStatus ==
                IF response.reset = "lastClean" THEN [n \in Nodes |-> "unknown"]
                ELSE ApplyReviewSubstantivenessStatusAdjudications(
                    resetSubstantivenessStatus,
                    response.taskBlockers
                )
            nextSoundStatus ==
                IF response.reset = "lastClean" THEN [n \in Nodes |-> "unknown"]
                ELSE ApplyReviewSoundStatusAdjudications(
                    resetSoundStatus,
                    response.taskBlockers
                )
            nextGlobal ==
                NodeCorrBlockersFor(nextCorrStatus, corrCurrentFp, corrApprovedFp, presentNodes)
                \cup
                PaperBlockersFor(nextTargetCorrStatus, paperCurrentFp, paperApprovedFp, configuredTargets)
                \cup
                SubstantivenessBlockersFor(nextSubstantivenessStatus, substantivenessCurrentFp, substantivenessApprovedFp, presentNodes)
                \cup
                SoundBlockersFor(nextSoundStatus, soundCurrentFp, soundApprovedFp, presentNodes, openNodes)
            nextHeldCandidates ==
                {n \in presentNodes :
                    /\ n \in currentProofNodes
                    /\ n \in openNodes
                    /\ nextCorrStatus[n] = "pass"
                    /\ corrCurrentFp[n] = corrApprovedFp[n]
                    /\ ~(nextSoundStatus[n] = "pass" /\ soundCurrentFp[n] = soundApprovedFp[n])}
            nextHeld ==
                IF {b \in nextGlobal : b.kind \in {"node_corr", "paper_faithfulness", "substantiveness"}} # {} THEN
                    NoNode
                ELSE IF /\ heldTarget \in nextHeldCandidates
                   /\ \A m \in nextHeldCandidates: NodeRank[heldTarget] >= NodeRank[m]
                THEN
                    heldTarget
                ELSE IF nextHeldCandidates = {} THEN
                    NoNode
                ELSE
                    CHOOSE n \in nextHeldCandidates:
                        \A m \in nextHeldCandidates:
                            \/ NodeRank[n] > NodeRank[m]
                            \/ /\ NodeRank[n] = NodeRank[m]
                               /\ NodeOrder[n] >= NodeOrder[m]
       IN
            /\ activeNode' = response.nextActive
            /\ heldTarget' = nextHeld
            /\ targetEditMode' =
                IF response.nextActive = NoNode THEN "global" ELSE response.nextMode
            /\ proofEditMode' = "local"
            /\ reviewerComments' = response.comments
            /\ latestPaperEvidenceLanes' = {}
            /\ latestCorrEvidenceLanes' = {}
            /\ latestSoundEvidenceLanes' = {}
            /\ latestPaperReviewTargets' = {}
            /\ latestCorrReviewNodes' = {}
            /\ latestSoundReviewNodes' = {}
            /\ latestPaperPanelSplit' = latestPaperPanelSplit
            /\ latestCorrPanelSplit' = FALSE
            /\ latestSoundPanelSplit' = FALSE
            /\ previousPaperFindingLanes' = previousPaperFindingLanes
            /\ previousCorrFindingLanes' = previousCorrFindingLanes
            /\ previousSoundFindingLanes' = previousSoundFindingLanes
            /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
            /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
            /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
            /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
            /\ paperStatus' = nextTargetCorrStatus
            /\ paperApprovedFp' = ApplyReviewPaperApprovedFpAdjudications(
                   ApplyReviewPaperApprovedFpResets(paperApprovedFp, response.resetBlockers),
                   paperCurrentFp, response.taskBlockers)
            /\ substantivenessStatus' = nextSubstantivenessStatus
            /\ substantivenessApprovedFp' = ApplyReviewSubstantivenessApprovedFpAdjudications(
                   ApplyReviewSubstantivenessApprovedFpResets(substantivenessApprovedFp, response.resetBlockers),
                   substantivenessCurrentFp, response.taskBlockers)
            /\ corrStatus' = nextCorrStatus
            /\ corrApprovedFp' = ApplyReviewCorrApprovedFpAdjudications(
                   ApplyReviewCorrApprovedFpResets(corrApprovedFp, response.resetBlockers),
                   corrCurrentFp, response.taskBlockers)
            /\ soundStatus' = nextSoundStatus
            /\ soundApprovedFp' = ApplyReviewSoundApprovedFpAdjudications(
                   ApplyReviewSoundApprovedFpResets(soundApprovedFp, response.resetBlockers),
                   soundCurrentFp, response.taskBlockers)
            \* Sound assessment store mirror (theorem-stating Continue
            \* after a valid worker burst: resets + adjudications).
            /\ soundAssessmentStatus' =
                   IF response.reset = "lastClean" THEN DefaultSoundAssessmentStatus
                   ELSE ApplyReviewSoundAssessmentAdjudications(
                       ApplyReviewSoundAssessmentResets(soundAssessmentStatus,
                                                        response.resetBlockers),
                       response.taskBlockers)
            /\ reviewerRequestedSoundVerifierNodes' =
                   IF response.reset = "lastClean" THEN {}
                   ELSE IF response.reset = NoCheckpoint THEN
                       reviewerRequestedSoundVerifierNodes
                           \cup (response.requestSoundVerifierNodes \cap presentNodes)
                   ELSE
                       reviewerRequestedSoundVerifierNodes
            /\ soundReverificationContext' = NoSoundReverificationContext
            /\ nodeDifficulty' = ApplyDifficultyUpdates(nodeDifficulty, response.difficultyMap)
            /\ easyAttempts' = ApplyDifficultyAttemptResets(nodeDifficulty, easyAttempts, response.difficultyMap)
            /\ humanInputOutstanding' =
                IF response.clearHumanInput THEN FALSE ELSE humanInputOutstanding
            /\ pendingTask' =
                [
                    taskBlockers |-> response.taskBlockers \cap nextGlobal,
                    node |-> activeNode',
                    mode |-> targetEditMode',
                    orphanCleanupNodes |-> {},
                    nextWorkerContextMode |-> response.nextWorkerContextMode,
                    paperFocusRanges |-> response.paperFocusRanges,
                    workStyleHint |-> response.workStyleHint,
                    allowNewObligations |-> TRUE,
                    mustCloseActive |-> FALSE,
                    authorizedNodes |-> {},
                    consumedGlobalRepairGrant |-> FALSE
                ]
            /\ stage' = "Start"
            /\ gateKind' = "none"
            /\ gateFromInvalidAttempt' = FALSE
            /\ invalidAttempt' = FALSE
            /\ attempt' = 0
            /\ CommitCurrentWorktree
    /\ UNCHANGED <<
            phase,
            cycle,
            configuredTargets,
            approvedConfiguredTargets,
            presentNodes,
            openNodes,
            localClosureUnverified,
            currentCoverage,
            approvedCoverage,
            paperCurrentFp,
            substantivenessCurrentFp,
            currentTargetFp,
            approvedTargetFp,
            coarseDagNodes,
            corrCurrentFp,
            soundCurrentFp,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED CurrentStructureVars
    /\ ClearArtifactsAndRecordHistory

\* Mirror of kernel `apply_theorem_review_response` NEED_INPUT arm in
\* engine.rs (TheoremStating + non-retry case calling
\* `route_need_input_to_auditor` with gate_from_invalid_attempt=FALSE).
\* Routes to StuckMathAudit, activates the latch. See
\* ReviewNeedInputAfterInvalid for the symmetric retry-case wiring.
ReviewNeedInputAfterValid ==
    /\ phase = "theorem_stating"
    /\ stage = "Reviewer"
    /\ ~invalidAttempt
    /\ ReviewDecisionLegal
    /\ response.decision = "NEED_INPUT"
    /\ LET
            nextCorrStatus == ApplyReviewCorrStatusResets(corrStatus, response.resetBlockers)
            nextTargetCorrStatus == ApplyReviewPaperStatusResets(paperStatus, response.resetBlockers)
            nextSubstantivenessStatus == ApplyReviewSubstantivenessStatusResets(substantivenessStatus, response.resetBlockers)
            nextSoundStatus == ApplyReviewSoundStatusResets(soundStatus, response.resetBlockers)
            nextGlobal ==
                NodeCorrBlockersFor(nextCorrStatus, corrCurrentFp, corrApprovedFp, presentNodes)
                \cup
                PaperBlockersFor(nextTargetCorrStatus, paperCurrentFp, paperApprovedFp, configuredTargets)
                \cup
                SubstantivenessBlockersFor(nextSubstantivenessStatus, substantivenessCurrentFp, substantivenessApprovedFp, presentNodes)
                \cup
                SoundBlockersFor(nextSoundStatus, soundCurrentFp, soundApprovedFp, presentNodes, openNodes)
       IN
            /\ stage' = "StuckMathAudit"
            /\ gateKind' = "none"
            /\ gateFromInvalidAttempt' = FALSE
            /\ stuckMathAuditActive' = TRUE
            /\ stuckMathAuditNeedInputAudit' =
                [gateFromInvalidAttempt |-> FALSE]
            /\ stuckMathAuditBurstRetryCount' = 0
            /\ lastStuckMathAuditDispatchedCycle' = cycle
            /\ reviewerComments' = ""
            /\ latestPaperEvidenceLanes' = {}
            /\ latestCorrEvidenceLanes' = {}
            /\ latestSoundEvidenceLanes' = {}
            /\ latestPaperReviewTargets' = {}
            /\ latestCorrReviewNodes' = {}
            /\ latestSoundReviewNodes' = {}
            /\ latestPaperPanelSplit' = latestPaperPanelSplit
            /\ latestCorrPanelSplit' = FALSE
            /\ latestSoundPanelSplit' = FALSE
            /\ previousPaperFindingLanes' = previousPaperFindingLanes
            /\ previousCorrFindingLanes' = previousCorrFindingLanes
            /\ previousSoundFindingLanes' = previousSoundFindingLanes
            /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
            /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
            /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
            /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
            /\ paperStatus' = nextTargetCorrStatus
            /\ paperApprovedFp' = ApplyReviewPaperApprovedFpResets(paperApprovedFp, response.resetBlockers)
            /\ substantivenessStatus' = nextSubstantivenessStatus
            /\ substantivenessApprovedFp' = ApplyReviewSubstantivenessApprovedFpResets(substantivenessApprovedFp, response.resetBlockers)
            /\ corrStatus' = nextCorrStatus
            /\ corrApprovedFp' = ApplyReviewCorrApprovedFpResets(corrApprovedFp, response.resetBlockers)
            /\ soundStatus' = nextSoundStatus
            /\ soundApprovedFp' = ApplyReviewSoundApprovedFpResets(soundApprovedFp, response.resetBlockers)
            \* Sound assessment store mirror (theorem-stating NeedInput
            \* after a valid worker burst: sticky-fail).
            /\ soundAssessmentStatus' =
                   IF response.reset = "lastClean" THEN DefaultSoundAssessmentStatus
                   ELSE ApplyReviewSoundAssessmentResets(soundAssessmentStatus,
                                                         response.resetBlockers)
            /\ reviewerRequestedSoundVerifierNodes' =
                   IF response.reset = "lastClean" THEN {}
                   ELSE reviewerRequestedSoundVerifierNodes
            /\ soundReverificationContext' = NoSoundReverificationContext
            /\ nodeDifficulty' = ApplyDifficultyUpdates(nodeDifficulty, response.difficultyMap)
            /\ easyAttempts' = ApplyDifficultyAttemptResets(nodeDifficulty, easyAttempts, response.difficultyMap)
            /\ pendingTask' = NoPendingTask
            /\ invalidAttempt' = FALSE
            /\ humanInputOutstanding' =
                IF response.clearHumanInput THEN FALSE ELSE humanInputOutstanding
            /\ CommitCurrentWorktree
    /\ UNCHANGED <<
            phase,
            cycle,
            attempt,
            activeNode,
            heldTarget,
            targetEditMode,
            proofEditMode,
            configuredTargets,
            approvedConfiguredTargets,
            presentNodes,
            openNodes,
            localClosureUnverified,
            currentCoverage,
            approvedCoverage,
            paperCurrentFp,
            substantivenessCurrentFp,
            currentTargetFp,
            approvedTargetFp,
            coarseDagNodes,
            corrCurrentFp,
            soundCurrentFp,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED CurrentStructureVars
    /\ ClearArtifactsAndRecordHistory

ReviewAdvancePhase ==
    \* The reviewer declares theorem-stating complete and hands off directly
    \* to the outside-expert HumanGate. No intermediate combined-panel
    \* re-verification: the per-phase verifier panels have already pinned
    \* status + approvedFp on every node, and AdvancePhase legality requires
    \* GlobalBlockers = {} (via ReviewDecisionLegal). The human is the expert
    \* gate.
    /\ phase = "theorem_stating"
    /\ stage = "Reviewer"
    /\ ~invalidAttempt
    /\ ReviewDecisionLegal
    /\ response.decision = "ADVANCE_PHASE"
    /\ stage' = "HumanGate"
    /\ gateKind' = "advance"
    /\ gateFromInvalidAttempt' = FALSE
    /\ pendingTask' = NoPendingTask
    /\ activeNode' = response.nextActive
    /\ heldTarget' = NoNode
    /\ targetEditMode' = "global"
    /\ proofEditMode' = "local"
    /\ reviewerComments' = ""
    /\ latestPaperEvidenceLanes' = {}
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = {}
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ nodeDifficulty' = ApplyDifficultyUpdates(nodeDifficulty, response.difficultyMap)
    /\ easyAttempts' = ApplyDifficultyAttemptResets(nodeDifficulty, easyAttempts, response.difficultyMap)
    /\ invalidAttempt' = FALSE
    /\ humanInputOutstanding' =
        IF response.clearHumanInput THEN FALSE ELSE humanInputOutstanding
    /\ CommitCurrentWorktree
    /\ UNCHANGED <<
            phase,
            cycle,
            attempt,
            configuredTargets,
            approvedConfiguredTargets,
            presentNodes,
            openNodes,
            currentCoverage,
            approvedCoverage,
            paperStatus,
            paperCurrentFp,
            paperApprovedFp,
            substantivenessStatus,
            substantivenessCurrentFp,
            committedSubstantivenessCurrentFp,
            substantivenessApprovedFp,
            currentTargetFp,
            approvedTargetFp,
            coarseDagNodes,
            corrStatus,
            corrCurrentFp,
            corrApprovedFp,
            soundStatus,
            soundCurrentFp,
            soundApprovedFp,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED AllStructureVars
    /\ UNCHANGED LocalClosureVars
    /\ ClearArtifactsAndRecordHistory

ReviewContinueProof ==
    /\ phase = "proof_formalization"
    /\ stage = "Reviewer"
    /\ ReviewDecisionLegal
    /\ response.decision = "CONTINUE"
    \* Proposal v32: nextActiveCoarse validation. May only be non-NoNode
    \* outside retry-review, and must be a current kernel hint (which
    \* in turn requires ActiveCoarseChangeAllowed).
    /\ (response.nextActiveCoarse # NoNode) =>
           /\ retryOutcomeKind = "none"
           /\ response.nextActiveCoarse \in KernelHintedNextActiveCoarseNodes
    \* Proposal v32 followups (kernel commits 32895d6 + 31d9012):
    \* ProofFormalization Continue must ADVANCE the coarse anchor
    \* whenever the kernel signals that the anchor lock is open on a
    \* clean unlock — i.e. KernelHintedNextActiveCoarseNodes is non-empty
    \* AND CoarseAnchorStarvationUnlocked is FALSE. Two sub-cases trigger
    \* the rejection: (a) activeCoarseNode = NoNode (no anchor to keep)
    \* and (b) anchor reached shallow-coarse-closure with no global
    \* blockers (clean unlock — piggybacking on the closed cone is
    \* label noise). Starvation unlocks are exempted. Retry contexts
    \* are exempted (nextActiveCoarse # NoNode is itself illegal there).
    /\ /\ retryOutcomeKind = "none"
       /\ coarseDagNodes # {}
       /\ KernelHintedNextActiveCoarseNodes # {}
       /\ ~CoarseAnchorStarvationUnlocked
       => response.nextActiveCoarse # NoNode
    /\ LET retryReview == (retryOutcomeKind # "none")
           resetRequested == (response.reset \in {"lastCommit", "lastClean"})
           chosenActive ==
               IF response.nextActive # NoNode THEN response.nextActive ELSE activeNode
           chosenCoarse ==
               IF response.nextActiveCoarse # NoNode
                   THEN response.nextActiveCoarse
                   ELSE activeCoarseNode
       IN
        /\ IF response.reset = "lastClean" THEN
                ApplyLastCleanReset
            ELSE IF response.reset = "lastCommit" THEN
                RestoreCommittedWorktree
            ELSE
                /\ currentNodeKinds' = currentNodeKinds
                /\ currentProofNodes' = currentProofNodes
                /\ currentDeps' = currentDeps
                /\ currentTargetClaims' = currentTargetClaims
                /\ presentNodes' = presentNodes
                /\ openNodes' = openNodes
                \* Patch C: no-reset Continue preserves the live closure
                \* tier; invalidations only fire on accepted worker deltas
                \* or migration probes, neither of which is happening here.
                /\ localClosureUnverified' = localClosureUnverified
                /\ currentCoverage' = currentCoverage
                /\ paperCurrentFp' = paperCurrentFp
                /\ currentTargetFp' = currentTargetFp
                /\ corrCurrentFp' = corrCurrentFp
                /\ soundCurrentFp' = soundCurrentFp
        /\ activeNode' = chosenActive
        /\ heldTarget' = NoNode
        /\ targetEditMode' = "global"
        /\ proofEditMode' =
            IF chosenActive = NoNode THEN "local" ELSE response.nextMode
    /\ corrStatus' =
           IF response.reset = "lastClean" THEN [n \in Nodes |-> "unknown"]
           ELSE ApplyReviewCorrStatusAdjudications(
               ApplyReviewCorrStatusResets(corrStatus, response.resetBlockers),
               response.taskBlockers)
    /\ corrApprovedFp' = ApplyReviewCorrApprovedFpAdjudications(
           ApplyReviewCorrApprovedFpResets(corrApprovedFp, response.resetBlockers),
           IF response.reset = "lastClean" THEN DefaultFp
           ELSE IF response.reset = "lastCommit" THEN committedCorrCurrentFp ELSE corrCurrentFp,
           response.taskBlockers)
    /\ paperStatus' =
           IF response.reset = "lastClean" THEN [t \in Targets |-> "unknown"]
           ELSE ApplyReviewPaperStatusAdjudications(
               ApplyReviewPaperStatusResets(paperStatus, response.resetBlockers),
               response.taskBlockers)
    /\ paperApprovedFp' = ApplyReviewPaperApprovedFpAdjudications(
           ApplyReviewPaperApprovedFpResets(paperApprovedFp, response.resetBlockers),
           IF response.reset = "lastClean" THEN DefaultTargetFp
           ELSE IF response.reset = "lastCommit" THEN committedPaperCurrentFp ELSE paperCurrentFp,
           response.taskBlockers)
    /\ soundStatus' =
           IF response.reset = "lastClean" THEN [n \in Nodes |-> "unknown"]
           ELSE ApplyReviewSoundStatusAdjudications(
               ApplyReviewSoundStatusResets(soundStatus, response.resetBlockers),
               response.taskBlockers)
    /\ soundApprovedFp' = ApplyReviewSoundApprovedFpAdjudications(
           ApplyReviewSoundApprovedFpResets(soundApprovedFp, response.resetBlockers),
           IF response.reset = "lastClean" THEN DefaultFp
           ELSE IF response.reset = "lastCommit" THEN committedSoundCurrentFp ELSE soundCurrentFp,
           response.taskBlockers)
    \* Rich-taxonomy assessment store mirror. LastClean reset zeros to
    \* fresh_unknown (kernel `apply_last_clean_reset` clears
    \* `sound_assessments`); reset_blockers zero per-node;
    \* task_blockers pin Sound to reviewer_pinned_fail.
    /\ soundAssessmentStatus' =
           IF response.reset = "lastClean" THEN DefaultSoundAssessmentStatus
           ELSE ApplyReviewSoundAssessmentAdjudications(
               ApplyReviewSoundAssessmentResets(soundAssessmentStatus,
                                                response.resetBlockers),
               response.taskBlockers)
    \* Reviewer-requested verifier dispatch latch (kernel
    \* `queue_reviewer_requested_sound_verifiers`, engine.rs ~3729):
    \* the union only fires on Continue with no reset; LastClean
    \* zeros the set; otherwise the set is preserved across the
    \* review acceptance. Eligibility filter is approximated by
    \* `presentNodes \cap response.requestSoundVerifierNodes` — the
    \* kernel's tighter `sound_verifier_eligible` predicate is a
    \* subset of presentNodes and the response validator upstream
    \* already constrains the reviewer to a sensible set.
    /\ reviewerRequestedSoundVerifierNodes' =
           IF response.reset = "lastClean" THEN {}
           ELSE IF response.reset = NoCheckpoint THEN
               reviewerRequestedSoundVerifierNodes
                   \cup (response.requestSoundVerifierNodes \cap presentNodes)
           ELSE
               reviewerRequestedSoundVerifierNodes
    \* The per-request reverification context evaporates with each
    \* review acceptance (the next Sound request rebuilds it from the
    \* assessment store via `request_sound_reverification_context`).
    /\ soundReverificationContext' = NoSoundReverificationContext
    /\ reviewerComments' = response.comments
    /\ latestPaperEvidenceLanes' = {}
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = {}
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ nodeDifficulty' = ApplyDifficultyUpdates(nodeDifficulty, response.difficultyMap)
    /\ easyAttempts' = ApplyDifficultyAttemptResets(nodeDifficulty, easyAttempts, response.difficultyMap)
    /\ pendingTask' =
        [
            taskBlockers |-> response.taskBlockers,
            node |-> activeNode',
            mode |-> proofEditMode',
            orphanCleanupNodes |-> {},
            nextWorkerContextMode |-> response.nextWorkerContextMode,
            paperFocusRanges |-> response.paperFocusRanges,
            workStyleHint |-> response.workStyleHint,
            allowNewObligations |-> response.allowNewObligations,
            mustCloseActive |-> response.mustCloseActive,
            authorizedNodes |-> response.authorizedNodes,
            consumedGlobalRepairGrant |-> FALSE
        ]
    /\ phase' = phase
    /\ stage' = IF retryOutcomeKind # "none" THEN "Worker" ELSE "Start"
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ invalidAttempt' = FALSE
    /\ humanInputOutstanding' =
        IF response.clearHumanInput THEN FALSE ELSE humanInputOutstanding
    /\ attempt' = IF retryOutcomeKind # "none" THEN 1 ELSE 0
    /\ IF retryOutcomeKind # "none" THEN
            UNCHANGED
                <<
                    committedProofNodes,
                    committedNodeKinds,
                    committedDeps,
                    committedTargetClaims,
                    committedPresentNodes,
                    committedOpenNodes,
                    localClosureUnverified,
                    committedLocalClosureUnverified,
                    committedCoverage,
                    committedPaperCurrentFp,
                    committedSubstantivenessCurrentFp,
                    committedTargetFp,
                    committedCorrCurrentFp,
                    committedSoundCurrentFp
                >>
       ELSE
            CommitCurrentWorktree
    /\ substantivenessStatus' =
           IF response.reset = "lastClean" THEN [n \in Nodes |-> "unknown"]
           ELSE ApplyReviewSubstantivenessStatusAdjudications(
               ApplyReviewSubstantivenessStatusResets(substantivenessStatus, response.resetBlockers),
               response.taskBlockers)
    /\ substantivenessCurrentFp' =
           IF response.reset = "lastClean" THEN DefaultFp
           ELSE IF response.reset = "lastCommit" THEN committedSubstantivenessCurrentFp
           ELSE substantivenessCurrentFp
    /\ substantivenessApprovedFp' = ApplyReviewSubstantivenessApprovedFpAdjudications(
           ApplyReviewSubstantivenessApprovedFpResets(substantivenessApprovedFp, response.resetBlockers),
           IF response.reset = "lastClean" THEN DefaultFp
           ELSE IF response.reset = "lastCommit" THEN committedSubstantivenessCurrentFp
           ELSE substantivenessCurrentFp,
           response.taskBlockers)
    /\ UNCHANGED
        <<
            cycle,
            configuredTargets,
            approvedConfiguredTargets,
            approvedCoverage,
            approvedTargetFp
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ProtectedReapprovalVars
    \* Audit-plan reviewer mutations (kernel
    \* `apply_review_audit_plan_actions`): individual task dismissals
    \* stamp `dismissed=true` on the live plan; whole-plan dismissal
    \* moves the plan into `supersededAuditPlan`. Bridged into
    \* AcceptStuckMathAuditReviewAuditPlanActions in this spec; the
    \* per-Continue-arm path leaves the plan vars unchanged. Modeled
    \* here as the no-op default; the dedicated mutation actions
    \* (`RecordAuditPlan`, `DismissAuditPlanTask`) carry the real
    \* state change.
    /\ UNCHANGED AuditPlanVars
    \* Post-advance routing latch (kernel
    \* `apply_proof_review_response` engine.rs): cleared at entry,
    \* before phase-specific logic mutates state. Subsequent request
    \* issuances derive `post_advance_routing: false`.
    /\ postAdvanceRoutingPending' = FALSE
    \* Proposal v32: explicit coarse-anchor mutation. activeCoarseNode
    \* tracks the chosen coarse anchor (preserved if reviewer omits the
    \* override). cyclesInCoarseRepairMode is reset to 0 when the anchor
    \* changes (kernel fresh-start); otherwise incremented when
    \* CoarseRepairMode is currently TRUE and the cycle proceeds normally,
    \* and reset to 0 when CoarseRepairMode goes false. The increment
    \* fires on every reviewer-Continue cycle that doesn't change the
    \* anchor (matching the kernel notion of "another cycle in repair
    \* mode"); cleared on every other path.
    /\ activeCoarseNode' =
           IF response.nextActiveCoarse # NoNode
               THEN response.nextActiveCoarse
               ELSE activeCoarseNode
    /\ cyclesInCoarseRepairMode' =
           IF activeCoarseNode' # activeCoarseNode \/ activeCoarseNode' = NoNode THEN
               0
           ELSE IF CoarseRepairMode THEN
               cyclesInCoarseRepairMode + 1
           ELSE
               0
    /\ ClearArtifactsAndRecordHistory

\* Mirror of kernel `apply_proof_review_response` NEED_INPUT arm in
\* engine.rs calling `route_need_input_to_auditor` with
\* gate_from_invalid_attempt = (retry_outcome_kind == Invalid).
\* Routes to StuckMathAudit; activates the latch.
ReviewNeedInputProof ==
    /\ phase = "proof_formalization"
    /\ stage = "Reviewer"
    /\ ReviewDecisionLegal
    /\ response.decision = "NEED_INPUT"
    /\ LET retryReview == (retryOutcomeKind # "none")
           resetRequested == (response.reset \in {"lastCommit", "lastClean"})
       IN
        /\ IF response.reset = "lastClean" THEN
                ApplyLastCleanReset
            ELSE IF response.reset = "lastCommit" THEN
                RestoreCommittedWorktree
            ELSE
                /\ currentNodeKinds' = currentNodeKinds
                /\ currentProofNodes' = currentProofNodes
                /\ currentDeps' = currentDeps
                /\ currentTargetClaims' = currentTargetClaims
                /\ presentNodes' = presentNodes
                /\ openNodes' = openNodes
                \* Patch C: NeedInput preserves the live closure tier when
                \* no reset is requested.
                /\ localClosureUnverified' = localClosureUnverified
                /\ currentCoverage' = currentCoverage
                /\ paperCurrentFp' = paperCurrentFp
                /\ currentTargetFp' = currentTargetFp
                /\ corrCurrentFp' = corrCurrentFp
                /\ soundCurrentFp' = soundCurrentFp
        /\ phase' = phase
        /\ stage' = "StuckMathAudit"
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ stuckMathAuditActive' = TRUE
    /\ stuckMathAuditNeedInputAudit' =
        [gateFromInvalidAttempt |-> (retryOutcomeKind = "invalid")]
    /\ stuckMathAuditBurstRetryCount' = 0
    /\ lastStuckMathAuditDispatchedCycle' = cycle
    /\ reviewerComments' = ""
    /\ latestPaperEvidenceLanes' = {}
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = {}
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ nodeDifficulty' = ApplyDifficultyUpdates(nodeDifficulty, response.difficultyMap)
    /\ easyAttempts' = ApplyDifficultyAttemptResets(nodeDifficulty, easyAttempts, response.difficultyMap)
    /\ pendingTask' = NoPendingTask
    /\ invalidAttempt' = FALSE
    /\ humanInputOutstanding' =
        IF response.clearHumanInput THEN FALSE ELSE humanInputOutstanding
    /\ corrStatus' =
           IF response.reset = "lastClean" THEN [n \in Nodes |-> "unknown"]
           ELSE ApplyReviewCorrStatusResets(corrStatus, response.resetBlockers)
    /\ corrApprovedFp' = ApplyReviewCorrApprovedFpResets(corrApprovedFp, response.resetBlockers)
    /\ paperStatus' =
           IF response.reset = "lastClean" THEN [t \in Targets |-> "unknown"]
           ELSE ApplyReviewPaperStatusResets(paperStatus, response.resetBlockers)
    /\ paperApprovedFp' = ApplyReviewPaperApprovedFpResets(paperApprovedFp, response.resetBlockers)
    /\ soundStatus' =
           IF response.reset = "lastClean" THEN [n \in Nodes |-> "unknown"]
           ELSE ApplyReviewSoundStatusResets(soundStatus, response.resetBlockers)
    /\ soundApprovedFp' = ApplyReviewSoundApprovedFpResets(soundApprovedFp, response.resetBlockers)
    \* Sound assessment store / latch / reverification context mirror.
    \* NeedInput is a sticky-fail dispatch route: it does not run
    \* `ApplyReviewSoundAssessmentAdjudications` (no task_blockers are
    \* pinned at this stage). LastClean reset zeros to fresh_unknown;
    \* reset_blockers zero per-node; latch carried through except on
    \* LastClean; reverification context evaporates.
    /\ soundAssessmentStatus' =
           IF response.reset = "lastClean" THEN DefaultSoundAssessmentStatus
           ELSE ApplyReviewSoundAssessmentResets(soundAssessmentStatus,
                                                 response.resetBlockers)
    /\ reviewerRequestedSoundVerifierNodes' =
           IF response.reset = "lastClean" THEN {}
           ELSE reviewerRequestedSoundVerifierNodes
    /\ soundReverificationContext' = NoSoundReverificationContext
    \* Substantiveness lane is active in proof_formalization but
    \* ReviewNeedInputProof leaves the per-node mirrors UNCHANGED:
    \* NeedInput surfaces a human-gate decision rather than progressing
    \* lane state, so no substantiveness reset / adjudication fires here.
    /\ substantivenessStatus' = substantivenessStatus
    /\ substantivenessCurrentFp' = substantivenessCurrentFp
    /\ substantivenessApprovedFp' = substantivenessApprovedFp
    /\ IF retryOutcomeKind # "none" THEN
            UNCHANGED
                <<
                    committedProofNodes,
                    committedNodeKinds,
                    committedDeps,
                    committedTargetClaims,
                    committedPresentNodes,
                    committedOpenNodes,
                    localClosureUnverified,
                    committedLocalClosureUnverified,
                    committedCoverage,
                    committedPaperCurrentFp,
                    committedSubstantivenessCurrentFp,
                    committedTargetFp,
                    committedCorrCurrentFp,
                    committedSoundCurrentFp
                >>
       ELSE
            CommitCurrentWorktree
    /\ UNCHANGED
        <<
            cycle,
            attempt,
            activeNode,
            heldTarget,
            targetEditMode,
            proofEditMode,
            configuredTargets,
            approvedConfiguredTargets,
            approvedCoverage,
            approvedTargetFp
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED DeviationVars
    \* GlobalRepairAuditor / NeedInputAuditor mutex: routing a NeedInput
    \* escalation through the StuckMathAudit lane must pre-empt any
    \* in-flight `pendingGlobalRepairRequest`. Mirrors the proactive
    \* `state.pending_global_repair_request.take()` in
    \* `route_need_input_to_auditor` (engine.rs), symmetric to the
    \* existing `stuckMathAuditNeedInputAudit' = NoNeedInputAuditContext`
    \* clear in `RequestGlobalRepairAudit`. In normal flow the take is
    \* a no-op (the kernel never has both fields set on a Reviewer
    \* response); the auto-decline reason surfaces the pre-emption when
    \* it does fire, and the grant slot is cleared in the same step
    \* (the kernel pairs the clear with the request take).
    /\ pendingGlobalRepairRequest' = NoGlobalRepairRequest
    /\ pendingGlobalRepairGrant' =
           IF pendingGlobalRepairRequest # NoGlobalRepairRequest
                THEN NoGlobalRepairGrant
                ELSE pendingGlobalRepairGrant
    /\ latestGlobalRepairAuditDeclineReason' =
           IF pendingGlobalRepairRequest # NoGlobalRepairRequest
                THEN "declined"
                ELSE latestGlobalRepairAuditDeclineReason
    /\ latestGlobalRepairAuditDeclineCycle' =
           IF pendingGlobalRepairRequest # NoGlobalRepairRequest
                THEN cycle
                ELSE latestGlobalRepairAuditDeclineCycle
    /\ lastReviewerGlobalRepairRequestCycle' = lastReviewerGlobalRepairRequestCycle
    /\ everShallowCoarseClosed' = everShallowCoarseClosed
    /\ globalRepairModeEnabled' = globalRepairModeEnabled
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    \* Post-advance routing latch cleared at entry to
    \* `apply_proof_review_response` (kernel engine.rs).
    /\ postAdvanceRoutingPending' = FALSE
    /\ ClearArtifactsAndRecordHistory

ReviewContinueCleanup ==
    /\ phase = "cleanup"
    /\ stage = "Reviewer"
    /\ ReviewDecisionLegal
    /\ response.decision = "CONTINUE"
    \* Cleanup-v2: `response.nextActive` is rejected upstream by
    \* `ReviewResponseLegal`. Kernel derives `activeNode'` from
    \* `cleanupAuditTasks[response.cleanupNextTask].target_node` when a
    \* task is dispatched, and from `NoNode` otherwise. The spec abstracts
    \* the task-dispatch population (cleanupAuditTasks stays empty on
    \* modeled traces — see comment at line 4930), so we model only the
    \* cleared shape here.
    /\ LET resetRequested == (response.reset \in {"lastCommit", "lastClean"})
           chosenActive == NoNode
       IN
        /\ IF response.reset = "lastClean" THEN
                ApplyLastCleanReset
            ELSE IF response.reset = "lastCommit" THEN
                RestoreCommittedWorktree
            ELSE
                /\ currentNodeKinds' = currentNodeKinds
                /\ currentProofNodes' = currentProofNodes
                /\ currentDeps' = currentDeps
                /\ currentTargetClaims' = currentTargetClaims
                /\ presentNodes' = presentNodes
                /\ openNodes' = openNodes
                \* Patch C: cleanup-phase no-reset Continue preserves the
                \* live closure tier; cleanup-burst delta-acceptance is
                \* the proper invalidation site, not the reviewer step.
                /\ localClosureUnverified' = localClosureUnverified
                /\ currentCoverage' = currentCoverage
                /\ paperCurrentFp' = paperCurrentFp
                /\ currentTargetFp' = currentTargetFp
                /\ corrCurrentFp' = corrCurrentFp
                /\ soundCurrentFp' = soundCurrentFp
        /\ activeNode' = chosenActive
    /\ corrStatus' =
           IF response.reset = "lastClean" THEN [n \in Nodes |-> "unknown"]
           ELSE ApplyReviewCorrStatusAdjudications(
               ApplyReviewCorrStatusResets(corrStatus, response.resetBlockers),
               response.taskBlockers)
    /\ corrApprovedFp' = ApplyReviewCorrApprovedFpAdjudications(
           ApplyReviewCorrApprovedFpResets(corrApprovedFp, response.resetBlockers),
           IF response.reset = "lastClean" THEN DefaultFp
           ELSE IF response.reset = "lastCommit" THEN committedCorrCurrentFp ELSE corrCurrentFp,
           response.taskBlockers)
    /\ paperStatus' =
           IF response.reset = "lastClean" THEN [t \in Targets |-> "unknown"]
           ELSE ApplyReviewPaperStatusAdjudications(
               ApplyReviewPaperStatusResets(paperStatus, response.resetBlockers),
               response.taskBlockers)
    /\ paperApprovedFp' = ApplyReviewPaperApprovedFpAdjudications(
           ApplyReviewPaperApprovedFpResets(paperApprovedFp, response.resetBlockers),
           IF response.reset = "lastClean" THEN DefaultTargetFp
           ELSE IF response.reset = "lastCommit" THEN committedPaperCurrentFp ELSE paperCurrentFp,
           response.taskBlockers)
    /\ soundStatus' =
           IF response.reset = "lastClean" THEN [n \in Nodes |-> "unknown"]
           ELSE ApplyReviewSoundStatusAdjudications(
               ApplyReviewSoundStatusResets(soundStatus, response.resetBlockers),
               response.taskBlockers)
    /\ soundApprovedFp' = ApplyReviewSoundApprovedFpAdjudications(
           ApplyReviewSoundApprovedFpResets(soundApprovedFp, response.resetBlockers),
           IF response.reset = "lastClean" THEN DefaultFp
           ELSE IF response.reset = "lastCommit" THEN committedSoundCurrentFp ELSE soundCurrentFp,
           response.taskBlockers)
    \* Sound assessment store mirror (same shape as ReviewContinueProof).
    /\ soundAssessmentStatus' =
           IF response.reset = "lastClean" THEN DefaultSoundAssessmentStatus
           ELSE ApplyReviewSoundAssessmentAdjudications(
               ApplyReviewSoundAssessmentResets(soundAssessmentStatus,
                                                response.resetBlockers),
               response.taskBlockers)
    /\ reviewerRequestedSoundVerifierNodes' =
           IF response.reset = "lastClean" THEN {}
           ELSE IF response.reset = NoCheckpoint THEN
               reviewerRequestedSoundVerifierNodes
                   \cup (response.requestSoundVerifierNodes \cap presentNodes)
           ELSE
               reviewerRequestedSoundVerifierNodes
    /\ soundReverificationContext' = NoSoundReverificationContext
    \* Substantiveness lane is dormant in cleanup; leave the
    \* per-node mirrors UNCHANGED.
    /\ substantivenessStatus' = substantivenessStatus
    /\ substantivenessCurrentFp' = substantivenessCurrentFp
    /\ substantivenessApprovedFp' = substantivenessApprovedFp
    /\ heldTarget' = NoNode
    /\ targetEditMode' = "global"
    /\ proofEditMode' = "local"
    /\ reviewerComments' = response.comments
    /\ latestPaperEvidenceLanes' = {}
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = {}
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ nodeDifficulty' = ApplyDifficultyUpdates(nodeDifficulty, response.difficultyMap)
    /\ easyAttempts' = ApplyDifficultyAttemptResets(nodeDifficulty, easyAttempts, response.difficultyMap)
    /\ pendingTask' =
        [
            taskBlockers |-> response.taskBlockers,
            node |-> activeNode',
            mode |-> "cleanup",
            orphanCleanupNodes |-> {},
            nextWorkerContextMode |-> response.nextWorkerContextMode,
            paperFocusRanges |-> response.paperFocusRanges,
            workStyleHint |-> response.workStyleHint,
            allowNewObligations |-> TRUE,
            mustCloseActive |-> FALSE,
            authorizedNodes |-> {},
            consumedGlobalRepairGrant |-> FALSE
        ]
    /\ phase' = phase
    /\ stage' = IF retryOutcomeKind # "none" THEN "Worker" ELSE "Start"
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ invalidAttempt' = FALSE
    /\ humanInputOutstanding' =
        IF response.clearHumanInput THEN FALSE ELSE humanInputOutstanding
    /\ attempt' = IF retryOutcomeKind # "none" THEN 1 ELSE 0
    /\ IF retryOutcomeKind # "none" THEN
            UNCHANGED
                <<
                    committedProofNodes,
                    committedNodeKinds,
                    committedDeps,
                    committedTargetClaims,
                    committedPresentNodes,
                    committedOpenNodes,
                    localClosureUnverified,
                    committedLocalClosureUnverified,
                    committedCoverage,
                    committedPaperCurrentFp,
                    committedSubstantivenessCurrentFp,
                    committedTargetFp,
                    committedCorrCurrentFp,
                    committedSoundCurrentFp
                >>
       ELSE
            CommitCurrentWorktree
    /\ UNCHANGED
        <<
            cycle,
            configuredTargets,
            approvedConfiguredTargets,
            approvedCoverage,
            approvedTargetFp
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED DeviationVars
    /\ ClearArtifactsAndRecordHistory

\* Mirror of kernel `apply_cleanup_review_response` NEED_INPUT arm in
\* engine.rs calling `route_need_input_to_auditor` with
\* gate_from_invalid_attempt = (retry_outcome_kind == Invalid).
\* Routes to StuckMathAudit; activates the latch. Note this branch is
\* unreachable on modeled traces because
\* `RequestAllowedDecisions("review")` for Cleanup omits NEED_INPUT
\* (kernel `request_allowed_decisions`); the action body is updated
\* for completeness.
ReviewNeedInputCleanup ==
    /\ phase = "cleanup"
    /\ stage = "Reviewer"
    /\ ReviewDecisionLegal
    /\ response.decision = "NEED_INPUT"
    /\ LET resetRequested == (response.reset \in {"lastCommit", "lastClean"})
       IN
        /\ IF response.reset = "lastClean" THEN
                ApplyLastCleanReset
            ELSE IF response.reset = "lastCommit" THEN
                RestoreCommittedWorktree
            ELSE
                /\ currentNodeKinds' = currentNodeKinds
                /\ currentProofNodes' = currentProofNodes
                /\ currentDeps' = currentDeps
                /\ currentTargetClaims' = currentTargetClaims
                /\ presentNodes' = presentNodes
                /\ openNodes' = openNodes
                \* Patch C: cleanup-phase NeedInput preserves the live
                \* closure tier when no reset is requested.
                /\ localClosureUnverified' = localClosureUnverified
                /\ currentCoverage' = currentCoverage
                /\ paperCurrentFp' = paperCurrentFp
                /\ currentTargetFp' = currentTargetFp
                /\ corrCurrentFp' = corrCurrentFp
                /\ soundCurrentFp' = soundCurrentFp
        /\ phase' = phase
        /\ stage' = "StuckMathAudit"
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ stuckMathAuditActive' = TRUE
    /\ stuckMathAuditNeedInputAudit' =
        [gateFromInvalidAttempt |-> (retryOutcomeKind = "invalid")]
    /\ stuckMathAuditBurstRetryCount' = 0
    /\ lastStuckMathAuditDispatchedCycle' = cycle
    /\ reviewerComments' = ""
    /\ latestPaperEvidenceLanes' = {}
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = {}
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ nodeDifficulty' = ApplyDifficultyUpdates(nodeDifficulty, response.difficultyMap)
    /\ easyAttempts' = ApplyDifficultyAttemptResets(nodeDifficulty, easyAttempts, response.difficultyMap)
    /\ pendingTask' = NoPendingTask
    /\ invalidAttempt' = FALSE
    /\ humanInputOutstanding' =
        IF response.clearHumanInput THEN FALSE ELSE humanInputOutstanding
    /\ corrStatus' =
           IF response.reset = "lastClean" THEN [n \in Nodes |-> "unknown"]
           ELSE ApplyReviewCorrStatusResets(corrStatus, response.resetBlockers)
    /\ corrApprovedFp' = ApplyReviewCorrApprovedFpResets(corrApprovedFp, response.resetBlockers)
    /\ paperStatus' =
           IF response.reset = "lastClean" THEN [t \in Targets |-> "unknown"]
           ELSE ApplyReviewPaperStatusResets(paperStatus, response.resetBlockers)
    /\ paperApprovedFp' = ApplyReviewPaperApprovedFpResets(paperApprovedFp, response.resetBlockers)
    /\ soundStatus' =
           IF response.reset = "lastClean" THEN [n \in Nodes |-> "unknown"]
           ELSE ApplyReviewSoundStatusResets(soundStatus, response.resetBlockers)
    /\ soundApprovedFp' = ApplyReviewSoundApprovedFpResets(soundApprovedFp, response.resetBlockers)
    \* Sound assessment store / latch / reverification context mirror
    \* (same shape as ReviewNeedInputProof; NeedInput is sticky-fail
    \* dispatch — no task_blockers are pinned at this stage).
    /\ soundAssessmentStatus' =
           IF response.reset = "lastClean" THEN DefaultSoundAssessmentStatus
           ELSE ApplyReviewSoundAssessmentResets(soundAssessmentStatus,
                                                 response.resetBlockers)
    /\ reviewerRequestedSoundVerifierNodes' =
           IF response.reset = "lastClean" THEN {}
           ELSE reviewerRequestedSoundVerifierNodes
    /\ soundReverificationContext' = NoSoundReverificationContext
    \* Substantiveness lane is dormant in cleanup; leave the
    \* per-node mirrors UNCHANGED.
    /\ substantivenessStatus' = substantivenessStatus
    /\ substantivenessCurrentFp' = substantivenessCurrentFp
    /\ substantivenessApprovedFp' = substantivenessApprovedFp
    /\ IF retryOutcomeKind # "none" THEN
            UNCHANGED
                <<
                    committedProofNodes,
                    committedNodeKinds,
                    committedDeps,
                    committedTargetClaims,
                    committedPresentNodes,
                    committedOpenNodes,
                    localClosureUnverified,
                    committedLocalClosureUnverified,
                    committedCoverage,
                    committedPaperCurrentFp,
                    committedSubstantivenessCurrentFp,
                    committedTargetFp,
                    committedCorrCurrentFp,
                    committedSoundCurrentFp
                >>
       ELSE
            CommitCurrentWorktree
    /\ UNCHANGED
        <<
            cycle,
            attempt,
            activeNode,
            heldTarget,
            targetEditMode,
            proofEditMode,
            configuredTargets,
            approvedConfiguredTargets,
            approvedCoverage,
            approvedTargetFp
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ ClearArtifactsAndRecordHistory

ReviewDoneCleanup ==
    /\ phase = "cleanup"
    /\ stage = "Reviewer"
    /\ ReviewDecisionLegal
    /\ response.decision = "DONE"
    /\ LET resetRequested == (response.reset \in {"lastCommit", "lastClean"})
       IN
        /\ IF response.reset = "lastClean" THEN
                ApplyLastCleanReset
            ELSE IF response.reset = "lastCommit" THEN
                RestoreCommittedWorktree
            ELSE
                /\ currentNodeKinds' = currentNodeKinds
                /\ currentProofNodes' = currentProofNodes
                /\ currentDeps' = currentDeps
                /\ currentTargetClaims' = currentTargetClaims
                /\ presentNodes' = presentNodes
                /\ openNodes' = openNodes
                /\ currentCoverage' = currentCoverage
                /\ paperCurrentFp' = paperCurrentFp
                /\ currentTargetFp' = currentTargetFp
                /\ corrCurrentFp' = corrCurrentFp
                /\ soundCurrentFp' = soundCurrentFp
        /\ phase' = "complete"
        /\ stage' = "Complete"
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ invalidAttempt' = FALSE
    /\ attempt' = 0
    /\ activeNode' = NoNode
    /\ heldTarget' = NoNode
    /\ targetEditMode' = "global"
    /\ proofEditMode' = "local"
    /\ reviewerComments' = ""
    /\ latestPaperEvidenceLanes' = {}
    /\ latestCorrEvidenceLanes' = {}
    /\ latestSoundEvidenceLanes' = {}
    /\ latestPaperReviewTargets' = {}
    /\ latestCorrReviewNodes' = {}
    /\ latestSoundReviewNodes' = {}
    /\ latestPaperPanelSplit' = latestPaperPanelSplit
    /\ latestCorrPanelSplit' = FALSE
    /\ latestSoundPanelSplit' = FALSE
    /\ previousPaperFindingLanes' = previousPaperFindingLanes
    /\ previousCorrFindingLanes' = previousCorrFindingLanes
    /\ previousSoundFindingLanes' = previousSoundFindingLanes
    /\ latestSubstantivenessEvidenceLanes' = latestSubstantivenessEvidenceLanes
    /\ latestSubstantivenessReviewNodes' = latestSubstantivenessReviewNodes
    /\ latestSubstantivenessPanelSplit' = latestSubstantivenessPanelSplit
    /\ previousSubstantivenessFindingLanes' = previousSubstantivenessFindingLanes
    /\ nodeDifficulty' = ApplyDifficultyUpdates(nodeDifficulty, response.difficultyMap)
    /\ easyAttempts' = ApplyDifficultyAttemptResets(nodeDifficulty, easyAttempts, response.difficultyMap)
    /\ humanInputOutstanding' =
        IF response.clearHumanInput THEN FALSE ELSE humanInputOutstanding
    /\ pendingTask' = NoPendingTask
    /\ IF response.reset \in {"lastCommit", "lastClean"} THEN
            /\ committedNodeKinds' = committedNodeKinds
            /\ committedProofNodes' = committedProofNodes
            /\ committedDeps' = committedDeps
            /\ committedTargetClaims' = committedTargetClaims
            /\ committedPresentNodes' = committedPresentNodes
            /\ committedOpenNodes' = committedOpenNodes
            /\ committedCoverage' = committedCoverage
            /\ committedPaperCurrentFp' = committedPaperCurrentFp
            /\ committedSubstantivenessCurrentFp' = committedSubstantivenessCurrentFp
            /\ committedTargetFp' = committedTargetFp
            /\ committedCorrCurrentFp' = committedCorrCurrentFp
            /\ committedSoundCurrentFp' = committedSoundCurrentFp
       ELSE
            CommitCurrentWorktree
    /\ UNCHANGED <<
            cycle,
            configuredTargets,
            approvedConfiguredTargets,
            approvedCoverage,
            paperStatus,
            paperApprovedFp,
            substantivenessStatus,
            substantivenessCurrentFp,
            substantivenessApprovedFp,
            approvedTargetFp,
            coarseDagNodes,
            corrStatus,
            corrApprovedFp,
            soundStatus,
            soundApprovedFp,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ ClearArtifactsAndRecordHistory

EnvStageHumanApprove ==
    /\ stage = "HumanGate"
    /\ inFlightRequest.kind = "human_gate"
    /\ inFlightRequest.cycle = cycle
    /\ response = NoResponse
    /\ response' =
        [
            NoResponse EXCEPT
                !.status = "ok",
                !.kind = "human_gate",
                !.cycle = cycle,
                !.humanChoice = "approve"
        ]
    /\ UNCHANGED
        <<
            phase,
            stage,
            cycle,
            attempt,
            invalidAttempt,
            gateKind,
            gateFromInvalidAttempt,
            activeNode,
            heldTarget,
            targetEditMode,
            proofEditMode,
            configuredTargets,
            approvedConfiguredTargets,
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
                nodeDifficulty,
                easyAttempts,
                humanInputOutstanding,
                nativeHistoryKinds,
                requestSeq,
                pendingTask,
                inFlightRequest,
                cyclesSinceClean,
                hasEverBeenClean,
                forceReviewAfterConeClean
            >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED PromptCarryVars
    /\ UNCHANGED AllStructureVars
    /\ UNCHANGED LocalClosureVars

EnvStageHumanFeedback ==
    /\ stage = "HumanGate"
    /\ inFlightRequest.kind = "human_gate"
    /\ inFlightRequest.cycle = cycle
    /\ response = NoResponse
    /\ response' =
        [
            NoResponse EXCEPT
                !.status = "ok",
                !.kind = "human_gate",
                !.cycle = cycle,
                !.humanChoice = "feedback"
        ]
    /\ UNCHANGED
        <<
            phase,
            stage,
            cycle,
            attempt,
            invalidAttempt,
            gateKind,
            gateFromInvalidAttempt,
            activeNode,
            heldTarget,
            targetEditMode,
            proofEditMode,
            configuredTargets,
            approvedConfiguredTargets,
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
                nodeDifficulty,
                easyAttempts,
                humanInputOutstanding,
                nativeHistoryKinds,
                requestSeq,
            pendingTask,
            inFlightRequest,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED PromptCarryVars
    /\ UNCHANGED AllStructureVars
    /\ UNCHANGED LocalClosureVars

HumanApproveAdvance ==
    /\ stage = "HumanGate"
    /\ gateKind = "advance"
    /\ response.status = "ok"
    /\ response.kind = "human_gate"
    /\ response.humanChoice = "approve"
    /\ phase' = "proof_formalization"
    /\ stage' = "Start"
    /\ approvedConfiguredTargets' = configuredTargets
    /\ approvedCoverage' = currentCoverage
    /\ paperApprovedFp' = paperCurrentFp
    /\ approvedTargetFp' = currentTargetFp
    /\ coarseDagNodes' = presentNodes
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ pendingTask' = NoPendingTask
    /\ invalidAttempt' = FALSE
    /\ heldTarget' = NoNode
    /\ targetEditMode' = "global"
    /\ proofEditMode' = "local"
    /\ humanInputOutstanding' = FALSE
    /\ nodeDifficulty' = nodeDifficulty
    /\ easyAttempts' = easyAttempts
    \* Post-advance routing latch (kernel
    \* `apply_human_gate_response` GateKind::Advance::Approve in
    \* engine.rs). Cleared `activeNode` / `activeCoarseNode`; the next
    \* `start_cycle` issues a routing Reviewer so the reviewer can
    \* pick `next_active`, `next_active_coarse`, `must_close_active`,
    \* `allow_new_obligations`, `authorized_nodes`, and friends for
    \* the first burst of the new phase. Pre-seeding `activeCoarseNode'
    \* would defeat the routing review by emptying
    \* `KernelHintedNextActiveCoarseNodes`.
    /\ activeNode' = NoNode
    /\ activeCoarseNode' = NoNode
    /\ cyclesInCoarseRepairMode' = 0
    /\ postAdvanceRoutingPending' = TRUE
    /\ UNCHANGED PromptCarryVars
    /\ UNCHANGED
        <<
            cycle,
            attempt,
            configuredTargets,
            presentNodes,
            committedPresentNodes,
            openNodes,
            committedOpenNodes,
            localClosureUnverified,
            committedLocalClosureUnverified,
            currentCoverage,
            committedCoverage,
            currentTargetFp,
            committedTargetFp,
            paperStatus,
            paperCurrentFp,
            committedPaperCurrentFp,
            corrStatus,
            corrCurrentFp,
            committedCorrCurrentFp,
            corrApprovedFp,
            soundStatus,
            soundCurrentFp,
            committedSoundCurrentFp,
            soundApprovedFp,
            nodeDifficulty,
            easyAttempts,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED AllStructureVars
    /\ UNCHANGED LocalClosureVars
    /\ ClearArtifactsAndRecordHistory

HumanFeedbackAfterAdvance ==
    /\ stage = "HumanGate"
    /\ gateKind = "advance"
    /\ response.status = "ok"
    /\ response.kind = "human_gate"
    /\ response.humanChoice = "feedback"
    /\ phase' = "theorem_stating"
    /\ stage' = "Reviewer"
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ invalidAttempt' = FALSE
    /\ humanInputOutstanding' = TRUE
    /\ pendingTask' = NoPendingTask
    /\ nodeDifficulty' = nodeDifficulty
    /\ easyAttempts' = easyAttempts
    /\ UNCHANGED PromptCarryVars
    /\ UNCHANGED
        <<
            cycle,
            attempt,
            activeNode,
            heldTarget,
            targetEditMode,
            proofEditMode,
            configuredTargets,
            approvedConfiguredTargets,
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
            nodeDifficulty,
            easyAttempts,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED AllStructureVars
    /\ UNCHANGED LocalClosureVars
    /\ ClearArtifactsAndRecordHistory

HumanResolveNeedInput ==
    /\ stage = "HumanGate"
    /\ gateKind = "needinput"
    /\ response.status = "ok"
    /\ response.kind = "human_gate"
    /\ response.humanChoice \in {"approve", "feedback"}
    /\ stage' = "Reviewer"
    /\ gateKind' = "none"
    /\ gateFromInvalidAttempt' = FALSE
    /\ invalidAttempt' = gateFromInvalidAttempt
    /\ humanInputOutstanding' = TRUE
    /\ pendingTask' = NoPendingTask
    /\ nodeDifficulty' = nodeDifficulty
    /\ easyAttempts' = easyAttempts
    /\ UNCHANGED PromptCarryVars
    /\ UNCHANGED
        <<
            phase,
            cycle,
            attempt,
            activeNode,
            heldTarget,
            targetEditMode,
            proofEditMode,
            configuredTargets,
            approvedConfiguredTargets,
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
            nodeDifficulty,
            easyAttempts,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED AllStructureVars
    /\ UNCHANGED LocalClosureVars
    /\ ClearArtifactsAndRecordHistory

RejectInvalidHumanGateArtifact ==
    /\ stage = "HumanGate"
    /\ response.kind = "human_gate"
    /\ response.status = "malformed"
    /\ stage' = "HumanGate"
    /\ UNCHANGED
        <<
            phase,
            cycle,
            attempt,
            invalidAttempt,
            gateKind,
            gateFromInvalidAttempt,
            activeNode,
            heldTarget,
            targetEditMode,
            proofEditMode,
            configuredTargets,
            approvedConfiguredTargets,
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
            nodeDifficulty,
            easyAttempts,
            humanInputOutstanding,
            nativeHistoryKinds,
            cyclesSinceClean,
            hasEverBeenClean,
            forceReviewAfterConeClean
        >>
    /\ UNCHANGED CleanupV2Vars
    /\ UNCHANGED CoarseAnchorVars
    /\ UNCHANGED StuckMathAuditVars
    /\ UNCHANGED GlobalRepairVars
    /\ UNCHANGED PostAdvanceRoutingVars
    /\ UNCHANGED ProtectedReapprovalVars
    /\ UNCHANGED AuditPlanVars
    /\ UNCHANGED SoundAssessmentVars
    /\ UNCHANGED DeviationVars
    /\ UNCHANGED PromptCarryVars
    /\ UNCHANGED AllStructureVars
    /\ UNCHANGED LocalClosureVars
    /\ ClearArtifactsAndRecordHistory

RetryOutcomeKindNext ==
    IF /\ stage = "Start"
          /\ stage' # "Start"
    THEN
        "none"
    ELSE IF /\ stage = "Worker"
               /\ response.kind = "worker"
               /\ response.cycle = cycle
               /\ (response.status = "malformed"
                   \/ /\ response.status = "ok"
                      /\ WorkerFinalOutcome = "invalid")
    THEN
        "invalid"
    ELSE IF /\ stage = "Worker"
               /\ response.kind = "worker"
               /\ response.status = "ok"
               /\ response.cycle = cycle
               /\ WorkerFinalOutcome = "stuck"
    THEN
        "stuck"
    ELSE IF /\ stage = "Worker"
               /\ response.kind = "worker"
               /\ response.status = "ok"
               /\ response.cycle = cycle
               /\ WorkerFinalOutcome = "needs_restructure"
    THEN
        "needs_restructure"
    ELSE IF /\ stage \in {"VerifyPaper", "VerifyCorr", "VerifySound"}
               /\ response.status = "ok"
               /\ response.kind \in {"paper", "corr", "sound"}
               /\ response.cycle = cycle
    THEN
        "none"
    ELSE IF /\ stage = "Reviewer"
               /\ response.kind = "review"
               /\ response.status = "ok"
               /\ ReviewDecisionLegal
               /\ response.decision \in {"CONTINUE", "ADVANCE_PHASE", "DONE"}
    THEN
        "none"
    ELSE IF /\ stage = "HumanGate"
               /\ gateKind = "advance"
               /\ response.kind = "human_gate"
               /\ response.status = "ok"
               /\ response.humanChoice \in {"approve", "feedback"}
    THEN
        "none"
    ELSE
        retryOutcomeKind

\* The model does not represent worker-vs-supervisor checker mismatch as a
\* separate retry kind. It refines to the existing deterministic `invalid`
\* path: the supervisor workspace rerun is authoritative, any mismatch with the
\* worker-local checker result is logged/flagged outside the core state, and
\* the resulting worker response is reduced as an invalid attempt whose next
\* worker execution must be issued with fresh context.

\* ----------------------------------------------------------------------
\* Deviation lane env-driven mutators
\* ----------------------------------------------------------------------
\* Faithful refinement of the deviation lifecycle without re-threading
\* deviation primes through every existing accept/review action. The
\* kernel apply paths land deviation state changes inside worker bursts
\* (model.rs:5536-5664), verifier accepts (engine.rs:2447-2454), and
\* reviewer adjudication (model.rs:9498-9514, 9569-9577) — the spec
\* abstracts those into a small set of env-driven mutator actions that
\* fire non-deterministically whenever the gating preconditions hold.
\* The invariants in the closing section still constrain the result.
\*
\* All existing actions in CoreNext hold `UNCHANGED DeviationVars`, so
\* deviation state evolves only through these dedicated mutators. Each
\* mutator fires at any cycle (the spec doesn't tie them to a specific
\* stage — that's a kernel-level concern the spec abstracts away).

\* Worker burst creates a fresh deviation: registers a new id, seeds
\* status=Unknown / approvedFp=NoFingerprint / live fp = a chosen
\* Fingerprint, and auto-seeds `nodeDeviationClaims` for the affected
\* nodes (kernel model.rs:5536-5564). Gated on (a) the id being fresh
\* (no prior `deviationFiles` entry), (b) a chosen `affectedNodes`
\* subset of `presentNodes`. Cleanup / FinalCleanup workers are not
\* allowed to emit deviation requests (kernel
\* request_contracts.rs:1872-1888); spec gates on phase.
WorkerEmitDeviationRequest ==
    /\ inFlightRequest.kind = "none"
    /\ phase \in {"theorem_stating", "proof_formalization"}
    /\ Deviations # {}
    /\ \E id \in Deviations :
       \E affected \in SUBSET (presentNodes \ {NoNode}) :
       \E fp \in Fingerprints :
            /\ ~deviationFiles[id]
            /\ deviationFiles' = [deviationFiles EXCEPT ![id] = TRUE]
            /\ deviationStatus' = [deviationStatus EXCEPT ![id] = "unknown"]
            /\ deviationApprovedFp' = [deviationApprovedFp EXCEPT ![id] = NoFingerprint]
            /\ deviationCurrentFp' = [deviationCurrentFp EXCEPT ![id] = fp]
            /\ nodeDeviationClaims' =
                [n \in Nodes |->
                    IF n \in affected THEN
                        nodeDeviationClaims[n] \cup {id}
                    ELSE
                        nodeDeviationClaims[n]]
    /\ UNCHANGED CommittedDeviationVars
    /\ UNCHANGED LastCleanDeviationVars
    /\ UNCHANGED LatestDeviationReviewVars
    /\ UNCHANGED VarsWithoutRequest
    /\ UNCHANGED <<inFlightRequest, requestSeq>>

\* Worker burst re-issues a `deviation_requests` entry for an id ALREADY
\* in `deviationFiles` (kernel model.rs:5617-5641; mirror of the same
\* apply block as `WorkerEmitDeviationRequest`, but with the
\* `deviationFiles.get(id)` precondition flipped to "present"). The
\* kernel distinguishes two sub-cases based on whether the request's
\* `path` matches the stored path:
\*
\*   Same-path: `changed_path = false`. The kernel writes
\*   `deviation_files[id] := path` (idempotent), then does
\*   `deviation_status.entry(id).or_insert(Unknown)`. So the lane
\*   status is preserved (Pass / Fail / Unknown stays put), and
\*   `deviation_approved_fingerprints[id]` is untouched. The live
\*   fingerprint also does not move (the underlying TeX file is the
\*   same).
\*
\*   Path-change: `changed_path = true`. The kernel writes
\*   `deviation_status[id] := Unknown` and removes the entry from
\*   `deviation_approved_fingerprints`. The live fingerprint will
\*   eventually drift to the new file's content fingerprint — the spec
\*   models this drift as a fresh `Fingerprints` value bound atomically
\*   in the same step (in the kernel the drift is a separate
\*   `observe_deviation_fingerprints` runtime sweep, but at the
\*   protocol level the path-change → fingerprint-change correlation
\*   is total).
\*
\* In both sub-cases the kernel re-runs the affected-nodes auto-seed
\* loop (model.rs:5634-5641): for each node in
\* `request.affected_nodes ∩ live.present_nodes` the id is inserted
\* into `node_deviation_claims[node]`. Same rule as
\* `WorkerEmitDeviationRequest`.
\*
\* Phase gate is identical to `WorkerEmitDeviationRequest`
\* (request_contracts.rs:1872-1888 — Cleanup / FinalCleanup workers
\* don't carry the field). A single nondeterministic CHOICE captures
\* both sub-cases.
WorkerReissueDeviationRequest ==
    /\ inFlightRequest.kind = "none"
    /\ phase \in {"theorem_stating", "proof_formalization"}
    /\ Deviations # {}
    /\ \E id \in Deviations :
       \E affected \in SUBSET (presentNodes \ {NoNode}) :
       \E pathChanged \in BOOLEAN :
       \E fp \in Fingerprints :
            /\ deviationFiles[id]
            /\ deviationFiles' = deviationFiles
            /\ IF pathChanged THEN
                    \* Path-change sub-case: kernel resets status to
                    \* Unknown, clears the approved fingerprint, and the
                    \* live fingerprint drifts to a fresh value.
                    /\ deviationStatus' = [deviationStatus EXCEPT ![id] = "unknown"]
                    /\ deviationApprovedFp' =
                        [deviationApprovedFp EXCEPT ![id] = NoFingerprint]
                    /\ deviationCurrentFp' = [deviationCurrentFp EXCEPT ![id] = fp]
               ELSE
                    \* Same-path sub-case: kernel does
                    \* `deviation_status.entry(id).or_insert(Unknown)`,
                    \* leaving any existing Pass/Fail/Unknown in place;
                    \* approvedFp and currentFp are untouched. The
                    \* `or_insert` only fires if the id had no prior
                    \* status entry — in the spec `deviationStatus` is a
                    \* total function over Deviations so this is a true
                    \* no-op on the status/fp triple.
                    /\ UNCHANGED <<deviationStatus, deviationApprovedFp, deviationCurrentFp>>
            /\ nodeDeviationClaims' =
                [n \in Nodes |->
                    IF n \in affected THEN
                        nodeDeviationClaims[n] \cup {id}
                    ELSE
                        nodeDeviationClaims[n]]
    /\ UNCHANGED CommittedDeviationVars
    /\ UNCHANGED LastCleanDeviationVars
    /\ UNCHANGED LatestDeviationReviewVars
    /\ UNCHANGED VarsWithoutRequest
    /\ UNCHANGED <<inFlightRequest, requestSeq>>

\* Verifier panel returns a Pass / Fail decision for a deviation
\* currently in the verify frontier (Unknown). Mirror of kernel
\* `apply_deviation_updates` (engine.rs:5424-5443) plus the
\* `latest_deviation_review_ids` write at engine.rs:2541. On Pass,
\* `deviationApprovedFp` pins to the live fingerprint (non-empty
\* only).
EnvDeviationVerifierVerdict ==
    /\ inFlightRequest.kind = "none"
    /\ Deviations # {}
    /\ \E id \in Deviations :
       \E verdict \in {"pass", "fail"} :
            /\ deviationFiles[id]
            /\ CurrentDeviationUnknown(id)
            /\ deviationCurrentFp[id] # NoFingerprint
            /\ deviationStatus' = [deviationStatus EXCEPT ![id] = verdict]
            /\ deviationApprovedFp' =
                [deviationApprovedFp EXCEPT ![id] = deviationCurrentFp[id]]
            /\ latestDeviationReviewIds' = {id}
            /\ latestDeviationEvidenceLanes' = VerifierLanes
    /\ UNCHANGED <<deviationFiles, deviationCurrentFp, nodeDeviationClaims>>
    /\ UNCHANGED CommittedDeviationVars
    /\ UNCHANGED LastCleanDeviationVars
    /\ UNCHANGED VarsWithoutRequest
    /\ UNCHANGED <<inFlightRequest, requestSeq>>

\* Worker re-edits the deviation reference file, shifting the live
\* fingerprint. Mirror of the kernel's
\* `observe_deviation_fingerprints` runtime call followed by a
\* `normalize_live_structural_state` pass. Sticky-Fail (efaafa7,
\* model.rs:6582-6594): when status is Fail and live fp drifts off
\* the approved fp, the kernel's `current_deviation_state` reads
\* Unknown via the equality check — the spec's `CurrentDeviationState`
\* helper mirrors this directly, so the env action need only assign
\* the new live fp.
EnvDeviationFingerprintDrift ==
    /\ inFlightRequest.kind = "none"
    /\ Deviations # {}
    /\ \E id \in Deviations :
       \E fp \in Fingerprints :
            /\ deviationFiles[id]
            /\ fp # deviationCurrentFp[id]
            /\ deviationCurrentFp' = [deviationCurrentFp EXCEPT ![id] = fp]
    /\ UNCHANGED <<deviationFiles, deviationStatus, deviationApprovedFp, nodeDeviationClaims>>
    /\ UNCHANGED CommittedDeviationVars
    /\ UNCHANGED LastCleanDeviationVars
    /\ UNCHANGED LatestDeviationReviewVars
    /\ UNCHANGED VarsWithoutRequest
    /\ UNCHANGED <<inFlightRequest, requestSeq>>

\* Worker retires a deviation (kernel commit 4abe9dd,
\* `WorkerResponse.deviation_deletions`, model.rs:5654-5664). Contract
\* check: no node may still claim the id after the response's claim
\* updates are notionally applied. The spec models this gate as
\* "all per-node claims of the id are dropped IN THIS STEP". Apply
\* removes the id from `deviationFiles`, `deviationStatus`,
\* `deviationApprovedFp`, `deviationCurrentFp`,
\* `latestDeviationReviewIds`. The kernel does NOT touch the
\* filesystem — file removal is the worker's responsibility off-spec.
\*
\* Phase gate: Cleanup / FinalCleanup workers don't see deviation
\* deletion in their prompt schema (kernel
\* request_contracts.rs:1872-1888 — the field is dormant for
\* Cleanup), so the spec gates on
\* phase \in {theorem_stating, proof_formalization}.
WorkerRetireDeviation ==
    /\ inFlightRequest.kind = "none"
    /\ phase \in {"theorem_stating", "proof_formalization"}
    /\ Deviations # {}
    /\ \E id \in Deviations :
            /\ deviationFiles[id]
            /\ deviationFiles' = [deviationFiles EXCEPT ![id] = FALSE]
            /\ deviationStatus' = [deviationStatus EXCEPT ![id] = "unknown"]
            /\ deviationApprovedFp' = [deviationApprovedFp EXCEPT ![id] = NoFingerprint]
            /\ deviationCurrentFp' = [deviationCurrentFp EXCEPT ![id] = NoFingerprint]
            /\ nodeDeviationClaims' =
                [n \in Nodes |-> nodeDeviationClaims[n] \ {id}]
            /\ latestDeviationReviewIds' = latestDeviationReviewIds \ {id}
    /\ UNCHANGED latestDeviationEvidenceLanes
    /\ UNCHANGED CommittedDeviationVars
    /\ UNCHANGED LastCleanDeviationVars
    /\ UNCHANGED VarsWithoutRequest
    /\ UNCHANGED <<inFlightRequest, requestSeq>>

\* Worker updates the per-node claims map without changing the
\* deviation registry. Kernel: `node_deviation_claims` field on
\* `WorkerResponse` (model.rs:3990). The spec just permits any
\* TypeOK-legal claim map update. Gates on phase the same way as
\* WorkerEmitDeviationRequest.
WorkerUpdateDeviationClaims ==
    /\ inFlightRequest.kind = "none"
    /\ phase \in {"theorem_stating", "proof_formalization"}
    /\ \E newClaims \in [Nodes -> SUBSET Deviations] :
            /\ \A n \in Nodes :
                /\ newClaims[n] \subseteq {id \in Deviations : deviationFiles[id]}
                /\ (n \notin presentNodes) => newClaims[n] = {}
            /\ newClaims # nodeDeviationClaims
            /\ nodeDeviationClaims' = newClaims
    /\ UNCHANGED <<deviationFiles, deviationStatus, deviationCurrentFp, deviationApprovedFp>>
    /\ UNCHANGED CommittedDeviationVars
    /\ UNCHANGED LastCleanDeviationVars
    /\ UNCHANGED LatestDeviationReviewVars
    /\ UNCHANGED VarsWithoutRequest
    /\ UNCHANGED <<inFlightRequest, requestSeq>>

CoreNext ==
    \/ StartCycle
    \/ EnvEditConfiguredTargets
    \/ EnvRescindApprovedAxiom
    \/ IssueWorkerRequest
    \/ EnvStageWorkerValid
    \/ EnvStageWorkerInvalid
    \/ EnvStageWorkerStuck
    \/ EnvStageWorkerMalformed
    \/ AcceptValidWorkerTheorem
    \/ AcceptInvalidWorkerTheorem
    \/ AcceptStuckWorkerTheorem
    \/ AcceptNeedsRestructureWorkerTheorem
    \/ AcceptValidWorkerProof
    \/ AcceptInvalidWorkerProofRetry
    \/ AcceptInvalidWorkerProofEscalate
    \/ AcceptStuckWorkerProofRetry
    \/ AcceptStuckWorkerProofEscalate
    \/ AcceptNeedsRestructureWorkerProof
    \/ AcceptValidWorkerCleanup
    \/ AcceptInvalidWorkerCleanup
    \/ IssuePaperRequest
    \/ EnvStagePaperArtifact
    \/ EnvStagePaperMalformed
    \/ AcceptPaperArtifactTheorem
    \/ AcceptSubstantivenessArtifactTheorem
    \/ AcceptPaperArtifactProof
    \/ AcceptSubstantivenessArtifactProof
    \/ IssueCorrRequest
    \/ EnvStageCorrArtifact
    \/ EnvStageCorrMalformed
    \/ AcceptCorrArtifactTheorem
    \/ AcceptCorrArtifactProof
    \/ IssueSoundRequest
    \/ EnvStageSoundArtifact
    \/ EnvStageSoundMalformed
    \/ AcceptSoundArtifactTheorem
    \/ AcceptSoundArtifactProof
    \/ IssueReviewRequest
    \/ EnvStageReviewArtifact
    \/ RejectInvalidReviewArtifact
    \/ ReviewContinueAfterInvalid
    \/ ReviewNeedInputAfterInvalid
    \/ ReviewContinueAfterValid
    \/ ReviewNeedInputAfterValid
    \/ ReviewAdvancePhase
    \/ ReviewContinueProof
    \/ ReviewNeedInputProof
    \/ ReviewContinueCleanup
    \/ ReviewNeedInputCleanup
    \/ ReviewDoneCleanup
    \/ IssueHumanGateRequest
    \/ EnvStageHumanApprove
    \/ EnvStageHumanFeedback
    \/ RejectInvalidHumanGateArtifact
    \/ HumanApproveAdvance
    \/ HumanFeedbackAfterAdvance
    \/ HumanResolveNeedInput
    \* Cleanup-v2 audit lane (plan §16/§3, 2026-05-14).
    \/ IssueCleanupAuditRequest
    \/ AcceptCleanupAuditNeedToContinue
    \/ AcceptCleanupAuditDone
    \/ ReviewerCleanupDismissAndDispatch
    \/ ReviewerCleanupReAudit
    \* StuckMathAudit dispatch / response (2026-05-31). Mirrors kernel
    \* `route_need_input_to_auditor` + `apply_stuck_math_audit_response`
    \* + `retry_or_transition_stuck_math_audit_to_reviewer`.
    \/ IssueStuckMathAuditRequest
    \/ AcceptStuckMathAuditDispatchHumanGate
    \/ AcceptStuckMathAuditBackToReviewer
    \/ AcceptStuckMathAuditConeClean
    \/ AcceptStuckMathAuditRetry
    \/ AcceptStuckMathAuditRetryExhaustedBackToReviewer
    \/ AcceptStuckMathAuditRetryExhaustedDispatchHumanGate
    \* Reviewer-side audit-plan mutations and protected-target
    \* reapproval consume mirror (kernel
    \* `apply_review_audit_plan_actions`, `maybe_issue_protected_reapproval`,
    \* `apply_human_gate_response` GateKind::ProtectedReapproval).
    \/ RecordAuditPlan
    \/ DismissAuditPlanTask
    \/ MaybeIssueProtectedReapproval
    \/ HumanApproveProtectedReapproval
    \/ HumanFeedbackProtectedReapproval
    \* global_repair_mode (2026-06-05). Audit-gated cone-widening
    \* mechanism. See `global_repair_mode` cluster comment for the
    \* full Step A / B / C lifecycle.
    \/ RequestGlobalRepairAudit
    \/ ApplyStuckMathAuditGlobalRepairResponse
    \/ ConsumeGlobalRepairGrant
    \* Deviation lane env-driven mutators (kernel commits 7aad7cb /
    \* 4e83783 / efaafa7 / 4abe9dd).
    \/ WorkerEmitDeviationRequest
    \/ WorkerReissueDeviationRequest
    \/ EnvDeviationVerifierVerdict
    \/ EnvDeviationFingerprintDrift
    \/ WorkerRetireDeviation
    \/ WorkerUpdateDeviationClaims

Next ==
    /\ CoreNext
    /\ retryOutcomeKind' = RetryOutcomeKindNext

Spec == Init /\ [][Next]_Vars

\* ----------------------------------------------------------------------
\* Patch C — §7.12 stochastic scenario sketch (TLA-level §1.1 happy path)
\* ----------------------------------------------------------------------
\*
\* Concrete trace mirroring `LOCAL_CLOSURE_IMPL_PLAN.md` §1.1 + §7.12 #13
\* in TLA terms. Not implemented as a runnable TLA proof obligation —
\* sketch left as documentation so a future spec contributor can lift it
\* into an explicit `\E trace : ...` predicate or a TLC-state-trace
\* fixture.
\*
\* Setup: phase = "proof_formalization", presentNodes \supseteq {A, B},
\* {A, B} \subseteq currentProofNodes. A's `.lean` and B's `.lean` are
\* sorry-free (i.e. neither in openNodes); A locally closed against B's
\* prior statement (record present in the kernel; modeled here as the
\* fact that A is NOT in localClosureUnverified).
\*
\* Step 1 — TheoremStating-style edit invalidates the consumer.
\*   AcceptValidWorkerTheorem fires with response that mutates B's
\*   statement (a `currentDeps`/structure delta). The action's
\*   `localClosureUnverified' \in LocalClosureUnverifiedNeighbors(...)`
\*   admits a successor where A enters localClosureUnverified.
\*
\* Step 2 — gate-check time.
\*   At any subsequent stage transition, `FormalizationComplete` is
\*   evaluated. With A \in localClosureUnverified \cap currentProofNodes,
\*   the new clause forces FormalizationComplete = FALSE, even though
\*   ProofComplete (textual) is TRUE. The four phase-flip sites
\*   (lines 7387, 7734, 8475, 8639 — at the time of writing) all use
\*   `IF FormalizationComplete THEN <flip to "cleanup">`, so the
\*   cleanup transition does NOT fire.
\*   The invariant
\*   `StalePassClosurePreventsCleanupTransition` is the spec-level
\*   capture of this: if any proof_node lies in localClosureUnverified,
\*   FormalizationComplete is false.
\*
\* Step 3 — revalidation refresh exits A.
\*   A subsequent worker burst on A (legal because A \in
\*   localClosureUnverified — see ActiveNodeLegal extension) accepts.
\*   AcceptValidWorkerProof's `localClosureUnverified' \in
\*   LocalClosureUnverifiedNeighbors(...)` admits a successor where A
\*   exits localClosureUnverified.
\*   Now FormalizationComplete = TRUE and the next phase flip site
\*   transitions to "cleanup".
\*
\* This trace uses three actions —
\*   AcceptValidWorkerTheorem (or AcceptValidWorkerProof on B's edit),
\*   any FormalizationComplete-evaluating action while
\*   `localClosureUnverified \cap currentProofNodes # {}`,
\*   AcceptValidWorkerProof (on A) —
\* and exercises the invalidation, gate-block, refresh cycle that
\* §1.1 says the kernel must implement to close the stale-pass gap.
\*
\* The §7.12 kernel test list (#13 specifically) maps to:
\*   #13 phase-completion gate blocks on unverified — covered by
\*       StalePassClosurePreventsCleanupTransition + the
\*       FormalizationComplete extension.
\*   #18 deterministic revalidation succeeds — covered by Step 3 above.
\*   #19 request_allowed_next_active_nodes includes unverified-only
\*       node — covered by the ActiveNodeLegal extension.
\*
\* Patch A and Patch B do NOT require state-machine changes here:
\* the spec models the MCA gate symbolically in `ActiveNodeLegal`
\* (the `node \in liveOpen` clause already abstracts "needs work";
\* Patch B narrows that on the kernel side without changing the
\* abstract structure). Patch C's localClosureUnverified extension
\* is the structural change.
\* ----------------------------------------------------------------------

=============================================================================
