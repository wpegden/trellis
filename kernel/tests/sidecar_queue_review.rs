//! Sidecar queue redesign — reviewer wire + legality matrix + apply
//! (plan §6.1, amendments A7/A9).
//!
//! Drives the kernel's public `apply_event` API plus the full reviewer
//! wire path (`validate_trellis_reviewer_result_data` →
//! `RawReviewPayload` → `normalize_review_response`) so an allowlist
//! strip of the new fields fails loudly here (risk 1). Every reject row
//! asserts BOTH the boolean gate and the NAMED reason (the boolean and
//! reason paths share one helper kernel-side; these rows pin the shared
//! text).

use std::collections::BTreeSet;

use trellis_kernel::engine::{apply_event, ProtocolCommand, ProtocolEvent};
use trellis_kernel::review_normalization::{
    normalize_review_response, RawReviewPayload, ReviewNormalizationInput,
};
use trellis_kernel::{
    validate_trellis_reviewer_result_data, AssessmentOrigin, CorrStatus, NodeId, NodeKind, Phase,
    ProtocolState, RequestKind, ResponseStatus, ReviewDecisionKind, ReviewResponse,
    SoundAssessment, SoundAssessmentStatus, SoundFingerprintParts, Stage, TargetId, TaskMode,
    WorkingSnapshot,
    WrapperRequest, WrapperResponse,
};

fn nid(s: &str) -> NodeId {
    NodeId::from(s)
}

fn add_eligible_node(state: &mut ProtocolState, name: &str) {
    let n = nid(name);
    state.live.present_nodes.insert(n.clone());
    state.live.open_nodes.insert(n.clone());
    state.node_kinds.insert(n.clone(), NodeKind::Proof);
    state.proof_nodes.insert(n.clone());
    state.corr_status.insert(n.clone(), CorrStatus::Pass);
    state
        .live
        .corr_current_fingerprints
        .insert(n.clone(), format!("c-{name}"));
    state
        .corr_approved_fingerprints
        .insert(n.clone(), format!("c-{name}"));
    state
        .substantiveness_status
        .insert(n.clone(), CorrStatus::Pass);
    state
        .live
        .substantiveness_current_fingerprints
        .insert(n.clone(), format!("s-{name}"));
    state
        .substantiveness_approved_fingerprints
        .insert(n.clone(), format!("s-{name}"));
    state.deps.insert(n.clone(), BTreeSet::new());
    // Sound VerifierPass so the synthetic state carries NO blockers —
    // keeps the Review non-friction (no paper-grounding requirement),
    // which is orthogonal to the queue clauses under test. (Since the
    // 2026-07-23 owner decision the sound lane never gates sidecar
    // eligibility, so Pass-everywhere loses no coverage here.)
    state.sound_assessments.insert(
        n.clone(),
        SoundAssessment {
            status: SoundAssessmentStatus::VerifierPass,
            origin: AssessmentOrigin::VerifierPanel,
            fingerprints: SoundFingerprintParts::default(),
            lane_votes: std::collections::BTreeMap::new(),
            reviewer_action_id: None,
        },
    );
}

/// A ProofFormalization Reviewer-stage state with three open,
/// sidecar-eligible proof nodes (Rung / Lift / Act), no coarse-DAG
/// constraints, no audit latch.
fn proof_review_state(cycle: u32) -> ProtocolState {
    let mut state = ProtocolState {
        phase: Phase::ProofFormalization,
        stage: Stage::Reviewer,
        cycle,
        active_node: None,
        live: WorkingSnapshot::default(),
        ..ProtocolState::default()
    };
    for name in ["Rung", "Lift", "Act"] {
        add_eligible_node(&mut state, name);
    }
    state.committed = state.live.clone();
    state.committed_proof_nodes = state.proof_nodes.clone();
    state.committed_deps = state.deps.clone();
    state
}

/// A plain legal PF Continue routing the worker at `next_active`
/// (Local mode, empty scope).
fn continue_response(state: &ProtocolState, next_active: Option<&str>) -> ReviewResponse {
    ReviewResponse {
        request_id: 1,
        cycle: state.cycle,
        status: ResponseStatus::Ok,
        decision: ReviewDecisionKind::Continue,
        next_active: next_active.map(nid),
        next_mode: TaskMode::Local,
        allow_new_obligations: true,
        must_close_active: false,
        ..ReviewResponse::default()
    }
}

fn assert_setup_routes(state: &ProtocolState, node: &str) {
    let request = state.expected_request(1, RequestKind::Review);
    assert!(
        request.kernel_hinted_next_active_nodes.contains(&nid(node)),
        "test setup: {node} must be a hinted next_active candidate; hinted = {:?}",
        request.kernel_hinted_next_active_nodes
    );
    let response = continue_response(state, Some(node));
    assert!(
        state.review_response_legal(&response),
        "test setup: a plain Continue at {node} must be legal before queue fields enter; reasons = {:?}",
        state.review_response_rejection_reasons(&response)
    );
}

fn assert_reject_with_reason(state: &ProtocolState, response: &ReviewResponse, needle: &str) {
    assert!(
        !state.review_response_legal(response),
        "response must be illegal (expected reason containing {needle:?})"
    );
    let reasons = state.review_response_rejection_reasons(response);
    assert!(
        reasons.iter().any(|reason| reason.contains(needle)),
        "rejection reasons must name the queue clause {needle:?}; got {reasons:?}"
    );
}

fn apply_review(state: ProtocolState, response: ReviewResponse) -> trellis_kernel::TransitionOutcome {
    let mut state = state;
    let request = state.issue_request(RequestKind::Review);
    let mut response = response;
    response.request_id = request.id;
    apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: WrapperResponse::Review(response),
        },
    )
    .expect("legal review response must apply")
}

// ── Legality matrix: adds ───────────────────────────────────────────────

#[test]
fn add_eligible_nodes_is_legal_and_appends_in_list_order() {
    let state = proof_review_state(9);
    assert_setup_routes(&state, "Act");
    let mut response = continue_response(&state, Some("Act"));
    response.sidecar_queue_add = vec![nid("Rung"), nid("Lift")];
    assert!(
        state.review_response_legal(&response),
        "adding two eligible nodes must be legal; reasons = {:?}",
        state.review_response_rejection_reasons(&response)
    );
    let outcome = apply_review(state, response);
    let queue = &outcome.state.sidecar_queue;
    assert_eq!(
        queue.iter().map(|e| e.node.as_str()).collect::<Vec<_>>(),
        vec!["Rung", "Lift"],
        "order = submission order (Q3)"
    );
    assert_eq!(queue[0].entry_seq, 1);
    assert_eq!(queue[1].entry_seq, 2);
    assert!(queue.iter().all(|e| e.queued_at_cycle == 9));
    assert_eq!(outcome.state.sidecar_queue_seq, 2);
}

#[test]
fn add_ineligible_node_rejects_per_clause() {
    // Closed node.
    let mut state = proof_review_state(9);
    state.live.open_nodes.remove(&nid("Rung"));
    state.committed.open_nodes.remove(&nid("Rung"));
    let mut response = continue_response(&state, Some("Act"));
    response.sidecar_queue_add = vec![nid("Rung")];
    assert_reject_with_reason(&state, &response, "Rung is not sidecar-eligible");

    // SKETCH placeholder.
    let mut state = proof_review_state(9);
    state.live.sketch_proof_nodes.insert(nid("Rung"));
    let mut response = continue_response(&state, Some("Act"));
    response.sidecar_queue_add = vec![nid("Rung")];
    assert_reject_with_reason(&state, &response, "Rung is not sidecar-eligible");

    // Statement-lane drift (corr current fingerprint moved off approved).
    let mut state = proof_review_state(9);
    state
        .live
        .corr_current_fingerprints
        .insert(nid("Rung"), "drifted".to_string());
    let mut response = continue_response(&state, Some("Act"));
    response.sidecar_queue_add = vec![nid("Rung")];
    assert_reject_with_reason(&state, &response, "Rung is not sidecar-eligible");

    // Non-proof kind.
    let mut state = proof_review_state(9);
    state.node_kinds.insert(nid("Rung"), NodeKind::Definition);
    state.proof_nodes.remove(&nid("Rung"));
    state.committed_proof_nodes.remove(&nid("Rung"));
    let mut response = continue_response(&state, Some("Act"));
    response.sidecar_queue_add = vec![nid("Rung")];
    assert_reject_with_reason(&state, &response, "Rung is not sidecar-eligible");
}

#[test]
fn add_window_shut_rejects_in_uncovered_theorem_stating() {
    // TheoremStating with an uncovered configured target: the orphan
    // window is open ⇒ the sidecar window is shut ⇒ adds ineligible.
    let mut state = proof_review_state(9);
    state.phase = Phase::TheoremStating;
    state.configured_targets.insert(TargetId::from("t"));
    state
        .live
        .paper_current_fingerprints
        .insert(TargetId::from("t"), String::new());
    state
        .committed
        .paper_current_fingerprints
        .insert(TargetId::from("t"), String::new());
    state
        .paper_approved_fingerprints
        .insert(TargetId::from("t"), String::new());
    let mut response = continue_response(&state, None);
    response.next_mode = TaskMode::Global;
    response.sidecar_queue_add = vec![nid("Rung")];
    assert_reject_with_reason(&state, &response, "Rung is not sidecar-eligible");
}

#[test]
fn add_already_queued_or_duplicate_rejects() {
    // Already queued.
    let mut state = proof_review_state(9);
    state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
        node: nid("Rung"),
        entry_seq: 1,
        queued_at_cycle: 5,
    });
    state.sidecar_queue_seq = 1;
    let mut response = continue_response(&state, Some("Act"));
    response.sidecar_queue_add = vec![nid("Rung")];
    assert_reject_with_reason(&state, &response, "Rung is already queued");

    // Duplicate within the list.
    let state = proof_review_state(9);
    let mut response = continue_response(&state, Some("Act"));
    response.sidecar_queue_add = vec![nid("Rung"), nid("Rung")];
    assert_reject_with_reason(&state, &response, "Rung is already queued");
}

// ── Legality matrix: removes ────────────────────────────────────────────

#[test]
fn remove_queued_node_is_legal_and_preserves_survivor_order() {
    let mut state = proof_review_state(9);
    for (i, name) in ["Rung", "Lift", "Act"].iter().enumerate() {
        state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
            node: nid(name),
            entry_seq: (i + 1) as u64,
            queued_at_cycle: 5,
        });
    }
    state.sidecar_queue_seq = 3;
    // next_active must avoid the queued nodes (Q7) — remove Lift, route
    // nothing new: PF Continue requires next_active, so route at Act
    // AND remove it too.
    let mut response = continue_response(&state, Some("Act"));
    response.sidecar_queue_remove = vec![nid("Lift"), nid("Act")];
    assert!(
        state.review_response_legal(&response),
        "removing queued nodes must be legal; reasons = {:?}",
        state.review_response_rejection_reasons(&response)
    );
    let outcome = apply_review(state, response);
    assert_eq!(
        outcome
            .state
            .sidecar_queue
            .iter()
            .map(|e| e.node.as_str())
            .collect::<Vec<_>>(),
        vec!["Rung"],
        "survivor order preserved"
    );
    assert_eq!(
        outcome.state.sidecar_queue_seq, 3,
        "removals never touch the generation counter"
    );
}

#[test]
fn need_input_with_queue_fields_rejects_and_continue_still_applies() {
    // Delta audit F3: the queue apply is decision-independent, so
    // without a legality gate a NeedInput carrying queue fields would
    // mutate the queue on a stop-and-escalate decision. Both polarities:
    // NeedInput + queue fields rejects with the named reason; the same
    // fields on a Continue still apply.
    let mut state = proof_review_state(9);
    state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
        node: nid("Lift"),
        entry_seq: 1,
        queued_at_cycle: 5,
    });
    state.sidecar_queue_seq = 1;
    let need_input = |add: Vec<NodeId>, remove: Vec<NodeId>| ReviewResponse {
        request_id: 1,
        cycle: state.cycle,
        status: ResponseStatus::Ok,
        decision: ReviewDecisionKind::NeedInput,
        next_active: None,
        next_mode: state.current_mode(),
        sidecar_queue_add: add,
        sidecar_queue_remove: remove,
        ..ReviewResponse::default()
    };
    // Control: a queue-free NeedInput is legal in this state.
    let control = need_input(vec![], vec![]);
    assert!(
        state.review_response_legal(&control),
        "test setup: a queue-free NeedInput must be legal; reasons = {:?}",
        state.review_response_rejection_reasons(&control)
    );
    let needle = "NeedInput must leave sidecar_queue_add and sidecar_queue_remove empty";
    assert_reject_with_reason(&state, &need_input(vec![nid("Rung")], vec![]), needle);
    assert_reject_with_reason(&state, &need_input(vec![], vec![nid("Lift")]), needle);
    // Same queue edits on a Continue stay legal and apply.
    let mut response = continue_response(&state, Some("Act"));
    response.sidecar_queue_add = vec![nid("Rung")];
    response.sidecar_queue_remove = vec![nid("Lift")];
    assert!(
        state.review_response_legal(&response),
        "the same queue edits on Continue must stay legal; reasons = {:?}",
        state.review_response_rejection_reasons(&response)
    );
    let outcome = apply_review(state, response);
    assert_eq!(
        outcome
            .state
            .sidecar_queue
            .iter()
            .map(|e| e.node.as_str())
            .collect::<Vec<_>>(),
        vec!["Rung"],
        "Continue's add applied and its remove dequeued Lift"
    );
}

#[test]
fn remove_unqueued_node_rejects() {
    let state = proof_review_state(9);
    let mut response = continue_response(&state, Some("Act"));
    response.sidecar_queue_remove = vec![nid("Rung")];
    assert_reject_with_reason(&state, &response, "Rung is not in the sidecar queue");
}

#[test]
fn same_node_in_add_and_remove_rejects() {
    let mut state = proof_review_state(9);
    state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
        node: nid("Rung"),
        entry_seq: 1,
        queued_at_cycle: 5,
    });
    state.sidecar_queue_seq = 1;
    let mut response = continue_response(&state, Some("Act"));
    response.sidecar_queue_add = vec![nid("Rung")];
    response.sidecar_queue_remove = vec![nid("Rung")];
    assert_reject_with_reason(
        &state,
        &response,
        "appears in sidecar_queue_remove in the same response",
    );
}

// ── Cancel-before-worker (owner rule, Q7) ───────────────────────────────

#[test]
fn next_active_on_queued_node_without_remove_rejects() {
    let mut state = proof_review_state(9);
    assert_setup_routes(&state, "Rung");
    state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
        node: nid("Rung"),
        entry_seq: 1,
        queued_at_cycle: 5,
    });
    state.sidecar_queue_seq = 1;
    let response = continue_response(&state, Some("Rung"));
    assert_reject_with_reason(
        &state,
        &response,
        "next_active Rung is in the sidecar grunt queue",
    );
}

#[test]
fn next_active_on_queued_node_with_remove_is_legal_routes_and_dequeues() {
    let mut state = proof_review_state(9);
    assert_setup_routes(&state, "Rung");
    state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
        node: nid("Rung"),
        entry_seq: 1,
        queued_at_cycle: 5,
    });
    state.sidecar_queue_seq = 1;
    let mut response = continue_response(&state, Some("Rung"));
    response.sidecar_queue_remove = vec![nid("Rung")];
    assert!(
        state.review_response_legal(&response),
        "remove-and-route in one response must be legal; reasons = {:?}",
        state.review_response_rejection_reasons(&response)
    );
    let outcome = apply_review(state, response);
    assert_eq!(outcome.state.active_node, Some(nid("Rung")), "routed");
    assert!(outcome.state.sidecar_queue.is_empty(), "dequeued");
}

#[test]
fn next_active_on_added_node_rejects_via_q_prime() {
    let state = proof_review_state(9);
    assert_setup_routes(&state, "Rung");
    let mut response = continue_response(&state, Some("Rung"));
    response.sidecar_queue_add = vec![nid("Rung")];
    assert_reject_with_reason(
        &state,
        &response,
        "next_active Rung is in the sidecar grunt queue",
    );
}

// ── Phase-independence + config-off inertness ───────────────────────────

#[test]
fn queue_fields_apply_on_covered_theorem_stating_review() {
    // TheoremStating with every configured target covered: the sidecar
    // window is open, so adds are legal — the same matrix,
    // phase-independent. This state carries NO sidecar config anywhere
    // (the engine never reads config): the accepted response fills the
    // queue and nothing else happens — the legal-but-inert config-off
    // pin (Q6; a run without a daemon just accumulates queue state).
    let mut state = proof_review_state(9);
    state.phase = Phase::TheoremStating;
    let t = TargetId::from("t");
    state.configured_targets.insert(t.clone());
    state.target_claims.insert(nid("Act"), [t.clone()].into_iter().collect());
    state.live.coverage.insert(t.clone(), [nid("Act")].into_iter().collect());
    state
        .live
        .paper_current_fingerprints
        .insert(t.clone(), "fp".to_string());
    state.committed_target_claims = state.target_claims.clone();
    state.committed.coverage = state.live.coverage.clone();
    state
        .committed
        .paper_current_fingerprints
        .insert(t.clone(), "fp".to_string());
    state
        .paper_approved_fingerprints
        .insert(t.clone(), "fp".to_string());
    // Paper-faithfulness Pass on the covered target so the state stays
    // blocker-free (non-friction Review), as in the PF harness.
    state.paper_status.insert(t.clone(), CorrStatus::Pass);
    assert!(state.sidecar_window_open(&state.live));

    let mut response = continue_response(&state, None);
    response.next_mode = TaskMode::Global;
    response.sidecar_queue_add = vec![nid("Rung")];
    assert!(
        state.review_response_legal(&response),
        "TheoremStating (covered) queue add must be legal; reasons = {:?}",
        state.review_response_rejection_reasons(&response)
    );
    let outcome = apply_review(state, response);
    assert_eq!(outcome.state.sidecar_queue.len(), 1);
    assert_eq!(outcome.state.sidecar_queue[0].node, nid("Rung"));
}

// ── Rejection re-issue + tail prune under apply_event ───────────────────

#[test]
fn rejected_response_reissues_review_carrying_queue_reason() {
    let mut state = proof_review_state(9);
    state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
        node: nid("Rung"),
        entry_seq: 1,
        queued_at_cycle: 5,
    });
    state.sidecar_queue_seq = 1;
    let request = state.issue_request(RequestKind::Review);
    let mut response = continue_response(&state, Some("Rung"));
    response.request_id = request.id;
    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: WrapperResponse::Review(response),
        },
    )
    .expect("illegal review response re-issues, never errors");
    assert!(
        outcome
            .state
            .latest_review_rejection_reasons
            .iter()
            .any(|reason| reason.contains("include it in sidecar_queue_remove")),
        "reissued Review must carry the named queue reason; got {:?}",
        outcome.state.latest_review_rejection_reasons
    );
    let reissued: Vec<&WrapperRequest> = outcome
        .commands
        .iter()
        .filter_map(|c| match c {
            ProtocolCommand::IssueRequest { request } => Some(request),
            _ => None,
        })
        .collect();
    assert!(
        reissued.iter().any(|r| r.kind == RequestKind::Review),
        "a Review request must be re-issued"
    );
    assert_eq!(
        outcome.state.sidecar_queue.len(),
        1,
        "a rejected response mutates nothing"
    );
}

#[test]
fn tail_prune_fires_inside_apply_event() {
    // Queue Rung, then hand-close it (as the primary path would), then
    // apply any event: the deterministic tail prune drops the entry and
    // logs `closed`.
    let mut state = proof_review_state(9);
    state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
        node: nid("Rung"),
        entry_seq: 4,
        queued_at_cycle: 5,
    });
    state.sidecar_queue_seq = 4;
    state.live.open_nodes.remove(&nid("Rung"));
    state.committed.open_nodes.remove(&nid("Rung"));
    let response = continue_response(&state, Some("Act"));
    let outcome = apply_review(state, response);
    assert!(
        outcome.state.sidecar_queue.is_empty(),
        "closed node must be pruned by the apply_event tail"
    );
    let last = outcome
        .state
        .sidecar_queue_prune_log
        .last()
        .expect("prune logged");
    assert_eq!(last.node, nid("Rung"));
    assert_eq!(last.entry_seq, 4);
    assert_eq!(last.reason, "closed");
}

// ── Wire path (allowlist strip protection) ──────────────────────────────

#[test]
fn queue_fields_survive_validator_and_normalization() {
    let state = proof_review_state(9);
    let request = state.expected_request(1, RequestKind::Review);
    let raw = serde_json::json!({
        "decision": "continue",
        "reason": "route",
        "comments": "",
        "next_active": "Act",
        "next_mode": "local",
        "reset": "none",
        "allow_new_obligations": true,
        "must_close_active": false,
        "sidecar_queue_add": ["Rung", "Lift"],
        "sidecar_queue_remove": [],
    });
    let validated = validate_trellis_reviewer_result_data(&raw);
    assert!(validated.ok, "validator errors: {:?}", validated.errors);
    let data = validated.data.expect("validated data");
    assert_eq!(
        data["sidecar_queue_add"],
        serde_json::json!(["Rung", "Lift"]),
        "the allowlist re-emit must preserve sidecar_queue_add"
    );
    assert!(
        data.get("sidecar_queue_remove").is_none(),
        "empty sidecar_queue_remove stays OFF the validated payload (byte-identity)"
    );
    let raw_payload: RawReviewPayload =
        serde_json::from_value(data).expect("validated payload deserializes");
    let normalized = normalize_review_response(&ReviewNormalizationInput {
        request,
        raw_payload,
    })
    .expect("normalization succeeds");
    assert_eq!(
        normalized.response.sidecar_queue_add,
        vec![nid("Rung"), nid("Lift")]
    );
    assert!(normalized.response.sidecar_queue_remove.is_empty());
}

#[test]
fn duplicated_add_survives_wire_and_legality_rejects_with_named_reason() {
    // Delta audit F2 (deviation 1): the validator must NOT dedupe the
    // queue lists — a duplicated add flows through the allowlist re-emit
    // and normalization intact so `sidecar_queue_response_violations`
    // rejects it with the NAMED duplicate reason, instead of the wire
    // silently repairing the payload.
    let state = proof_review_state(9);
    let request = state.expected_request(1, RequestKind::Review);
    let raw = serde_json::json!({
        "decision": "continue",
        "reason": "route",
        "comments": "",
        "next_active": "Act",
        "next_mode": "local",
        "reset": "none",
        "allow_new_obligations": true,
        "must_close_active": false,
        "sidecar_queue_add": ["Rung", "Rung"],
        "sidecar_queue_remove": [],
    });
    let validated = validate_trellis_reviewer_result_data(&raw);
    assert!(validated.ok, "validator errors: {:?}", validated.errors);
    let data = validated.data.expect("validated data");
    assert_eq!(
        data["sidecar_queue_add"],
        serde_json::json!(["Rung", "Rung"]),
        "the validator must preserve the duplicate, not silently dedupe"
    );
    let raw_payload: RawReviewPayload =
        serde_json::from_value(data).expect("validated payload deserializes");
    let normalized = normalize_review_response(&ReviewNormalizationInput {
        request,
        raw_payload,
    })
    .expect("normalization succeeds");
    assert_eq!(
        normalized.response.sidecar_queue_add,
        vec![nid("Rung"), nid("Rung")],
        "normalization preserves the duplicate too"
    );
    assert_reject_with_reason(&state, &normalized.response, "Rung is already queued");
}

// ── Prompt-surface gating (Q6/A4) + Malformed-reissue byte-identity ─────

#[test]
fn review_payload_and_fragments_gate_on_advertise_flag() {
    let mut state = proof_review_state(9);
    state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
        node: nid("Rung"),
        entry_seq: 4,
        queued_at_cycle: 7,
    });
    state.sidecar_queue_seq = 4;

    // Engine-derived request: advertise flag FALSE (config is a runtime
    // concern) — queue projection present on the request, but NO prompt
    // surface: payload keys, optional_fields, and the fragment all
    // absent, so sidecar-less runs render byte-identical prompts.
    let request = state.expected_request(1, RequestKind::Review);
    assert_eq!(request.sidecar_queue.len(), 1);
    assert!(request.sidecar_window_open);
    assert!(!request.sidecar_advertise_queue_fields);
    let payload = trellis_kernel::request_contracts::review_contract_payload(&request);
    assert!(payload["request_summary"].get("sidecar_queue").is_none());
    assert!(payload["request_summary"]
        .get("sidecar_window_open")
        .is_none());
    let optional = payload["artifact_contract"]["optional_fields"].as_array().unwrap();
    assert!(!optional.iter().any(|v| v == "sidecar_queue_add"));
    let fragments = payload["prompt_fragments"].as_array().unwrap();
    assert!(!fragments
        .iter()
        .any(|v| v == "review/common/30g_sidecar_grunts.md"));

    // Advertised (the runtime resolved an enabled `sidecar` block):
    // payload section + optional fields + schema example + fragment.
    let mut advertised = request.clone();
    advertised.sidecar_advertise_queue_fields = true;
    let payload = trellis_kernel::request_contracts::review_contract_payload(&advertised);
    let queue = payload["request_summary"]["sidecar_queue"]
        .as_array()
        .expect("advertised payload carries the queue section");
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0]["node"], serde_json::json!("Rung"));
    assert_eq!(queue[0]["entry_seq"], serde_json::json!(4));
    assert_eq!(queue[0]["queued_at_cycle"], serde_json::json!(7));
    assert_eq!(
        payload["request_summary"]["sidecar_window_open"],
        serde_json::json!(true)
    );
    let optional = payload["artifact_contract"]["optional_fields"].as_array().unwrap();
    assert!(optional.iter().any(|v| v == "sidecar_queue_add"));
    assert!(optional.iter().any(|v| v == "sidecar_queue_remove"));
    let fragments = payload["prompt_fragments"].as_array().unwrap();
    let pos_30g = fragments
        .iter()
        .position(|v| v == "review/common/30g_sidecar_grunts.md")
        .expect("advertised request lists the grunt fragment");
    let pos_30a = fragments
        .iter()
        .position(|v| v == "review/common/30a_blocker_actions.md")
        .expect("blocker-actions fragment present");
    assert!(pos_30g > pos_30a, "30g renders inside the 30x block");
    let example = &payload["artifact_contract"]["prompt_schema_example"];
    assert!(example.get("sidecar_queue_add").is_some());
    assert!(example.get("sidecar_queue_remove").is_some());
}

/// A4/A9: with a NON-EMPTY queue in state, a Malformed reviewer
/// response re-issues a Review whose derivation is byte-identical to
/// the original modulo the request id (every field, the queue
/// projection and window flag included, re-derives from state).
#[test]
fn malformed_reissue_is_byte_identical_with_nonempty_queue() {
    let mut state = proof_review_state(9);
    state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
        node: nid("Rung"),
        entry_seq: 4,
        queued_at_cycle: 7,
    });
    state.sidecar_queue_seq = 4;
    let original = state.issue_request(RequestKind::Review);
    let malformed = ReviewResponse {
        request_id: original.id,
        cycle: state.cycle,
        status: ResponseStatus::Malformed,
        ..ReviewResponse::default()
    };
    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: WrapperResponse::Review(malformed),
        },
    )
    .expect("malformed response re-issues");
    let reissued = outcome
        .state
        .in_flight_request
        .as_ref()
        .expect("reissued request in flight");
    assert_eq!(reissued.sidecar_queue, original.sidecar_queue);
    assert_eq!(reissued.sidecar_window_open, original.sidecar_window_open);
    // The queue section of the VALIDATED payload re-derives to
    // identical bytes (the reissued request legitimately differs
    // elsewhere: it carries the Malformed rejection reasons). Compare
    // under advertisement so the section actually renders.
    let queue_section = |request: &WrapperRequest| -> String {
        let mut advertised = request.clone();
        advertised.sidecar_advertise_queue_fields = true;
        let payload =
            trellis_kernel::request_contracts::review_contract_payload(&advertised);
        serde_json::to_string(&serde_json::json!({
            "sidecar_queue": payload["request_summary"]["sidecar_queue"],
            "sidecar_window_open": payload["request_summary"]["sidecar_window_open"],
        }))
        .unwrap()
    };
    assert_eq!(
        queue_section(reissued),
        queue_section(&original),
        "queue payload section must re-derive byte-identically across the reissue"
    );
}

/// The three new WrapperRequest fields stay off the wire at their
/// defaults (in_flight_request is serialized state; pre-feature state
/// files round-trip byte-identically).
#[test]
fn wrapper_request_queue_fields_skip_when_empty_and_roundtrip() {
    let value = serde_json::to_value(WrapperRequest::default()).unwrap();
    let obj = value.as_object().unwrap();
    for key in [
        "sidecar_queue",
        "sidecar_window_open",
        "sidecar_advertise_queue_fields",
    ] {
        assert!(!obj.contains_key(key), "{key} must skip at default");
    }
    let mut populated = WrapperRequest::default();
    populated.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
        node: nid("Rung"),
        entry_seq: 2,
        queued_at_cycle: 5,
    });
    populated.sidecar_window_open = true;
    populated.sidecar_advertise_queue_fields = true;
    let round: WrapperRequest =
        serde_json::from_value(serde_json::to_value(&populated).unwrap()).unwrap();
    assert_eq!(round.sidecar_queue, populated.sidecar_queue);
    assert!(round.sidecar_window_open);
    assert!(round.sidecar_advertise_queue_fields);
}

#[test]
fn queue_free_validated_payload_is_byte_identical() {
    // A reviewer response that never touches the queue fields must
    // produce a validated payload WITHOUT the new keys (fresh-log /
    // contract-baseline byte-identity, amendment A7).
    let raw = serde_json::json!({
        "decision": "continue",
        "reason": "route",
        "comments": "",
        "next_active": "Act",
        "next_mode": "local",
        "reset": "none",
        "allow_new_obligations": true,
        "must_close_active": false,
    });
    let validated = validate_trellis_reviewer_result_data(&raw);
    assert!(validated.ok);
    let data = validated.data.expect("validated data");
    assert!(data.get("sidecar_queue_add").is_none());
    assert!(data.get("sidecar_queue_remove").is_none());
}
