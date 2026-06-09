//! Snapshot tests for the `trellis_runtime_cli` wire format.
//!
//! ## Why this file
//!
//! The kernel is about to take a 20-25k LOC behavior-preserving refactor.
//! `runtime_cli_observations.rs`, `request_contracts.rs`, and the bin
//! `runtime_cli.rs` all spell the same wire format from slightly different
//! angles. A snapshot test per `RuntimeCliRequest` variant turns "did the
//! refactor change behavior?" into a one-second `cargo test` answer:
//! identical snapshots ⇒ behavior preserved; a diff ⇒ a load-bearing
//! tripwire fires.
//!
//! ## How
//!
//! For each pure (no fs / no spawn) variant of `RuntimeCliRequest`, the
//! file under `tests/fixtures/runtime_cli_snapshots/<action>/<case>.json`
//! defines a request. The test loads the request JSON, pipes it through
//! the binary, and uses `insta::assert_json_snapshot!` to pin the output
//! shape.
//!
//! ## Adding a new case
//!
//! 1. Create `tests/fixtures/runtime_cli_snapshots/<action>/<case>.json`
//!    with a valid request body.
//! 2. Re-run `cargo test --test runtime_cli_snapshots`. The first run
//!    creates a `*.snap.new` pending snapshot; inspect it with
//!    `cargo insta review`.
//! 3. Once accepted, the `*.snap` file lives alongside this test.
//!
//! ## Hard scope
//!
//! - Pure actions only: no fs writes that need a tempdir, no spawn-
//!   reentrant CLIs. Fs-touching actions (init, step, prepare_worker_gate,
//!   etc.) live in `runtime_cli.rs` integration tests; refactoring those
//!   requires the contract baseline harness in pass 4.
//! - Action variants covered here: 19 of 43 (the pure ones).
//!   - build_malformed_response
//!   - normalize_corr
//!   - normalize_human_gate
//!   - normalize_paper
//!   - normalize_review
//!   - normalize_sound
//!   - resolve_main_result_targets (read-only fs)
//!   - validate_correspondence_result
//!   - validate_deviation_authorization_result
//!   - validate_paper_faithfulness_result
//!   - validate_soundness_result
//!   - validate_substantiveness_result
//!   - validate_trellis_audit_result
//!   - validate_trellis_reviewer_result
//!   - validate_trellis_stuck_math_audit_result
//!   - validate_trellis_worker_result
//!
//! - Variants requiring a repo tempdir are out of scope for this file
//!   and tracked by the contract baseline harness (pass 4).
//!
//! ## Determinism
//!
//! The binary is deterministic on these inputs (no time, no rng, no
//! network). Outputs are JSON; insta's `assert_json_snapshot!` writes a
//! pretty-printed YAML representation, which is diff-friendly.

#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("runtime_cli_snapshots")
}

/// Pipe `request_json` to the binary's stdin and return the parsed stdout.
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
    assert!(
        output.status.success(),
        "runtime cli exited with non-zero status: {}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "could not parse runtime cli stdout as JSON: {err}\nstdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// Compose `action` + (`case_name`).json under the fixtures root.
fn fixture_path(action: &str, case: &str) -> PathBuf {
    fixtures_root().join(action).join(format!("{}.json", case))
}

fn load_fixture(action: &str, case: &str) -> Value {
    let path = fixture_path(action, case);
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|err| panic!("read fixture {}: {err}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|err| panic!("parse fixture {}: {err}", path.display()))
}

/// Helper: drive a single fixture through the binary and snapshot.
/// Snapshot name = `<action>__<case>` so all variants of an action
/// cluster together under `tests/snapshots/`.
macro_rules! snap_case {
    ($action:literal, $case:literal $(,)?) => {{
        let request = load_fixture($action, $case);
        let response = run_runtime_cli(&request);
        insta::with_settings!({
            snapshot_path => "snapshots/runtime_cli_snapshots",
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_json_snapshot!(
                format!("{}__{}", $action, $case),
                response
            );
        });
    }};
}

/// Pre-flight sanity: every fixture under tests/fixtures/runtime_cli_snapshots
/// loads as valid JSON and has at least one `action` field. Catches dropped
/// fixtures (or a hand-edited one that broke parse) before the per-case
/// tests crash with cryptic spawn errors.
#[test]
fn all_fixtures_parse() {
    let root = fixtures_root();
    let mut count = 0_usize;
    let walk = walk_dir(&root);
    for entry in walk {
        if entry.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let value: Value = serde_json::from_slice(
            &std::fs::read(&entry).unwrap_or_else(|err| panic!("read {}: {err}", entry.display())),
        )
        .unwrap_or_else(|err| panic!("parse {}: {err}", entry.display()));
        assert!(
            value.get("action").is_some(),
            "fixture {} missing top-level `action` field",
            entry.display()
        );
        count += 1;
    }
    assert!(
        count > 0,
        "no fixtures found under {} — the snapshot suite is unwired",
        root.display()
    );
}

fn walk_dir(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk_dir(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

// ============================================================================
// build_malformed_response — pure (no fs, no spawn)
// ============================================================================

#[test]
fn snapshot_build_malformed_response_worker() {
    snap_case!("build_malformed_response", "worker");
}

#[test]
fn snapshot_build_malformed_response_review() {
    snap_case!("build_malformed_response", "review");
}

#[test]
fn snapshot_build_malformed_response_audit() {
    snap_case!("build_malformed_response", "audit");
}

#[test]
fn snapshot_build_malformed_response_stuck_math_audit() {
    snap_case!("build_malformed_response", "stuck_math_audit");
}

// ============================================================================
// normalize_human_gate — pure (no fs)
// ============================================================================

#[test]
fn snapshot_normalize_human_gate_approve() {
    snap_case!("normalize_human_gate", "approve");
}

#[test]
fn snapshot_normalize_human_gate_feedback() {
    snap_case!("normalize_human_gate", "feedback");
}

#[test]
fn snapshot_normalize_human_gate_malformed() {
    snap_case!("normalize_human_gate", "malformed");
}

// ============================================================================
// validate_trellis_worker_result — pure
// ============================================================================

#[test]
fn snapshot_validate_trellis_worker_result_valid_outcome() {
    snap_case!("validate_trellis_worker_result", "valid_outcome");
}

#[test]
fn snapshot_validate_trellis_worker_result_invalid_outcome() {
    snap_case!("validate_trellis_worker_result", "invalid_outcome");
}

#[test]
fn snapshot_validate_trellis_worker_result_stuck_outcome_cleanup_rejected() {
    snap_case!(
        "validate_trellis_worker_result",
        "stuck_outcome_cleanup_rejected"
    );
}

#[test]
fn snapshot_validate_trellis_worker_result_missing_outcome() {
    snap_case!("validate_trellis_worker_result", "missing_outcome");
}

// ============================================================================
// validate_trellis_reviewer_result — pure
// ============================================================================

#[test]
fn snapshot_validate_trellis_reviewer_result_continue() {
    snap_case!("validate_trellis_reviewer_result", "continue");
}

#[test]
fn snapshot_validate_trellis_reviewer_result_need_input() {
    snap_case!("validate_trellis_reviewer_result", "need_input");
}

#[test]
fn snapshot_validate_trellis_reviewer_result_advance_phase() {
    snap_case!("validate_trellis_reviewer_result", "advance_phase");
}

#[test]
fn snapshot_validate_trellis_reviewer_result_done() {
    snap_case!("validate_trellis_reviewer_result", "done");
}

#[test]
fn snapshot_validate_trellis_reviewer_result_invalid_decision() {
    snap_case!("validate_trellis_reviewer_result", "invalid_decision");
}

// ============================================================================
// validate_trellis_audit_result — pure
// ============================================================================

#[test]
fn snapshot_validate_trellis_audit_result_ok() {
    snap_case!("validate_trellis_audit_result", "ok");
}

#[test]
fn snapshot_validate_trellis_audit_result_missing_fields() {
    snap_case!("validate_trellis_audit_result", "missing_fields");
}

// ============================================================================
// validate_trellis_stuck_math_audit_result — pure
// ============================================================================

#[test]
fn snapshot_validate_trellis_stuck_math_audit_result_ok() {
    snap_case!("validate_trellis_stuck_math_audit_result", "ok");
}

#[test]
fn snapshot_validate_trellis_stuck_math_audit_result_confirm_need_input() {
    snap_case!(
        "validate_trellis_stuck_math_audit_result",
        "confirm_need_input"
    );
}

// ============================================================================
// validate_paper_faithfulness_result — pure
// ============================================================================

#[test]
fn snapshot_validate_paper_faithfulness_result_pass() {
    snap_case!("validate_paper_faithfulness_result", "pass");
}

#[test]
fn snapshot_validate_paper_faithfulness_result_fail() {
    snap_case!("validate_paper_faithfulness_result", "fail");
}

// ============================================================================
// validate_deviation_authorization_result — pure
// ============================================================================

#[test]
fn snapshot_validate_deviation_authorization_result_approved() {
    snap_case!("validate_deviation_authorization_result", "approved");
}

#[test]
fn snapshot_validate_deviation_authorization_result_rejected() {
    snap_case!("validate_deviation_authorization_result", "rejected");
}

// ============================================================================
// validate_substantiveness_result — pure
// ============================================================================

#[test]
fn snapshot_validate_substantiveness_result_pass() {
    snap_case!("validate_substantiveness_result", "pass");
}

#[test]
fn snapshot_validate_substantiveness_result_fail() {
    snap_case!("validate_substantiveness_result", "fail");
}

// ============================================================================
// validate_correspondence_result — pure
// ============================================================================

#[test]
fn snapshot_validate_correspondence_result_pass() {
    snap_case!("validate_correspondence_result", "pass");
}

#[test]
fn snapshot_validate_correspondence_result_fail() {
    snap_case!("validate_correspondence_result", "fail");
}

// ============================================================================
// validate_soundness_result — pure (just needs node_name string)
// ============================================================================

#[test]
fn snapshot_validate_soundness_result_pass() {
    snap_case!("validate_soundness_result", "pass");
}

#[test]
fn snapshot_validate_soundness_result_fail() {
    snap_case!("validate_soundness_result", "fail");
}

// ============================================================================
// normalize_corr — pure
// ============================================================================

#[test]
fn snapshot_normalize_corr_pass() {
    snap_case!("normalize_corr", "pass");
}

#[test]
fn snapshot_normalize_corr_fail() {
    snap_case!("normalize_corr", "fail");
}

// ============================================================================
// normalize_paper — pure
// ============================================================================

#[test]
fn snapshot_normalize_paper_pass() {
    snap_case!("normalize_paper", "pass");
}

#[test]
fn snapshot_normalize_paper_fail_targets() {
    snap_case!("normalize_paper", "fail_targets");
}

// ============================================================================
// normalize_sound — pure
// ============================================================================

#[test]
fn snapshot_normalize_sound_pass() {
    snap_case!("normalize_sound", "pass");
}

#[test]
fn snapshot_normalize_sound_fail() {
    snap_case!("normalize_sound", "fail");
}

// ============================================================================
// normalize_review — pure
// ============================================================================

#[test]
fn snapshot_normalize_review_continue() {
    snap_case!("normalize_review", "continue");
}

#[test]
fn snapshot_normalize_review_need_input() {
    snap_case!("normalize_review", "need_input");
}

#[test]
fn snapshot_normalize_review_advance_phase() {
    snap_case!("normalize_review", "advance_phase");
}

// ============================================================================
// worker_blocker_status_block — kernel-rendered prose for the worker prompt
// (Phase 2 of the 2026-06-02 bridge-to-kernel migration). Covers no-blockers,
// single-actionable, multi-kind inline, no-actionable-fallback, and overflow
// sidecar cases.
// ============================================================================

#[test]
fn snapshot_worker_blocker_status_block_no_blockers() {
    snap_case!("worker_blocker_status_block", "no_blockers");
}

#[test]
fn snapshot_worker_blocker_status_block_single_actionable() {
    snap_case!("worker_blocker_status_block", "single_actionable");
}

#[test]
fn snapshot_worker_blocker_status_block_multi_kind_inline() {
    snap_case!("worker_blocker_status_block", "multi_kind_inline");
}

#[test]
fn snapshot_worker_blocker_status_block_no_actionable_fallback() {
    snap_case!("worker_blocker_status_block", "no_actionable_fallback");
}

#[test]
fn snapshot_worker_blocker_status_block_overflow_sidecar() {
    snap_case!("worker_blocker_status_block", "overflow_sidecar");
}

// ============================================================================
// review_blocker_choices_block — kernel-rendered prose for the reviewer
// prompt (Phase 2 of the 2026-06-04 bridge-to-kernel migration). Mirrors
// the worker-side cases above: no-blockers, single-actionable, multi-kind
// inline, no-actionable-fallback, and overflow sidecar. Bridge consumes
// the field via `_resolve_review_blocker_choices_block`.
// ============================================================================

#[test]
fn snapshot_review_blocker_choices_block_no_blockers() {
    snap_case!("review_blocker_choices_block", "no_blockers");
}

#[test]
fn snapshot_review_blocker_choices_block_single_actionable() {
    snap_case!("review_blocker_choices_block", "single_actionable");
}

#[test]
fn snapshot_review_blocker_choices_block_multi_kind_inline() {
    snap_case!("review_blocker_choices_block", "multi_kind_inline");
}

#[test]
fn snapshot_review_blocker_choices_block_no_actionable_fallback() {
    snap_case!("review_blocker_choices_block", "no_actionable_fallback");
}

#[test]
fn snapshot_review_blocker_choices_block_overflow_sidecar() {
    snap_case!("review_blocker_choices_block", "overflow_sidecar");
}

// ============================================================================
// resolve_main_result_targets — minimal fs (uses raw_targets when paper_path absent)
// ============================================================================

#[test]
fn snapshot_resolve_main_result_targets_explicit_targets() {
    let request = serde_json::json!({
        "action": "resolve_main_result_targets",
        "raw_targets": [
            {"start_line": 10, "end_line": 12, "tex_label": "thm:a"},
            {"start_line": 20, "end_line": 22, "tex_label": "thm:b"},
        ],
    });
    let response = run_runtime_cli(&request);
    insta::with_settings!({
        snapshot_path => "snapshots/runtime_cli_snapshots",
        prepend_module_to_snapshot => false,
    }, {
        insta::assert_json_snapshot!(
            "resolve_main_result_targets__explicit_targets",
            response
        );
    });
}

#[test]
fn snapshot_resolve_main_result_targets_with_labels_only() {
    let request = serde_json::json!({
        "action": "resolve_main_result_targets",
        "raw_labels": ["thm:a", "thm:b"],
    });
    let response = run_runtime_cli(&request);
    insta::with_settings!({
        snapshot_path => "snapshots/runtime_cli_snapshots",
        prepend_module_to_snapshot => false,
    }, {
        insta::assert_json_snapshot!(
            "resolve_main_result_targets__with_labels_only",
            response
        );
    });
}

/// Escape a literal string so it can be used as an insta filter regex
/// pattern. Equivalent to `regex::escape` but doesn't require the
/// `regex` crate as a direct dep.
fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

