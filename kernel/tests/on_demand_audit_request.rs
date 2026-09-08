//! On-demand "call for an audit" end-to-end integration fixture.
//!
//! Drives the kernel's public `apply_event` API — the same surface the
//! supervisor drives — through the on-demand audit lifecycle:
//!
//!   reviewer Continue + audit_request  -> StuckMathAudit dispatched this turn
//!   auditor response (report + tasks)  -> audit_plan written, returns to Reviewer
//!   immediate re-request               -> rejected on cooldown
//!
//! The per-decision legality, phase-gating, mutex deferral, allowlist
//! re-emit, and worker-advisory paths are unit-covered in the lib `mod
//! tests` of `model.rs` / `engine.rs`. This fixture pins the chained flow.
//!
//! No external supervisor / codex / synthetic harness; no host `lake`;
//! a SMALL synthetic state built through the public API.

use std::collections::{BTreeMap, BTreeSet};

use trellis_kernel::engine::{apply_event, ProtocolCommand, ProtocolEvent};
use trellis_kernel::{
    AuditRequest, AuditRequestReasonKind, CorrStatus, Fingerprint, NodeId, Phase, ProtocolState,
    RequestKind, ResponseStatus, ReviewDecisionKind, ReviewResponse, Stage, StuckMathAuditResponse,
    WorkingSnapshot, WrapperRequest, WrapperResponse, AUDIT_REPORT_TEXT_MIN_CHARS,
};

fn nid(s: &str) -> NodeId {
    NodeId::from(s)
}

/// A small clean ProofFormalization reviewer state that admits an
/// on-demand audit (no audit lane in flight, audits admitted, no cooldown).
fn proof_review_state(cycle: u32) -> ProtocolState {
    let a = nid("A");
    let mut state = ProtocolState {
        phase: Phase::ProofFormalization,
        stage: Stage::Reviewer,
        cycle,
        // No active node: keeps the synthetic post-audit Reviewer state
        // legal at validate-entry without a full last_clean mirror set.
        active_node: None,
        proof_nodes: BTreeSet::from([a.clone()]),
        coarse_dag_nodes: BTreeSet::from([a.clone()]),
        active_coarse_node: Some(a.clone()),
        deps: BTreeMap::from([(a.clone(), BTreeSet::new())]),
        live: WorkingSnapshot {
            present_nodes: BTreeSet::from([a.clone()]),
            ..WorkingSnapshot::default()
        },
        ..ProtocolState::default()
    };
    // A is open (carries a live blocker) so the StuckMathAudit latch stays
    // active and the auditor's plan is not immediately moved to superseded
    // by the blocker-free latch clear.
    state.live.open_nodes = BTreeSet::from([a.clone()]);
    state.corr_status.insert(a.clone(), CorrStatus::Unknown);
    state
        .live
        .corr_current_fingerprints
        .insert(a.clone(), Fingerprint::from("corr".to_string()));
    state
        .substantiveness_status
        .insert(a.clone(), trellis_kernel::SubstantivenessStatus::Unknown);
    state
        .live
        .substantiveness_current_fingerprints
        .insert(a.clone(), Fingerprint::default());
    state.committed = state.live.clone();
    state.committed_proof_nodes = state.proof_nodes.clone();
    state.committed_deps = state.deps.clone();
    state
}

fn audit_request_continue(state: &ProtocolState, reason: &str) -> ReviewResponse {
    let request = state.expected_request(1, RequestKind::Review);
    ReviewResponse {
        request_id: request.id,
        cycle: state.cycle,
        status: ResponseStatus::Ok,
        decision: ReviewDecisionKind::Continue,
        next_active_coarse: state.active_coarse_node.clone(),
        allow_new_obligations: true,
        must_close_active: false,
        audit_request: Some(AuditRequest {
            reason_kind: AuditRequestReasonKind::Approach,
            reason: reason.to_string(),
        }),
        ..ReviewResponse::default()
    }
}

fn first_issued(commands: &[ProtocolCommand]) -> &WrapperRequest {
    commands
        .iter()
        .find_map(|c| match c {
            ProtocolCommand::IssueRequest { request } => Some(request),
            _ => None,
        })
        .expect("an IssueRequest command")
}

#[test]
fn reviewer_request_dispatches_audit_then_plan_returns_to_reviewer_and_cooldown_blocks_rerequest() {
    let mut state = proof_review_state(30);
    let response = audit_request_continue(&state, "the approach cannot close");
    assert!(
        state.review_response_legal(&response),
        "a non-acting audit_request Continue must be legal"
    );
    // Issue the in-flight review request the response answers.
    let _ = state.issue_request(RequestKind::Review);

    // Step 1: reviewer request -> StuckMathAudit dispatched this turn.
    let dispatched = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: WrapperResponse::Review(response),
        },
    )
    .expect("audit_request review should dispatch a StuckMathAudit");
    assert_eq!(dispatched.state.stage, Stage::StuckMathAudit);
    let audit_req = first_issued(&dispatched.commands).clone();
    assert_eq!(audit_req.kind, RequestKind::StuckMathAudit);
    assert_eq!(
        dispatched.state.stuck_math_audit.trigger,
        "reviewer requested audit (approach): the approach cannot close"
    );
    assert!(dispatched.state.pending_audit_request.is_none());
    assert_eq!(dispatched.state.last_audit_request_cycle, Some(30));

    // Step 2: the auditor produces a plan -> audit_plan written, returns to Reviewer.
    let audit_response = StuckMathAuditResponse {
        request_id: audit_req.id,
        cycle: audit_req.cycle,
        status: ResponseStatus::Ok,
        report: "y".repeat(AUDIT_REPORT_TEXT_MIN_CHARS),
        ..StuckMathAuditResponse::default()
    };
    let after_audit = apply_event(
        dispatched.state,
        ProtocolEvent::WrapperResponse {
            response: WrapperResponse::StuckMathAudit(audit_response),
        },
    )
    .expect("the auditor response should write a plan and return to Reviewer");
    assert_eq!(after_audit.state.stage, Stage::Reviewer);
    assert!(
        after_audit.state.audit_plan.is_some(),
        "the on-demand auditor runs the ordinary structural-blocker role and writes a plan"
    );

    // Step 3: an immediate re-request (same cycle) is blocked by the cooldown.
    let mut rerequest_state = after_audit.state;
    let rerequest = audit_request_continue(&rerequest_state, "again, immediately");
    assert!(
        !rerequest_state.review_response_legal(&rerequest),
        "an immediate on-demand re-request must be blocked by the dispatch cooldown"
    );
    let reasons = rerequest_state.review_response_rejection_reasons(&rerequest);
    assert!(
        reasons.iter().any(|r| r.contains("not admissible")),
        "the cooldown rejection must be named; got {reasons:?}"
    );
    // Belt-and-braces: the carrier is not set by a rejected response.
    rerequest_state.pending_audit_request = None;
}
