//! Decide-pair one-location invariant at the step boundary (pre-merge audit,
//! `trust-v1-hotpatch-ratification`).
//!
//! Drives the runtime CLI `step` action against a real runtime root whose
//! configured Decide pair has been corrupted on disk into a divergence that is
//! NOT an admitted recovery prefix (both polarities' `.lean` present in
//! `Tablet/` with divergent content). The step must fail loudly BEFORE any
//! persist/checkpoint — the load-time `validate_configured_decide_layout`
//! guard (`runtime.rs` load path) rejects the layout, so the CLI returns an
//! error and the persisted state + event log stay byte-for-byte unadvanced.
//!
//! Constraints: small synthetic fixture, temp dirs rooted under the build area
//! via `common::project_tempdir` (never `/tmp`).

mod common;

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;
use trellis_kernel::{
    ChallengeResolution, ChallengeTargetId, ChallengeTargetKind, ChallengeTargetSpec, NodeId,
    NodeKind, Phase, ProtocolState, RuntimeMetadata, RuntimePaths, SupervisorRuntime,
};

/// A minimal state carrying a single configured `Decide` pair whose live
/// polarity is `Prove` (primary live in `Tablet/`, refutation dormant). Mirrors
/// the kernel's own `decide_state` unit fixture so the layout validator is
/// armed once the pair materializes on disk.
fn decide_pair_state() -> ProtocolState {
    let primary = ChallengeTargetId::from("correct");
    let mut state = ProtocolState::default();
    state.configured_challenge_targets.insert(
        primary.clone(),
        ChallengeTargetSpec {
            name: "Correct".into(),
            resolution: ChallengeResolution::Decide,
            ..ChallengeTargetSpec::default()
        },
    );
    state.configured_challenge_targets.insert(
        trellis_kernel::refutation_target_id(&primary),
        ChallengeTargetSpec {
            name: "Correct__Refutation".into(),
            ..ChallengeTargetSpec::default()
        },
    );
    // Prove polarity is the default (no `pv_live_polarity` entry).
    state
}

/// Seed the valid Prove-live layout: primary in `Tablet/`, refutation dormant.
fn seed_valid_prove_layout(repo: &Path) {
    for (dir, name, body) in [
        ("Tablet", "Correct", "PRIMARY"),
        ("Dormant", "Correct__Refutation", "REFUTATION"),
    ] {
        fs::create_dir_all(repo.join(dir)).unwrap();
        fs::write(repo.join(dir).join(format!("{name}.lean")), body).unwrap();
        fs::write(repo.join(dir).join(format!("{name}.tex")), body).unwrap();
    }
}

/// Snapshot a directory tree as a sorted list of `(relative-path, bytes)` so
/// two snapshots compare byte-for-byte. Missing dir → empty snapshot.
fn snapshot_tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let rel = path.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                out.push((rel, fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn run_cli(request: &Value) -> (Value, bool) {
    let (value, ok, _stderr) = run_cli_with_stderr(request);
    (value, ok)
}

fn run_cli_with_stderr(request: &Value) -> (Value, bool, String) {
    let exe = env!("CARGO_BIN_EXE_trellis_runtime_cli");
    let mut child = Command::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn runtime cli");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(request.to_string().as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "cli stdout not JSON: {err}\nstdout: {}\nstderr: {stderr}",
            String::from_utf8_lossy(&output.stdout),
        )
    });
    (value, output.status.success(), stderr)
}

/// A configured Decide pair corrupted into a NON-recoverable divergence (both
/// polarities' `.lean` present in `Tablet/` with divergent content) makes a
/// runtime-CLI `step` fail loudly at the pre-commit layout guard, leaving the
/// persisted state and event log byte-for-byte unadvanced.
#[test]
fn cli_step_rejects_decide_layout_divergence_before_commit() {
    let tmp = common::project_tempdir();
    let root = tmp.path().join("runtime");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();

    // A valid Prove-live pair, persisted into a fresh runtime root. Init does
    // not inspect the worktree, so this stands in for a healthy checkpoint.
    seed_valid_prove_layout(&repo);
    let state = decide_pair_state();
    let paths = RuntimePaths::new(root.clone());
    SupervisorRuntime::initialize_with_metadata(
        paths,
        state,
        RuntimeMetadata {
            repo_path: Some(repo.clone()),
            ..RuntimeMetadata::default()
        },
    )
    .expect("initialize runtime with a Prove-live Decide pair");

    // Corrupt the on-disk layout into a divergence that is NOT an admitted
    // recovery prefix: the dormant refutation's `.lean` is ALSO present in
    // Tablet/ with DIVERGENT content (its `.tex` stays only in Dormant/). The
    // refutation node now straddles Tablet(lean) + Dormant(lean,tex) — a
    // topology outside the closed forward/reverse prefix set, so recovery
    // leaves it untouched and validation must reject it.
    fs::write(
        repo.join("Tablet/Correct__Refutation.lean"),
        "DIVERGENT-TABLET-COPY",
    )
    .unwrap();

    let state_path: PathBuf = root.join("protocol_state.json");
    let event_log_dir = repo.join(".trellis-history/event-log");
    let state_before = fs::read(&state_path).expect("persisted state present");
    let events_before = snapshot_tree(&event_log_dir);

    // Drive the CLI `step` action. The load-time guard fires before any step
    // mutation, so the action fails.
    let request = serde_json::json!({ "action": "step", "root": root });
    let (response, ok) = run_cli(&request);
    assert!(!ok, "step must fail on a divergent Decide layout: {response:#}");
    assert_eq!(response["status"], "error", "response: {response:#}");
    let message = response["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("invalid disk layout") && message.contains("Correct__Refutation"),
        "error must name the pair/one-location invariant violation; got: {message}"
    );

    // Nothing was persisted or advanced: state bytes and the event log are
    // byte-for-byte identical to before the failed step.
    let state_after = fs::read(&state_path).expect("persisted state still present");
    assert_eq!(
        state_before, state_after,
        "a rejected step must not rewrite the persisted state"
    );
    assert_eq!(
        events_before,
        snapshot_tree(&event_log_dir),
        "a rejected step must not append to the event log"
    );
}

fn decide_pair_state_missing_deps() -> ProtocolState {
    let primary = ChallengeTargetId::from("goal:correct");
    let mut state = ProtocolState::default();
    state.phase = Phase::TheoremStating;
    state.pv_tablet_configured = true;
    state.configured_challenge_targets.insert(
        primary.clone(),
        ChallengeTargetSpec {
            kind: ChallengeTargetKind::Theorem,
            name: "Correct".into(),
            lean: "theorem Correct : a = b := by".into(),
            resolution: ChallengeResolution::Decide,
            ..ChallengeTargetSpec::default()
        },
    );
    state.configured_challenge_targets.insert(
        trellis_kernel::refutation_target_id(&primary),
        ChallengeTargetSpec {
            kind: ChallengeTargetKind::Theorem,
            name: "Correct__Refutation".into(),
            lean: "theorem Correct__Refutation : ¬ (a = b) := by".into(),
            ..ChallengeTargetSpec::default()
        },
    );
    let node = NodeId::from("Correct");
    state.live.present_nodes.insert(node.clone());
    state.committed.present_nodes.insert(node.clone());
    state.proof_nodes.insert(node.clone());
    state.node_kinds.insert(node.clone(), NodeKind::Proof);
    state.committed_node_kinds.insert(node, NodeKind::Proof);
    // Deliberately NO `state.deps` entry.
    state
}

fn protocol_position(state_path: &Path) -> Vec<(String, Value)> {
    let state: Value =
        serde_json::from_slice(&fs::read(state_path).expect("persisted state present")).unwrap();
    [
        "phase",
        "stage",
        "cycle",
        "attempt",
        "request_seq",
        "active_node",
        "pending_task",
        "gate_kind",
    ]
    .into_iter()
    .map(|key| (key.to_string(), state.get(key).cloned().unwrap_or(Value::Null)))
    .collect()
}

#[test]
fn cli_step_rejects_present_node_without_node_kind_or_deps() {
    let tmp = common::project_tempdir();
    let root = tmp.path().join("runtime");
    let repo = tmp.path().join("repo");
    // Seed a VALID Prove-side layout so the disk-layout validator passes and the
    // registration invariant below is what actually fires. (On the pre-trust-v1
    // base this test was written against, no layout validator existed.)
    seed_valid_prove_layout(&repo);

    let config_path = repo.join("trellis.config.json");
    fs::write(
        &config_path,
        serde_json::json!({
            "repo_path": repo,
            "worker": {"provider": "codex", "model": "worker-a", "label": "worker-a"},
            "reviewer": {"provider": "codex", "model": "reviewer-a", "label": "reviewer-a"},
            "workflow": {}
        })
        .to_string(),
    )
    .unwrap();

    let paths = RuntimePaths::new(root.clone());
    SupervisorRuntime::initialize_with_metadata(
        paths,
        decide_pair_state_missing_deps(),
        RuntimeMetadata {
            repo_path: Some(repo.clone()),
            config_path: Some(config_path),
            ..RuntimeMetadata::default()
        },
    )
    .expect("initialize runtime with an unregistered Decide-pair node");

    let state_path: PathBuf = root.join("protocol_state.json");
    let event_log_dir = repo.join(".trellis-history/event-log");

    let request = serde_json::json!({ "action": "step", "root": root });

    // Pass 1 — drives the load path to completion so every rewrite it is
    // ENTITLED to make is already materialised on disk. Measured, on this
    // fixture, as: `corr_fingerprint_schema_version`, `easy_attempts`, `live`,
    // `local_closure_failures`, `local_closure_unverified_nodes`,
    // `node_difficulty`, and (new) `challenge_claims` from the B2
    // decide-registration migration. None of those is protocol advancement,
    // and all of them are idempotent — which is what pass 2 then lets us
    // assert against the WHOLE state file rather than a field whitelist.
    let (response, ok) = run_cli(&request);
    assert!(
        !ok,
        "step must fail on an unregistered Decide-pair node: {response:#}"
    );
    assert_eq!(response["status"], "error", "response: {response:#}");
    let message = response["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("present decide-pair node Correct has no deps entry"),
        "error must name the node and the missing registration; got: {message}"
    );

    // Snapshot the ENTIRE persisted state, not a hand-picked list of protocol-
    // position keys (audit round 2, F5). The position-only form left every
    // other field unguarded: verifier statuses, fingerprints and trust_base
    // projections could all be mutated and persisted by a rejected step and the
    // test would still pass.
    let state_before = fs::read(&state_path).expect("persisted state present");
    let position_before = protocol_position(&state_path);
    let events_before = snapshot_tree(&event_log_dir);

    // Pass 2 — the assertion. `validate()` fires inside `apply_event`, i.e.
    // after the (now no-op) load path, so a rejected step must leave the state
    // file byte-for-byte identical and the event log unappended.
    let (response, ok) = run_cli(&request);
    assert!(
        !ok,
        "step must still fail on an unregistered Decide-pair node: {response:#}"
    );
    let message = response["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("present decide-pair node Correct has no deps entry"),
        "the rejection must be stable across passes; got: {message}"
    );

    assert_eq!(
        state_before,
        fs::read(&state_path).expect("persisted state still present"),
        "a rejected step must not rewrite ANY of the persisted state"
    );
    assert_eq!(
        position_before,
        protocol_position(&state_path),
        "a rejected step must not advance the persisted protocol position"
    );
    assert_eq!(
        events_before,
        snapshot_tree(&event_log_dir),
        "a rejected step must not append to the event log"
    );
}

/// A `Decide` pair flipped LIVE onto `Disprove`, whose refutation node is
/// present and proof-bearing but carries NO `challenge_claims` entry — the
/// state the pre-fix `apply_decide_polarity_flip` left behind for
/// `dec2flt_total__Refutation` and `parse_number_faithful__Refutation`.
fn stranded_disprove_state() -> ProtocolState {
    let primary = ChallengeTargetId::from("goal:correct");
    let mut state = ProtocolState::default();
    state.phase = Phase::TheoremStating;
    state.pv_tablet_configured = true;
    state.configured_challenge_targets.insert(
        primary.clone(),
        ChallengeTargetSpec {
            kind: ChallengeTargetKind::Theorem,
            name: "Correct".into(),
            lean: "theorem Correct : a = b := by".into(),
            resolution: ChallengeResolution::Decide,
            ..ChallengeTargetSpec::default()
        },
    );
    state.configured_challenge_targets.insert(
        trellis_kernel::refutation_target_id(&primary),
        ChallengeTargetSpec {
            kind: ChallengeTargetKind::Theorem,
            name: "Correct__Refutation".into(),
            lean: "theorem Correct__Refutation : ¬ (a = b) := by".into(),
            ..ChallengeTargetSpec::default()
        },
    );
    state
        .pv_live_polarity
        .insert(primary, trellis_kernel::ChallengePolarity::Disprove);
    let node = NodeId::from("Correct__Refutation");
    state.live.present_nodes.insert(node.clone());
    state.committed.present_nodes.insert(node.clone());
    state.proof_nodes.insert(node.clone());
    state.node_kinds.insert(node.clone(), NodeKind::Proof);
    state.committed_node_kinds.insert(node.clone(), NodeKind::Proof);
    state.deps.insert(node, Default::default());
    // Deliberately NO `challenge_claims` entry — nothing backfills it.
    state
}

/// Seed the Disprove-live layout: the refutation in `Tablet/`, the primary
/// dormant.
fn seed_valid_disprove_layout(repo: &Path) {
    for (dir, name, body) in [
        ("Tablet", "Correct__Refutation", "REFUTATION"),
        ("Dormant", "Correct", "PRIMARY"),
    ] {
        fs::create_dir_all(repo.join(dir)).unwrap();
        fs::write(repo.join(dir).join(format!("{name}.lean")), body).unwrap();
        fs::write(repo.join(dir).join(format!("{name}.tex")), body).unwrap();
    }
}

/// Audit round 2, B2 / named test 6 — the load-time migration, end to end
/// through the real CLI. The stranded state LOADS (it is not rejected), the
/// claim is re-derived from `configured_challenge_targets` and PERSISTED, and
/// the repair is announced on stderr rather than applied silently.
#[test]
fn cli_load_migrates_stranded_decide_challenge_claim_and_logs_it() {
    let tmp = common::project_tempdir();
    let root = tmp.path().join("runtime");
    let repo = tmp.path().join("repo");
    seed_valid_disprove_layout(&repo);

    let config_path = repo.join("trellis.config.json");
    fs::write(
        &config_path,
        serde_json::json!({
            "repo_path": repo,
            "worker": {"provider": "codex", "model": "worker-a", "label": "worker-a"},
            "reviewer": {"provider": "codex", "model": "reviewer-a", "label": "reviewer-a"},
            "workflow": {}
        })
        .to_string(),
    )
    .unwrap();

    let paths = RuntimePaths::new(root.clone());
    SupervisorRuntime::initialize_with_metadata(
        paths,
        stranded_disprove_state(),
        RuntimeMetadata {
            repo_path: Some(repo.clone()),
            config_path: Some(config_path),
            ..RuntimeMetadata::default()
        },
    )
    .expect("initialize runtime with a stranded Disprove-live Decide pair");

    let state_path: PathBuf = root.join("protocol_state.json");
    let before: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    assert!(
        before["challenge_claims"]
            .get("Correct__Refutation")
            .is_none(),
        "fixture precondition: the persisted claim is stranded"
    );

    let request = serde_json::json!({ "action": "step", "root": root });
    let (_response, _ok, stderr) = run_cli_with_stderr(&request);

    // The migration ran before the step, persisted, and said so.
    assert!(
        stderr.contains("DECIDE-REGISTRATION MIGRATION")
            && stderr.contains("challenge_claims[Correct__Refutation]"),
        "the repair must be logged, not silent; stderr was:\n{stderr}"
    );
    let after: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    assert_eq!(
        after["challenge_claims"]["Correct__Refutation"],
        serde_json::json!(["goal:correct__refutation"]),
        "the live refutation must be re-derived as claiming its own configured target"
    );

    // One-shot: a second load repairs nothing and logs nothing.
    let (_response, _ok, stderr) = run_cli_with_stderr(&request);
    assert!(
        !stderr.contains("DECIDE-REGISTRATION MIGRATION"),
        "the migration must not re-fire on an already-healed state; stderr:\n{stderr}"
    );
}
