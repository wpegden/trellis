use std::io::Write;
use std::process::{Command, Stdio};

use trellis_kernel::{AbstractState, Phase, Stage, TransitionResponse};

fn run_cli(input: &str) -> std::process::Output {
    let exe = env!("CARGO_BIN_EXE_trellis_kernel_cli");
    let mut child = Command::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cli");
    child
        .stdin
        .as_mut()
        .expect("stdin available")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for cli output")
}

#[test]
fn cli_successfully_applies_transition_request() {
    let input = serde_json::json!({
        "state": {
            "stage": "Start"
        },
        "event": {
            "event": "start_cycle"
        }
    })
    .to_string();

    let output = run_cli(&input);
    assert_eq!(output.status.code(), Some(0));

    let response: TransitionResponse =
        serde_json::from_slice(&output.stdout).expect("parse success response");
    match response {
        TransitionResponse::Success { state, commands } => {
            assert_eq!(state.phase, Phase::TheoremStating);
            assert_eq!(state.stage, Stage::Worker);
            assert_eq!(state.cycle, 1);
            assert_eq!(commands.len(), 1);
        }
        other => panic!("expected success response, got {:?}", other),
    }
}

#[test]
fn cli_returns_structured_protocol_error() {
    let input = serde_json::json!({
        "state": {
            "phase": "Complete",
            "stage": "Start"
        },
        "event": {
            "event": "start_cycle"
        }
    })
    .to_string();

    let output = run_cli(&input);
    assert_eq!(output.status.code(), Some(1));

    let response: TransitionResponse =
        serde_json::from_slice(&output.stdout).expect("parse error response");
    match response {
        TransitionResponse::Error { error } => {
            assert_eq!(error.kind, "invalid_phase");
            assert!(error.message.contains("Complete"));
        }
        other => panic!("expected error response, got {:?}", other),
    }
}

#[test]
fn cli_returns_invalid_request_for_bad_json_shape() {
    let output = run_cli("{\"state\":{}}");
    assert_eq!(output.status.code(), Some(2));

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse invalid request response");
    assert_eq!(
        value.get("status").and_then(|v| v.as_str()),
        Some("invalid_request")
    );
    assert!(value
        .get("message")
        .and_then(|v| v.as_str())
        .is_some_and(|msg| msg.contains("missing field")));
    assert!(String::from_utf8(output.stderr)
        .expect("stderr utf8")
        .contains("invalid request JSON"));
}

#[test]
fn cli_roundtrips_defaultable_abstract_state() {
    let request = serde_json::json!({
        "state": AbstractState::default(),
        "event": {
            "event": "start_cycle"
        }
    })
    .to_string();

    let output = run_cli(&request);
    assert_eq!(output.status.code(), Some(0));

    let response: TransitionResponse =
        serde_json::from_slice(&output.stdout).expect("parse response");
    match response {
        TransitionResponse::Success { state, .. } => {
            assert_eq!(state.stage, Stage::Worker);
            assert_eq!(state.cycle, 1);
        }
        other => panic!("expected success response, got {:?}", other),
    }
}
