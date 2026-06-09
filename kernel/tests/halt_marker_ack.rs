//! Audit M-3 — regression tests for the controlled
//! checker-disagreement halt-marker clear path. Pre-fix, the only way
//! to clear was `rm checker_disagreement_halt.json`; the audit flagged
//! this as an operational hazard (no audit trail, no probe-rerun
//! safeguard, no way to distinguish "operator investigated and decided"
//! from "operator was tired and rm'd").
//!
//! The fix adds `acknowledge_checker_disagreement_halt_marker` with
//! three explicit modes:
//!   * No flags + no probe → refused; operator must supply evidence.
//!   * Probe supplied with `axcheck.agreed=true` → cleared.
//!   * `force=true` → cleared unconditionally; logged as override.
//!
//! Every attempt is logged to `<runtime_root>/halt_history/ack_log.jsonl`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use tempfile::TempDir;
use trellis_kernel::model::{AxiomizationCheckOutput, LocalClosureProbeOutput, NodeId};
use trellis_kernel::runtime_cli_observations_halt::{
    acknowledge_checker_disagreement_halt_marker, checker_disagreement_halt_marker_path,
    checker_disagreement_halt_marker_present, halt_history_path, HaltMarkerAckOutcome,
    CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME,
};

/// Serialize tests that mutate `TRELLIS_KERNEL_CACHE_ROOT` — env var
/// reads are global, so parallel tests would race.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// RAII guard that swaps `TRELLIS_KERNEL_CACHE_ROOT` to `path` for the
/// lifetime of the guard and restores the prior value (if any) on drop.
struct CacheRootGuard {
    _lock: MutexGuard<'static, ()>,
    prior: Option<String>,
}

impl CacheRootGuard {
    fn install(path: &Path) -> Self {
        let lock = env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let prior = std::env::var("TRELLIS_KERNEL_CACHE_ROOT").ok();
        unsafe {
            std::env::set_var("TRELLIS_KERNEL_CACHE_ROOT", path);
        }
        Self { _lock: lock, prior }
    }
}

impl Drop for CacheRootGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.prior {
                Some(v) => std::env::set_var("TRELLIS_KERNEL_CACHE_ROOT", v),
                None => std::env::remove_var("TRELLIS_KERNEL_CACHE_ROOT"),
            }
        }
    }
}

fn local_tempdir() -> TempDir {
    let tmp_root = std::env::current_dir()
        .expect("current dir")
        .join(".tmp-tests");
    fs::create_dir_all(&tmp_root).expect("tmp root");
    tempfile::tempdir_in(&tmp_root).expect("tempdir")
}

fn write_minimal_halt_marker(root: &Path) {
    let path = root.join(CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME);
    fs::write(
        &path,
        serde_json::json!({
            "kind": "checker_disagreement",
            "schema_version": 1,
            "active_node": "test_node",
            "cycle": 7,
            "request_id": 42,
            "unix_ts": 0,
            "primary_only_axioms": ["sorryAx"],
            "axcheck_only_axioms": [],
            "primary_only_boundaries": [],
            "axcheck_only_boundaries": [],
            "primary_kernel_axioms": ["sorryAx", "propext"],
            "primary_boundary_theorems": ["Tablet.Helper"],
            "axcheck_kernel_axioms": ["propext"],
            "axcheck_boundary_theorems": ["Tablet.Helper"],
            "probe_errors": ["axiomization cross-check disagrees: primary-only axioms: [sorryAx]"],
            "probe_status": "checker_disagreement",
            "raw_stdout": "",
            "raw_stderr": "",
            "clear_instructions": "manually rm the file (pre-M-3)",
        })
        .to_string(),
    )
    .expect("write halt marker");
}

fn agreed_probe() -> LocalClosureProbeOutput {
    let mut probe = LocalClosureProbeOutput::default();
    probe.status = "ok".to_string();
    probe.kernel_axioms.insert("propext".to_string());
    probe.boundary_theorems.insert(
        NodeId::from("Tablet.Helper"),
        "stmt-hash".to_string(),
    );
    probe.axiomization_check = Some(AxiomizationCheckOutput {
        kernel_axioms: probe.kernel_axioms.clone(),
        boundary_theorems: BTreeSet::from(["Tablet.Helper".to_string()]),
        agreed: true,
        skipped: false,
        primary_only_axioms: Vec::new(),
        axcheck_only_axioms: Vec::new(),
        primary_only_boundaries: Vec::new(),
        axcheck_only_boundaries: Vec::new(),
        error: None,
    });
    probe
}

fn disagreed_probe() -> LocalClosureProbeOutput {
    let mut probe = LocalClosureProbeOutput::default();
    probe.status = "ok".to_string();
    probe.kernel_axioms.insert("sorryAx".to_string());
    probe.kernel_axioms.insert("propext".to_string());
    probe.axiomization_check = Some(AxiomizationCheckOutput {
        kernel_axioms: BTreeSet::from(["propext".to_string()]),
        boundary_theorems: BTreeSet::new(),
        agreed: false,
        skipped: false,
        primary_only_axioms: vec!["sorryAx".to_string()],
        axcheck_only_axioms: Vec::new(),
        primary_only_boundaries: Vec::new(),
        axcheck_only_boundaries: Vec::new(),
        error: None,
    });
    probe
}

#[test]
fn ack_refuses_without_probe_or_force() {
    let dir = local_tempdir();
    let _guard = CacheRootGuard::install(dir.path());
    write_minimal_halt_marker(dir.path());
    assert!(checker_disagreement_halt_marker_present());

    let outcome =
        acknowledge_checker_disagreement_halt_marker("operator was sleepy", false, None)
            .expect("ack returns structured outcome");
    match outcome {
        HaltMarkerAckOutcome::Refused { reason, history_path } => {
            assert!(
                reason.contains("no evidence supplied"),
                "refusal must explain why; got: {reason}"
            );
            assert!(history_path.exists(), "refused attempts must still log");
        }
        other => panic!("expected Refused, got {:?}", other),
    }
    // Marker still present — refused clears must NOT delete the marker.
    assert!(
        checker_disagreement_halt_marker_present(),
        "refused ack must not clear the marker"
    );
}

#[test]
fn ack_clears_with_force_flag() {
    let dir = local_tempdir();
    let _guard = CacheRootGuard::install(dir.path());
    write_minimal_halt_marker(dir.path());
    assert!(checker_disagreement_halt_marker_present());

    let outcome =
        acknowledge_checker_disagreement_halt_marker("operator override after investigation", true, None)
            .expect("ack returns outcome");
    match outcome {
        HaltMarkerAckOutcome::Cleared { mode, history_path } => {
            assert_eq!(mode, "force", "force flag must produce mode=force");
            assert!(history_path.exists(), "history line must be written");
            // History must record the override.
            let history_text =
                fs::read_to_string(&history_path).expect("read history");
            assert!(
                history_text.contains("\"mode\":\"force\""),
                "history must record mode=force for audit; got: {history_text}"
            );
            assert!(
                history_text.contains("operator override after investigation"),
                "history must record operator reason verbatim; got: {history_text}"
            );
        }
        other => panic!("expected Cleared, got {:?}", other),
    }
    assert!(
        !checker_disagreement_halt_marker_present(),
        "force ack must clear the marker"
    );
}

#[test]
fn ack_clears_when_probe_reagrees() {
    let dir = local_tempdir();
    let _guard = CacheRootGuard::install(dir.path());
    write_minimal_halt_marker(dir.path());
    assert!(checker_disagreement_halt_marker_present());

    let outcome = acknowledge_checker_disagreement_halt_marker(
        "re-ran probe; collectors now agree on canonical-only kernel_axioms",
        false,
        Some(&agreed_probe()),
    )
    .expect("ack returns outcome");
    match outcome {
        HaltMarkerAckOutcome::Cleared { mode, .. } => {
            assert_eq!(
                mode, "probe_reagree",
                "agreed probe should produce mode=probe_reagree"
            );
        }
        other => panic!("expected Cleared, got {:?}", other),
    }
    assert!(
        !checker_disagreement_halt_marker_present(),
        "agreed-probe ack must clear the marker"
    );
}

#[test]
fn ack_refuses_when_probe_still_disagrees() {
    let dir = local_tempdir();
    let _guard = CacheRootGuard::install(dir.path());
    write_minimal_halt_marker(dir.path());
    assert!(checker_disagreement_halt_marker_present());

    let outcome = acknowledge_checker_disagreement_halt_marker(
        "operator wanted to verify",
        false,
        Some(&disagreed_probe()),
    )
    .expect("ack returns outcome");
    match outcome {
        HaltMarkerAckOutcome::Refused { reason, history_path } => {
            assert!(
                reason.contains("still disagrees"),
                "must surface the still-disagrees diagnostic; got: {reason}"
            );
            assert!(history_path.exists());
        }
        other => panic!("expected Refused, got {:?}", other),
    }
    assert!(
        checker_disagreement_halt_marker_present(),
        "still-disagrees probe must NOT clear the marker"
    );
}

#[test]
fn ack_preserves_original_diagnostic_via_rename() {
    // M-3 forensics requirement: clearing the marker must NOT lose the
    // original JSON content. The fix renames the marker to
    // `*.acked-<ts>.json` rather than `rm`-ing it. Verify the original
    // payload is recoverable after a force-clear.
    let dir = local_tempdir();
    let _guard = CacheRootGuard::install(dir.path());
    write_minimal_halt_marker(dir.path());

    let _ = acknowledge_checker_disagreement_halt_marker("force-clear", true, None)
        .expect("ack returns");

    // Look for any file matching `checker_disagreement_halt.acked-*.json`.
    let entries: Vec<_> = fs::read_dir(dir.path())
        .expect("read runtime root")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let acked = entries
        .iter()
        .find(|n| n.starts_with("checker_disagreement_halt.acked-"));
    assert!(
        acked.is_some(),
        "force-clear must preserve the original diagnostic via rename; got entries: {entries:?}"
    );
}

#[test]
fn ack_history_is_append_only_jsonl() {
    // Multiple ack attempts must produce multiple history lines so an
    // operator can audit the full timeline.
    let dir = local_tempdir();
    let _guard = CacheRootGuard::install(dir.path());
    write_minimal_halt_marker(dir.path());

    // Attempt 1: refused.
    let _ = acknowledge_checker_disagreement_halt_marker("first try", false, None)
        .expect("ack 1");
    // Attempt 2: also refused.
    let _ = acknowledge_checker_disagreement_halt_marker(
        "second try with bad probe",
        false,
        Some(&disagreed_probe()),
    )
    .expect("ack 2");
    // Attempt 3: cleared with force.
    let _ = acknowledge_checker_disagreement_halt_marker(
        "operator override after investigation",
        true,
        None,
    )
    .expect("ack 3");

    let history_path = halt_history_path().expect("history path");
    let text = fs::read_to_string(&history_path).expect("read history");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "history must record all three attempts; got: {text}"
    );
    for line in &lines {
        // Each line must be a valid JSON object.
        let v: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|e| {
            panic!("history line not valid JSON: {line}; err: {e}");
        });
        assert_eq!(
            v.get("kind").and_then(|s| s.as_str()),
            Some("checker_disagreement_ack"),
            "every line must tag as checker_disagreement_ack"
        );
    }
}

#[test]
fn ack_no_marker_logs_attempt_but_returns_cleared() {
    // Defensive: if no marker is present at ack time, the operator
    // either pre-rm'd it OR the supervisor already cleared it.
    // Returning `Cleared { mode: "no_marker_present" }` keeps the
    // command idempotent. The history line still records the attempt.
    let dir = local_tempdir();
    let _guard = CacheRootGuard::install(dir.path());
    // Pre-condition: no marker present.
    assert!(!checker_disagreement_halt_marker_present());

    let outcome =
        acknowledge_checker_disagreement_halt_marker("nothing to clear", false, None)
            .expect("ack returns outcome");
    match outcome {
        HaltMarkerAckOutcome::Cleared { mode, history_path } => {
            assert_eq!(
                mode, "no_marker_present",
                "no-marker case must produce mode=no_marker_present"
            );
            assert!(history_path.exists(), "history line still written");
        }
        other => panic!("expected Cleared(no_marker_present), got {:?}", other),
    }
}

// Silence unused imports used elsewhere in the test fixtures.
fn _silence_unused() {
    let _: BTreeMap<NodeId, String> = BTreeMap::new();
    let _ = checker_disagreement_halt_marker_path;
}
