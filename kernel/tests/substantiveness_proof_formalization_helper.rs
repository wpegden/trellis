// Pin the substantiveness lane fires in ProofFormalization so that
// helper nodes added by Hard-mode proof-formalization restructure are
// checked for substantiveness. Today (post-change) the lane fires in both
// TheoremStating and ProofFormalization; before this change it
// short-circuited to Pass outside TheoremStating, leaving Hard-restructure
// helpers unchecked.
//
// Pinned properties:
//   - `substantiveness_verify_nodes()` includes helper nodes whose
//     substantiveness state is Unknown in ProofFormalization.
//   - Unknown alone is not a Fail blocker.
//   - A Substantiveness Fail verdict (status=Fail with matching
//     fingerprints) produces a Substantiveness blocker via
//     `global_blockers()` in ProofFormalization.
//   - That blocker rejects `AdvancePhase` via
//     `WrapperRequest::review_response_legal`.
//
// Lives as an integration test (matches the pattern set by
// `kernel/tests/substantiveness_advance_phase_gate.rs` because the
// kernel's `mod tests` lib block is gated by pre-existing K-8
// NodeId/TargetId migration breakage).

use std::collections::{BTreeMap, BTreeSet};

use trellis_kernel::{
    Blocker, BlockerKind, BlockerObject, CorrStatus, NodeId, Phase, ProtocolState, RequestKind,
    ResetChoice, ResponseStatus, ReviewDecisionKind, ReviewResponse, SoundStatus, TaskMode,
    WorkerContextMode, WorkerWorkStyleHint,
};

fn nid(s: &str) -> NodeId {
    NodeId::from(s)
}

/// Build a ProofFormalization state with two proof nodes (`X` from
/// theorem-stating, `Helper_H` newly added) plus the Preamble. Corr +
/// Sound are clean for both; substantiveness for `X` is Pass with
/// matching fingerprints. `Helper_H`'s substantiveness fingerprint is
/// seeded but no status is set, so the lane is Unknown — this models a
/// helper just emitted by a Hard-mode restructure that has not yet been
/// verified.
fn proof_state_with_helper_substantiveness_unknown() -> ProtocolState {
    let mut state = ProtocolState::default();
    state.phase = Phase::ProofFormalization;
    state.live.present_nodes = BTreeSet::from([nid("Preamble"), nid("X"), nid("Helper_H")]);
    state.proof_nodes = BTreeSet::from([nid("X"), nid("Helper_H")]);

    // No configured_targets => paper-target lane vacuously Pass.

    // Corr: Preamble + X + Helper_H all Pass with matching fingerprints.
    for n in [nid("Preamble"), nid("X"), nid("Helper_H")] {
        state.corr_status.insert(n.clone(), CorrStatus::Pass);
        state
            .live
            .corr_current_fingerprints
            .insert(n.clone(), "cfp".to_string());
        state
            .corr_approved_fingerprints
            .insert(n, "cfp".to_string());
    }

    // Sound: no open_nodes => `needs_sound` is false for X and Helper_H,
    // so the sound lane is vacuously Pass. We seed Pass anyway to mirror
    // the pattern used in the existing advance-phase-gate test.
    for n in [nid("X"), nid("Helper_H")] {
        state.sound_status.insert(n.clone(), SoundStatus::Pass);
        state
            .live
            .sound_current_fingerprints
            .insert(n.clone(), "sfp".to_string());
        state
            .sound_approved_fingerprints
            .insert(n, "sfp".to_string());
    }

    // Substantiveness: X is Pass with matching fingerprints. Helper_H
    // has a current_fingerprint but no status entry => Unknown.
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
    state
        .live
        .substantiveness_current_fingerprints
        .insert(nid("Helper_H"), "fpH".to_string());
    // No status / approved_fp for Helper_H => Unknown.

    state
}

#[test]
fn substantiveness_verify_nodes_includes_helper_in_proof_formalization() {
    // Direction 1: the substantiveness frontier fires in
    // ProofFormalization. The helper node must appear; X (already Pass)
    // must not.
    let state = proof_state_with_helper_substantiveness_unknown();
    let frontier = state.substantiveness_verify_nodes();
    assert!(
        frontier.contains(&nid("Helper_H")),
        "Helper_H is substantiveness=Unknown in ProofFormalization; \
         it must appear on the substantiveness frontier; got {:?}",
        frontier
    );
    assert!(
        !frontier.contains(&nid("X")),
        "X is substantiveness=Pass; must NOT appear on the frontier; got {:?}",
        frontier
    );
}

#[test]
fn substantiveness_unknown_alone_is_not_a_blocker() {
    // Unknown isn't a Fail; the verifier resolves it. `global_blockers`
    // does include Unknown nodes (since `current_substantiveness_pass`
    // is false for them), but the failed-blockers set only contains the
    // nodes whose state is *Fail*. The Unknown helper produces a
    // non-failed blocker — let's verify the failed-blockers set does
    // not contain a Substantiveness Fail blocker for Helper_H.
    let state = proof_state_with_helper_substantiveness_unknown();
    let failed = state.current_failed_blockers();
    let helper_substantiveness_fail = Blocker {
        kind: BlockerKind::Substantiveness,
        object: BlockerObject::Node {
            node: nid("Helper_H"),
        },
        fingerprint: "fpH".to_string(),
        deferred: false,
    };
    assert!(
        !failed.contains(&helper_substantiveness_fail),
        "Helper_H Unknown must not appear in current_failed_blockers; got {:?}",
        failed
    );
}

#[test]
fn substantiveness_fail_for_helper_blocks_advance_phase_in_proof_formalization() {
    // Direction 2: a Substantiveness Fail verdict on the helper must
    // surface as a Substantiveness blocker in `global_blockers()` in
    // ProofFormalization, and that blocker must reject AdvancePhase via
    // `review_response_legal`.
    let mut state = proof_state_with_helper_substantiveness_unknown();
    // Promote Helper_H from Unknown to Fail with matching fingerprints
    // (i.e. the verifier landed a Fail and the kernel advanced the
    // approved fingerprint to match the current one — the standard
    // Fail-with-matching-fp shape that produces a current Fail blocker).
    state
        .substantiveness_status
        .insert(nid("Helper_H"), CorrStatus::Fail);
    state
        .substantiveness_approved_fingerprints
        .insert(nid("Helper_H"), "fpH".to_string());

    // Confirm `global_blockers()` carries a Substantiveness blocker for
    // Helper_H.
    let blockers = state.global_blockers();
    let helper_blocker = Blocker {
        kind: BlockerKind::Substantiveness,
        object: BlockerObject::Node {
            node: nid("Helper_H"),
        },
        fingerprint: "fpH".to_string(),
        deferred: false,
    };
    assert!(
        blockers.contains(&helper_blocker),
        "Substantiveness Fail on Helper_H must produce a global blocker in ProofFormalization; got {:?}",
        blockers
    );

    // And AdvancePhase must be rejected.
    let request = state.expected_request(0, RequestKind::Review);
    assert!(
        request
            .blockers
            .iter()
            .any(|b| b.kind == BlockerKind::Substantiveness && b.object == helper_blocker.object),
        "expected request blockers must carry the Substantiveness Fail; got {:?}",
        request.blockers
    );
    let response = ReviewResponse {
        request_id: request.id,
        cycle: state.cycle,
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
    };
    assert!(
        !request.review_response_legal(&response),
        "AdvancePhase must be rejected when a Substantiveness Fail blocker is present in ProofFormalization",
    );
}
