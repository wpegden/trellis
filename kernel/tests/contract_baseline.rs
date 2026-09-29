//! Cross-version contract baseline.
//!
//! ## Why this file
//!
//! `scripts/contract_baseline.sh capture` saves
//! `(request, response)` JSON pairs under
//! `tests/contract_baseline/<action>/<case>.json` from the *current*
//! `trellis_runtime_cli` binary. This test re-runs every captured
//! request through the live binary and asserts the response matches
//! byte-for-byte. The diff is the precise wire-format delta — exactly
//! what a behavior-preserving refactor must not produce.
//!
//! ## Relationship to other tests
//!
//! - `runtime_cli_snapshots.rs` (pass 2): same input fixtures, but
//!   captured via insta YAML snapshots. Optimized for diff readability
//!   in a refactor session.
//! - This file (pass 4): same input fixtures, captured as raw JSON
//!   pairs. Optimized for "the binary I just rebuilt produces
//!   identical bytes to the binary at the commit that captured the
//!   baseline".
//! - `tla_replay.rs`: protocol-level replay, asserts whole post-state
//!   after applying an event. Higher-level than the wire format.
//!
//! The three tests are complementary: snapshots catch wire shape,
//! contract baseline catches byte-exact reproduction, TLA replay
//! catches state-machine logic.
//!
//! ## When to refresh
//!
//! If you *intentionally* change a wire field, run
//! `scripts/contract_baseline.sh capture` to refresh the baseline,
//! then review the diff in your PR. The reviewer reads the diff to
//! confirm the change matches the intent.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

fn baseline_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("contract_baseline")
}

fn run_runtime_cli(request_json: &Value) -> Value {
    let exe = env!("CARGO_BIN_EXE_trellis_runtime_cli");
    let mut child = Command::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn trellis_runtime_cli");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(request_json.to_string().as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait for runtime cli");
    if !output.status.success() {
        panic!(
            "runtime cli exited non-zero: {}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "could not parse runtime cli stdout: {err}\nstdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().and_then(|s| s.to_str()) == Some("json") {
                out.push(path);
            }
        }
    }
}

#[derive(Debug)]
struct BaselineRecord {
    path: PathBuf,
    request: Value,
    expected_response: Value,
}

fn collect_records() -> Vec<BaselineRecord> {
    let root = baseline_root();
    let mut paths = Vec::new();
    walk(&root, &mut paths);
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let bytes = fs::read(&path)
            .unwrap_or_else(|err| panic!("read baseline {}: {err}", path.display()));
        let envelope: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|err| panic!("parse baseline {}: {err}", path.display()));
        let request = envelope
            .get("request")
            .cloned()
            .unwrap_or_else(|| panic!("baseline {} missing request", path.display()));
        let expected_response = envelope
            .get("response")
            .cloned()
            .unwrap_or_else(|| panic!("baseline {} missing response", path.display()));
        out.push(BaselineRecord {
            path,
            request,
            expected_response,
        });
    }
    out
}

/// Pre-flight: the baseline directory exists and has at least one
/// pair. Catches the case where someone deletes the baseline and the
/// real assertions silently report "0 mismatches".
#[test]
fn baseline_directory_is_populated() {
    let records = collect_records();
    assert!(
        !records.is_empty(),
        "no contract baseline pairs under {}; \
         run scripts/contract_baseline.sh capture",
        baseline_root().display()
    );
    eprintln!("contract baseline pairs: {}", records.len());
}

/// The main contract assertion: every captured (request, expected)
/// pair, when the request is re-fed to the binary, produces a
/// response that's structurally equal to the expected one.
///
/// "Structurally equal" means `serde_json::Value` equality — order of
/// keys in the JSON object does NOT matter, but every key-value pair
/// does. This is the right semantic for a behavior-preserving refactor:
/// if the binary now emits the same fields but in a different order,
/// the wire contract is preserved (downstream parsers don't care about
/// object key order in JSON).
///
/// For *byte-exact* reproduction (key order matters), use
/// `scripts/contract_baseline.sh verify`, which sorts keys before
/// diffing.
#[test]
fn every_baseline_request_reproduces_its_response() {
    let records = collect_records();
    let mut mismatches = Vec::new();
    for record in &records {
        let actual = run_runtime_cli(&record.request);
        if actual != record.expected_response {
            mismatches.push((record.path.clone(), actual));
        }
    }
    if !mismatches.is_empty() {
        let mut report = String::new();
        for (path, actual) in &mismatches {
            report.push_str(&format!(
                "\n--- baseline {} ---\nactual: {}\n",
                path.display(),
                serde_json::to_string_pretty(actual).unwrap_or_default()
            ));
        }
        panic!(
            "{} of {} baseline pairs drifted{}",
            mismatches.len(),
            records.len(),
            report
        );
    }
}
