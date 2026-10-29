//! Persistence guards for the reduced trust state and unchanged generic wires.

use std::fs;
use std::path::PathBuf;

use trellis_kernel::runtime::{CheckpointHookPayload, EventLogRecord, RuntimeCheckpoint};
use trellis_kernel::{ProtocolEvent, ProtocolState, TrustBaseMode, WrapperResponse};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("state_fixture_roundtrip")
}

fn read_fixture(name: &str) -> Vec<u8> {
    fs::read(fixture_dir().join(name)).expect("read committed fixture")
}

#[test]
fn current_math_mode_state_round_trips_byte_identically() {
    let fixture = read_fixture("math_mode_state.json");
    let state: ProtocolState = serde_json::from_slice(&fixture).expect("load math state");
    assert_eq!(state.trust_base.mode, TrustBaseMode::Disabled);
    assert_eq!(state.trust_base.format_version, 0);
    assert_eq!(
        serde_json::to_vec_pretty(&state).unwrap(),
        fixture.strip_suffix(b"\n").unwrap_or(&fixture)
    );
}

#[test]
fn current_math_checkpoint_wires_round_trip_byte_identically() {
    let fixture = read_fixture("math_mode_checkpoint.json");
    let checkpoint: RuntimeCheckpoint = serde_json::from_slice(&fixture).expect("load checkpoint");
    assert_eq!(
        serde_json::to_vec_pretty(&checkpoint).unwrap(),
        fixture.strip_suffix(b"\n").unwrap_or(&fixture)
    );

    let fixture = read_fixture("math_mode_checkpoint_hook_payload.json");
    let payload: CheckpointHookPayload =
        serde_json::from_slice(&fixture).expect("load checkpoint hook payload");
    assert_eq!(
        serde_json::to_vec_pretty(&payload).unwrap(),
        fixture.strip_suffix(b"\n").unwrap_or(&fixture)
    );
}

#[test]
fn current_math_stuck_audit_wires_round_trip_byte_identically() {
    let fixture = read_fixture("math_stuck_audit_event_line.jsonl");
    let line = fixture.strip_suffix(b"\n").expect("newline-terminated event");
    let record: EventLogRecord = serde_json::from_slice(line).expect("load event record");
    assert!(matches!(
        record.event,
        ProtocolEvent::WrapperResponse {
            response: WrapperResponse::StuckMathAudit(_)
        }
    ));
    let mut encoded = serde_json::to_vec(&record).unwrap();
    encoded.push(b'\n');
    assert_eq!(encoded, fixture);

    let fixture = read_fixture("math_stuck_audit_bridge_payload.json");
    let response: WrapperResponse = serde_json::from_slice(&fixture).expect("load bridge response");
    let mut value = serde_json::to_value(response).unwrap();
    value.as_object_mut().unwrap().insert(
        "kind".to_string(),
        serde_json::Value::String("stuck_math_audit".to_string()),
    );
    assert_eq!(serde_json::to_vec(&value).unwrap(), fixture);
}

#[test]
fn old_authorization_bearing_state_is_refused() {
    let fixture = read_fixture("pre_prune_trust_state.json");
    let error = serde_json::from_slice::<ProtocolState>(&fixture).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("unknown field") || message.contains("format_version"),
        "unexpected refusal: {message}"
    );
}

#[test]
fn new_required_state_round_trip_uses_current_format() {
    let mut state = ProtocolState::default();
    state.trust_base.mode = TrustBaseMode::RequiredV1;
    state.trust_base.format_version = trellis_kernel::model::TRUST_STATE_FORMAT_VERSION;
    let encoded = serde_json::to_vec(&state).unwrap();
    let decoded: ProtocolState = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(
        decoded.trust_base.format_version,
        trellis_kernel::model::TRUST_STATE_FORMAT_VERSION
    );
    let text = String::from_utf8(encoded).unwrap();
    for removed in [
        "journal_checkpoint",
        "package_authorization_event_hash",
        "conditional_theorem_candidates",
        "source_validation_guidance",
        "pending_trust_witness_reports",
    ] {
        assert!(!text.contains(removed), "new state contains {removed}");
    }
}
