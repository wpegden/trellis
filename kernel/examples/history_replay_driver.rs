//! Persistent, filesystem-free driver for recorded kernel histories.
//!
//! ProtocolState deliberately skips derived closure reverse indices during
//! serialization.  Production rebuilds them on runtime load, and a replay
//! driver must do the same when a recorded checkpoint is installed.  Keeping
//! the process alive between events preserves those derived fields just as the
//! real SupervisorRuntime does.

use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::{self, BufRead, Write};
use trellis_kernel::ProtocolState;
use trellis_kernel::{apply_event, recompute_local_closure_reverse_indices, ProtocolEvent};

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum DriverRequest {
    SetState { state: ProtocolState },
    ApplyEvent { event: ProtocolEvent },
    GetState,
}

fn state_digest(state: &ProtocolState) -> Result<String, String> {
    let bytes = serde_json::to_vec(state)
        .map_err(|error| format!("failed to serialize state for digest: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn emit(value: &serde_json::Value) -> Result<(), String> {
    let stdout = io::stdout();
    let mut locked = stdout.lock();
    serde_json::to_writer(&mut locked, value)
        .map_err(|error| format!("failed to serialize response: {error}"))?;
    writeln!(locked).map_err(|error| format!("failed to terminate response: {error}"))?;
    locked
        .flush()
        .map_err(|error| format!("failed to flush response: {error}"))
}

fn run() -> Result<(), String> {
    let stdin = io::stdin();
    let mut state: Option<ProtocolState> = None;
    for (line_number, line) in stdin.lock().lines().enumerate() {
        let line = line.map_err(|error| format!("failed to read stdin: {error}"))?;
        let request: DriverRequest = serde_json::from_str(&line)
            .map_err(|error| format!("invalid request on line {}: {error}", line_number + 1))?;
        match request {
            DriverRequest::SetState {
                state: mut new_state,
            } => {
                // Match SupervisorRuntime::initialize/load normalization for
                // the pure state portions needed by apply_event.
                new_state.normalize_all_structural_state();
                recompute_local_closure_reverse_indices(&mut new_state);
                state = Some(new_state);
                emit(&json!({"status": "ready"}))?;
            }
            DriverRequest::ApplyEvent { event } => {
                let current = state
                    .take()
                    .ok_or_else(|| "apply_event received before set_state".to_string())?;
                match apply_event(current, event) {
                    Ok(outcome) => {
                        let next_state = outcome.state;
                        let commands = outcome.commands;
                        let response = json!({
                            "status": "success",
                            "commands": &commands,
                            "cycle": next_state.cycle,
                            "phase": &next_state.phase,
                            "stage": &next_state.stage,
                            "state_sha256": state_digest(&next_state)?,
                        });
                        // Keep the in-memory state, including fields skipped by
                        // serde, instead of round-tripping through `response`.
                        state = Some(next_state);
                        emit(&response)?;
                    }
                    Err(error) => {
                        emit(&json!({
                            "status": "error",
                            "error": format!("{error:?}"),
                        }))?;
                    }
                }
            }
            DriverRequest::GetState => {
                let current = state
                    .as_ref()
                    .ok_or_else(|| "get_state received before set_state".to_string())?;
                emit(&json!({
                    "status": "state",
                    "state": current,
                    "state_sha256": state_digest(current)?,
                }))?;
            }
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(2);
    }
}
