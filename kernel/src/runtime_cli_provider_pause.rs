//! Provider allowance exhaustion parks a dispatch without committing a response.

use serde::Deserialize;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use trellis_kernel::{RuntimeError, WrapperRequest};

pub const PAUSED: &str = "trellis: provider budget pause recorded";

#[derive(Deserialize)]
struct BudgetPause {
    provider: String,
    model: Option<String>,
    role: String,
    reason: String,
    detected_at: String,
}

/// Called only for a nonzero bridge exit with an explicit operational payload.
/// An I/O or decoding failure remains a real runtime error, never a clean pause.
pub fn record(root: &Path, request: &WrapperRequest, value: &Value) -> Result<(), String> {
    let pause: BudgetPause = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid provider budget pause: {error}"))?;
    if pause.provider.is_empty() || pause.reason.is_empty() {
        return Err("provider budget pause has no provider or reason".into());
    }
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let payload = json!({
        "kind": "provider_budget",
        "launch_attempt": std::env::var("TRELLIS_LAUNCH_ATTEMPT").ok(),
        "runtime_root": fs::canonicalize(root).map_err(|error| error.to_string())?,
        "supervisor_executable": std::env::current_exe().map_err(|error| error.to_string())?,
        "armed_by": "supervisor",
        "armed_at": pause.detected_at,
        "armed_at_epoch": epoch,
        "armed_at_cycle": request.cycle,
        "reason": format!("{} {}: {}", pause.provider, pause.role, pause.reason),
        "detail": {
            "provider": pause.provider, "model": pause.model, "role": pause.role,
            "request_id": request.id, "request_kind": request.kind,
        },
    });
    let target = root.join("pause_request.json");
    let tmp = root.join(format!(".provider-budget-pause-{}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        serde_json::to_writer_pretty(&mut file, &payload)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, &target)?;
        fs::File::open(root)?.sync_all()
    };
    let result =
        write().map_err(|error| format!("failed to record provider budget pause: {error}"));
    let _ = fs::remove_file(tmp);
    result
}

pub fn step_error(error: RuntimeError) -> String {
    match error {
        RuntimeError::Adapter(message) if message == PAUSED => message,
        other => format!("runtime step failed: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trellis_kernel::{
        ProtocolState, RequestKind, RuntimePaths, Stage, SupervisorRuntime, WrapperAdapter,
    };

    #[test]
    fn provider_budget_pause_preserves_pending_request_and_persisted_state() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = crate::process_globals_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("runtime");
        let mut state = ProtocolState::default();
        state.stage = Stage::Worker;
        state.request_seq = 42;
        state.in_flight_request = Some(Box::new(state.expected_request(42, RequestKind::Worker)));
        let paths = RuntimePaths::new(&root);
        let config_path = dir.path().join("config.json");
        fs::write(
            &config_path,
            json!({"worker":{"provider":"codex","model":"test"}}).to_string(),
        )
        .unwrap();
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            state,
            trellis_kernel::RuntimeMetadata {
                config_path: Some(config_path.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let before = fs::read(&paths.state_path).unwrap();
        let bridge = dir.path().join("bridge.sh");
        fs::write(&bridge, concat!(
            "#!/bin/sh\ncat >/dev/null\n",
            "printf '%s\\n' '{\"ok\":false,\"pause\":{\"kind\":\"provider_budget\",\"provider\":\"codex\",\"model\":\"test\",\"role\":\"worker\",\"reason\":\"usage_limit_reached; resets tomorrow\",\"detected_at\":\"2026-09-28T19:00:00Z\"}}'\nexit 3\n",
        )).unwrap();
        fs::set_permissions(&bridge, fs::Permissions::from_mode(0o700)).unwrap();
        let mut adapter = crate::ProcessBridgeAdapter {
            command: bridge,
            config_path,
            runtime_root: root.clone(),
            repo_path: None,
        };
        let error = runtime.step(&mut adapter).unwrap_err();
        assert_eq!(step_error(error), PAUSED);
        assert_eq!(fs::read(&paths.state_path).unwrap(), before);
        assert_eq!(runtime.state().in_flight_request.as_ref().unwrap().id, 42);
        let record: Value =
            serde_json::from_slice(&fs::read(root.join("pause_request.json")).unwrap()).unwrap();
        assert_eq!(record["kind"], "provider_budget");
        assert_eq!(record["detail"]["request_id"], 42);
        assert!(record["reason"]
            .as_str()
            .unwrap()
            .contains("resets tomorrow"));
        assert!(!root.join("runtime_error_halt.json").exists());

        // Reloading for Resume exposes exactly the same pending request.
        let resumed = SupervisorRuntime::load(paths).unwrap();
        assert_eq!(resumed.state().in_flight_request.as_ref().unwrap().id, 42);
        assert_eq!(
            adapter
                .dispatch(resumed.state().in_flight_request.as_ref().unwrap())
                .unwrap_err(),
            PAUSED
        );
        assert_eq!(fs::read(root.join("protocol_state.json")).unwrap(), before);
    }

    #[test]
    fn provider_budget_pause_write_failure_and_other_errors_remain_errors() {
        let dir = tempfile::tempdir().unwrap();
        let payload = json!({"provider":"codex", "role":"worker", "reason":"usage_limit_reached", "detected_at":"now"});
        assert!(record(
            &dir.path().join("missing"),
            &WrapperRequest::default(),
            &payload
        )
        .is_err());
        assert!(record(dir.path(), &WrapperRequest::default(), &json!({})).is_err());
        assert_ne!(
            step_error(RuntimeError::Adapter("429 transient throttle".into())),
            PAUSED
        );
    }
}
