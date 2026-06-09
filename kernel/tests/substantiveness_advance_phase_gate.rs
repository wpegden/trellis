// Regression test pinning the wiring between `BlockerKind::Substantiveness`
// and `WrapperRequest::review_response_legal::AdvancePhase`.
//
// Audit `/tmp/paper_node_faithfulness_audit.md` §2.5 verified the gating is
// correct today but flagged that no test exercises it. A future refactor
// that drops Substantiveness from `global_blockers()` — or splits blockers
// across the action buckets in a way that excludes it from the
// AdvancePhase gate — would silently break a critical safety property:
// phase-advance from TheoremStating with an outstanding Substantiveness
// Fail.
//
// Two directions are pinned:
//   - blocker present  => `review_response_legal(AdvancePhase) == false`
//   - blocker resolved => `review_response_legal(AdvancePhase) == true`
//
// Lives as an integration test because the kernel's lib `mod tests`
// modules are blocked by pre-existing K-8 NodeId/TargetId migration
// breakage (matches the Phase-B pattern set by
// `kernel/tests/substantiveness_validator.rs` and
// `kernel/tests/substantiveness_normalizer.rs`).

use std::collections::{BTreeMap, BTreeSet};

use trellis_kernel::{
    Blocker, BlockerKind, BlockerObject, CorrStatus, NodeId, Phase, ProtocolState, RequestKind,
    ResetChoice, ResponseStatus, ReviewDecisionKind, ReviewResponse, SoundStatus, TaskMode,
    WorkerContextMode, WorkerWorkStyleHint,
};

fn nid(s: &str) -> NodeId {
    NodeId::from(s)
}

/// Build a TheoremStating state with a single proof node `X` (plus the
/// mandatory `Preamble`). All non-substantiveness lanes are clean for X
/// and Preamble. Caller seeds the substantiveness lane to choose
/// blocker-present vs blocker-resolved.
fn theorem_state_clean_except_substantiveness() -> ProtocolState {
    let mut state = ProtocolState::default();
    state.phase = Phase::TheoremStating;
    state.live.present_nodes = BTreeSet::from([nid("Preamble"), nid("X")]);
    state.proof_nodes = BTreeSet::from([nid("X")]);
    // No configured_targets => paper-target lane vacuously Pass.
    // No open_nodes => `needs_sound` returns false for X => sound lane
    // skipped. We still seed Sound=Pass to mirror the pattern in the
    // existing `last_clean_reset_restores_*` test.
    state.sound_status.insert(nid("X"), SoundStatus::Pass);
    state
        .live
        .sound_current_fingerprints
        .insert(nid("X"), "sfp".to_string());
    state
        .sound_approved_fingerprints
        .insert(nid("X"), "sfp".to_string());

    // Corr: Preamble + X both Pass with matching fingerprints.
    for n in [nid("Preamble"), nid("X")] {
        state.corr_status.insert(n.clone(), CorrStatus::Pass);
        state
            .live
            .corr_current_fingerprints
            .insert(n.clone(), "cfp".to_string());
        state
            .corr_approved_fingerprints
            .insert(n, "cfp".to_string());
    }

    state
}

/// Seed substantiveness=Fail for X with matching fingerprints. Produces
/// a definite Substantiveness Fail blocker in `global_blockers()`.
fn seed_substantiveness_fail(state: &mut ProtocolState) {
    state
        .substantiveness_status
        .insert(nid("X"), CorrStatus::Fail);
    state
        .live
        .substantiveness_current_fingerprints
        .insert(nid("X"), "fpX".to_string());
    state
        .substantiveness_approved_fingerprints
        .insert(nid("X"), "fpX".to_string());
}

/// Seed substantiveness=Pass for X with matching fingerprints. Yields
/// an empty `global_blockers()`.
fn seed_substantiveness_pass(state: &mut ProtocolState) {
    state
        .substantiveness_status
        .insert(nid("X"), CorrStatus::Pass);
    state
        .live
        .substantiveness_current_fingerprints
        .insert(nid("X"), "fpX".to_string());
    state
        .substantiveness_approved_fingerprints
        .insert(nid("X"), "fpX".to_string());
}

/// Build a baseline AdvancePhase ReviewResponse with empty
/// task/override/reset blocker action buckets and otherwise valid
/// fields. The state's blockers determine whether
/// `review_response_legal` accepts or rejects this response.
fn advance_phase_response(request_id: u32, cycle: u32) -> ReviewResponse {
    ReviewResponse {
        request_id,
        cycle,
        status: ResponseStatus::Ok,
        decision: ReviewDecisionKind::AdvancePhase,
        reason: String::new(),
        comments: String::new(),
        task_blockers: BTreeSet::new(),
        override_blockers: BTreeSet::new(),
        reset_blockers: BTreeSet::new(),
        request_sound_verifier_nodes: BTreeSet::new(),
        next_active: None,
        next_active_coarse: None,
        reset: ResetChoice::None,
        reset_node: None,
        next_mode: TaskMode::default(),
        difficulty_updates: BTreeMap::new(),
        clear_human_input: false,
        next_worker_context_mode: WorkerContextMode::default(),
        paper_focus_ranges: Vec::new(),
        work_style_hint: WorkerWorkStyleHint::default(),
        protected_semantic_change_nodes: BTreeSet::new(),
        confirm_protected_semantic_change_scope: false,
        global_repair_request: None,
        consume_global_repair_grant: false,
        authorized_nodes: BTreeSet::new(),
        allow_new_obligations: true,
        must_close_active: false,
        cleanup_dismiss_tasks: Vec::new(),
        cleanup_next_task: None,
        cleanup_request_reaudit: false,
        paper_grounding: trellis_kernel::PaperGrounding::default(),
        stuck_math_audit: None,
        dismiss_audit_plan: false,
        dismissed_tasks: Vec::new(),
    }
}

#[test]
fn substantiveness_fail_blocker_blocks_advance_phase() {
    // Direction 1: a Substantiveness Fail blocker must reject
    // AdvancePhase via `review_response_legal`'s
    // `self.blockers.is_empty()` AdvancePhase clause. If a future refactor
    // drops Substantiveness from `global_blockers()`, this test fires.
    let mut state = theorem_state_clean_except_substantiveness();
    seed_substantiveness_fail(&mut state);

    let request = state.expected_request(0, RequestKind::Review);

    // Test prerequisite: the only blocker present must be a Substantiveness
    // Fail blocker on X. If anything else leaks in, the test isn't pinning
    // the property we care about.
    let expected_blocker = Blocker {
        kind: BlockerKind::Substantiveness,
        object: BlockerObject::Node { node: nid("X") },
        fingerprint: "fpX".to_string(),
        deferred: false,
    };
    assert_eq!(
        request.blockers,
        BTreeSet::from([expected_blocker]),
        "test prerequisite: only blocker must be Substantiveness Fail on X; got {:?}",
        request.blockers,
    );

    let response = advance_phase_response(request.id, state.cycle);
    assert!(
        !request.review_response_legal(&response),
        "AdvancePhase must be rejected when a Substantiveness Fail blocker is present",
    );
}

#[test]
fn substantiveness_pass_permits_advance_phase() {
    // Direction 2: with the Substantiveness blocker resolved (status=Pass
    // with matching fingerprints) and no other blockers,
    // `review_response_legal` must accept AdvancePhase. If a future refactor
    // adds an unrelated kind of "Substantiveness blocker" that survives
    // resolution, this test fires.
    let mut state = theorem_state_clean_except_substantiveness();
    seed_substantiveness_pass(&mut state);

    let request = state.expected_request(0, RequestKind::Review);

    // Test prerequisite: clean state, no blockers.
    assert!(
        request.blockers.is_empty(),
        "test prerequisite: state must be clean; got {:?}",
        request.blockers,
    );
    // And AdvancePhase must be a permitted decision in this state (else we
    // would be testing a different rejection path).
    assert!(
        request
            .allowed_decisions
            .contains(&ReviewDecisionKind::AdvancePhase),
        "test prerequisite: AdvancePhase must be in allowed_decisions; got {:?}",
        request.allowed_decisions,
    );

    let response = advance_phase_response(request.id, state.cycle);
    assert!(
        request.review_response_legal(&response),
        "AdvancePhase must be accepted when Substantiveness is resolved and no other blockers exist",
    );
}
