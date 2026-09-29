//! Process memory (PROCESS_MEMORY_SPEC.md) — feature-level regression
//! tests:
//!
//! 1. serde default-compat: persisted state / response JSON written
//!    BEFORE this feature (no `process_memory_seq`,
//!    `pending_memory_challenges`, `memory_challenges`,
//!    `memory_operations`, `preserve_process_memory` keys) must load
//!    unchanged, and the new fields must stay off the wire at their
//!    defaults.
//! 2. Allowlist regression (feedback_allowlist_validator): the
//!    `validate_trellis_*_result_data` validators strip unknown fields —
//!    these tests prove the new payload fields survive the round-trip,
//!    the exact bug class that bit `challenge_claim_updates` /
//!    `request_sound_verifier_node_ids` before.

use serde_json::json;
use trellis_kernel::{
    validate_trellis_reviewer_result_data, validate_trellis_stuck_math_audit_result_data,
    validate_trellis_worker_result_data, ProtocolState, ReviewResponse, StuckMathAuditResponse,
    WorkerResponse, WrapperRequest,
};

// ---- serde default-compat ---------------------------------------------

#[test]
fn protocol_state_without_process_memory_fields_loads_with_defaults() {
    let mut as_json = serde_json::to_value(ProtocolState::default()).expect("serialize state");
    let obj = as_json.as_object_mut().expect("state is an object");
    // Simulate a pre-feature persisted state file: the new keys are absent.
    obj.remove("process_memory_seq");
    assert!(
        obj.remove("pending_memory_challenges").is_none(),
        "empty pending_memory_challenges must be skipped on the wire"
    );
    let loaded: ProtocolState = serde_json::from_value(as_json).expect("pre-feature state loads");
    assert_eq!(loaded.process_memory_seq, 0);
    assert!(loaded.pending_memory_challenges.is_empty());
    assert_eq!(loaded, ProtocolState::default());
}

#[test]
fn review_response_without_new_fields_defaults_to_preserve() {
    let mut as_json = serde_json::to_value(ReviewResponse::default()).expect("serialize review");
    let obj = as_json.as_object_mut().expect("review is an object");
    assert!(
        obj.remove("memory_challenges").is_none() && obj.remove("preserve_process_memory").is_none(),
        "defaults must be skipped on the wire (write compatibility)"
    );
    let loaded: ReviewResponse = serde_json::from_value(as_json).expect("pre-feature review loads");
    assert!(loaded.preserve_process_memory, "absent flag must default to preserve");
    assert!(loaded.memory_challenges.is_empty());
    // Explicit false round-trips.
    let mut explicit = ReviewResponse::default();
    explicit.preserve_process_memory = false;
    let as_json = serde_json::to_value(&explicit).expect("serialize");
    assert_eq!(as_json["preserve_process_memory"], json!(false));
    let back: ReviewResponse = serde_json::from_value(as_json).expect("round-trip");
    assert!(!back.preserve_process_memory);
}

#[test]
fn worker_and_audit_responses_without_new_fields_load_with_defaults() {
    let mut worker = serde_json::to_value(WorkerResponse::default()).expect("serialize worker");
    assert!(worker.as_object_mut().unwrap().remove("memory_challenges").is_none());
    let loaded: WorkerResponse = serde_json::from_value(worker).expect("pre-feature worker loads");
    assert!(loaded.memory_challenges.is_empty());

    let mut audit =
        serde_json::to_value(StuckMathAuditResponse::default()).expect("serialize audit");
    assert!(audit.as_object_mut().unwrap().remove("memory_operations").is_none());
    let loaded: StuckMathAuditResponse =
        serde_json::from_value(audit).expect("pre-feature audit loads");
    assert!(loaded.memory_operations.is_empty());
}

/// The `memory_challenges` ADVERTISEMENT flag stays off the wire at its
/// default, so a pre-feature persisted `in_flight_request` round-trips
/// byte-identically (the `sidecar_advertise_queue_fields` precedent).
#[test]
fn wrapper_request_process_memory_flag_skips_when_default_and_roundtrips() {
    let value = serde_json::to_value(WrapperRequest::default()).expect("serialize request");
    assert!(
        !value
            .as_object()
            .expect("request object")
            .contains_key("process_memory_active"),
        "process_memory_active must skip at default"
    );
    let mut populated = WrapperRequest::default();
    populated.process_memory_active = true;
    let round: WrapperRequest =
        serde_json::from_value(serde_json::to_value(&populated).expect("serialize"))
            .expect("deserialize");
    assert!(round.process_memory_active);
}

// ---- allowlist regression (feedback_allowlist_validator) ---------------

fn minimal_worker_payload() -> serde_json::Value {
    json!({
        "outcome": "valid",
        "summary": "s",
        "comments": "",
        "semantic_dep_updates": {},
        "target_claim_updates": {},
        "difficulty_updates": {},
        "deleted_nodes": [],
        "needs_restructure_suggested_nodes": []
    })
}

#[test]
fn worker_memory_challenges_survive_allowlist_validation() {
    let mut payload = minimal_worker_payload();
    payload["memory_challenges"] = json!([
        {"entry_id": "pm-0003-x", "reason": "compiled probe contradicts the bound"}
    ]);
    let result = validate_trellis_worker_result_data(&payload);
    assert!(result.ok, "errors: {:?}", result.errors);
    let data = result.data.expect("success data");
    assert_eq!(
        data["memory_challenges"],
        json!([{"entry_id": "pm-0003-x", "reason": "compiled probe contradicts the bound"}]),
        "memory_challenges must survive the allowlist re-emit"
    );
    // Baseline stability: no key when the worker did not challenge.
    let clean = validate_trellis_worker_result_data(&minimal_worker_payload());
    assert!(clean.ok);
    assert!(clean.data.expect("data").get("memory_challenges").is_none());
}

#[test]
fn worker_memory_challenges_shape_is_validated() {
    let mut payload = minimal_worker_payload();
    payload["memory_challenges"] = json!([{"entry_id": "", "reason": ""}]);
    let result = validate_trellis_worker_result_data(&payload);
    assert!(!result.ok);
    assert!(result
        .errors
        .iter()
        .any(|e| e.contains("memory_challenges[0]")));
}

fn minimal_reviewer_payload() -> serde_json::Value {
    json!({
        "decision": "continue",
        "reason": "r",
        "comments": "",
        "next_active": "",
        "next_mode": "targeted",
        "reset": "none",
        "task_blocker_ids": [],
        "reset_blocker_ids": [],
        "difficulty_updates": {},
        "allow_new_obligations": true,
        "must_close_active": false,
        "clear_human_input": false
    })
}

#[test]
fn reviewer_memory_fields_survive_allowlist_validation() {
    let mut payload = minimal_reviewer_payload();
    payload["memory_challenges"] = json!([
        {"entry_id": "pm-0002-y", "reason": "paper display (3.4) contradicts the entry"}
    ]);
    payload["preserve_process_memory"] = json!(false);
    let result = validate_trellis_reviewer_result_data(&payload);
    assert!(result.ok, "errors: {:?}", result.errors);
    let data = result.data.expect("success data");
    assert_eq!(
        data["memory_challenges"][0]["entry_id"],
        json!("pm-0002-y"),
        "memory_challenges must survive the allowlist re-emit"
    );
    assert_eq!(data["preserve_process_memory"], json!(false));
    // Baseline stability: neither key when the reviewer did not use them.
    let clean = validate_trellis_reviewer_result_data(&minimal_reviewer_payload());
    assert!(clean.ok, "errors: {:?}", clean.errors);
    let data = clean.data.expect("data");
    assert!(data.get("memory_challenges").is_none());
    assert!(data.get("preserve_process_memory").is_none());
}

fn minimal_audit_payload() -> serde_json::Value {
    json!({
        "report": format!("## Claim being audited\n{}", "x".repeat(400)),
        "tasks": [],
        "probe_paths": []
    })
}

#[test]
fn audit_memory_operations_survive_allowlist_validation() {
    let mut payload = minimal_audit_payload();
    payload["memory_operations"] = json!([
        {"op": "add", "type": "refuted-route", "coarse_node": "global",
         "title": "Route X refuted", "body": "Counterexample n=3 inline."},
        {"op": "retire", "entry_id": "pm-0001-old", "reason": "mis-scaled"}
    ]);
    let result = validate_trellis_stuck_math_audit_result_data(&payload);
    assert!(result.ok, "errors: {:?}", result.errors);
    let data = result.data.expect("success data");
    let ops = data["memory_operations"].as_array().expect("ops array");
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0]["op"], json!("add"));
    assert_eq!(ops[0]["type"], json!("refuted-route"));
    assert_eq!(ops[1]["entry_id"], json!("pm-0001-old"));
    // Baseline stability: no key when the audit carries no operations.
    let clean = validate_trellis_stuck_math_audit_result_data(&minimal_audit_payload());
    assert!(clean.ok, "errors: {:?}", clean.errors);
    assert!(clean.data.expect("data").get("memory_operations").is_none());
}

#[test]
fn audit_memory_operations_shape_rejections() {
    for (op, expected) in [
        (json!({"op": "delete", "entry_id": "pm-1"}), "op must be one of"),
        (
            json!({"op": "add", "type": "hunch", "coarse_node": "global", "title": "t", "body": "b"}),
            "type must be one of",
        ),
        (
            json!({"op": "add", "type": "constraint", "coarse_node": "global", "title": "", "body": "b"}),
            "title must be non-empty",
        ),
        (
            json!({"op": "add", "type": "constraint", "coarse_node": "global", "title": "t", "body": ""}),
            "body must be non-empty",
        ),
        (
            json!({"op": "add", "type": "constraint", "coarse_node": "", "title": "t", "body": "b"}),
            "coarse_node must be",
        ),
        (
            json!({"op": "retire", "entry_id": "pm-1", "reason": ""}),
            "reason must be non-empty",
        ),
        (
            json!({"op": "supersede", "entry_id": "", "type": "constraint", "title": "t", "body": "b"}),
            "entry_id must be non-empty",
        ),
        (
            json!({"op": "add", "type": "constraint", "coarse_node": "global", "title": "t",
                   "body": "x".repeat(8001)}),
            "at most 8000 characters",
        ),
    ] {
        let mut payload = minimal_audit_payload();
        payload["memory_operations"] = json!([op]);
        let result = validate_trellis_stuck_math_audit_result_data(&payload);
        assert!(!result.ok, "op {op:?} must be rejected");
        assert!(
            result.errors.iter().any(|e| e.contains(expected)),
            "op {op:?}: expected an error containing {expected:?}, got {:?}",
            result.errors
        );
    }
}
