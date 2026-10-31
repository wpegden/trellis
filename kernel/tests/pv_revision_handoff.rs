use std::collections::{BTreeMap, BTreeSet};
use trellis_kernel::*;

fn completed_refutation() -> ProtocolState {
    let primary: ChallengeTargetId = "goal:correct".into();
    let refutation = refutation_target_id(&primary);
    let positive = ChallengeTargetSpec {
        kind: ChallengeTargetKind::Theorem,
        name: "Correct".into(),
        lean: "theorem Correct : False := by".into(),
        resolution: ChallengeResolution::Decide,
        ..Default::default()
    };
    let negative = ChallengeTargetSpec {
        kind: ChallengeTargetKind::Theorem,
        name: "Correct__Refutation".into(),
        lean: refutation_statement(&positive).unwrap(),
        ..Default::default()
    };
    let node: NodeId = "Correct__Refutation".into();
    let mut state = ProtocolState {
        phase: Phase::ProofFormalization,
        stage: Stage::Start,
        pv_tablet_configured: true,
        pv_revision_enabled: true,
        configured_challenge_targets: BTreeMap::from([(primary.clone(), positive), (refutation.clone(), negative)]),
        pv_live_polarity: BTreeMap::from([(primary, ChallengePolarity::Disprove)]),
        ..Default::default()
    };
    state.live.present_nodes.insert(node.clone());
    state.proof_nodes.insert(node.clone());
    state.challenge_claims.insert(node.clone(), BTreeSet::from([refutation]));
    state.deps.insert(node.clone(), BTreeSet::new());
    state.node_kinds.insert(node.clone(), NodeKind::Proof);
    state.corr_status.insert(node.clone(), CorrStatus::Pass);
    state.corr_approved_fingerprints.insert(node.clone(), "corr".into());
    state.live.corr_current_fingerprints.insert(node.clone(), "corr".into());
    state.sound_status.insert(node.clone(), SoundStatus::Pass);
    state.sound_approved_fingerprints.insert(node.clone(), "sound".into());
    state.live.sound_current_fingerprints.insert(node.clone(), "sound".into());
    state.substantiveness_status.insert(node.clone(), SubstantivenessStatus::Pass);
    state.substantiveness_approved_fingerprints.insert(node.clone(), "sub".into());
    state.live.substantiveness_current_fingerprints.insert(node.clone(), "sub".into());
    state.local_closure_records.insert(node.clone(), LocalClosureRecord { node, ..Default::default() });
    state.normalize_all_structural_state();
    state.ensure_node_metadata();
    state.commit_live();
    assert!(state.formalization_complete(), "{:?}", state.global_blockers());
    state
}

#[test]
fn pv_revision_handoff_parks_before_cleanup_and_without_source_bug_witness() {
    let state = completed_refutation();
    assert!(state.pv_refutation_findings.is_empty());
    let outcome = apply_event(state, ProtocolEvent::StartCycle).unwrap();
    assert_eq!(outcome.state.phase, Phase::ProofFormalization);
    assert_eq!(outcome.state.stage, Stage::Start);
    assert_eq!(outcome.commands, vec![ProtocolCommand::CommitCheckpoint]);
    let handoff = outcome.state.pv_revision_handoff.as_ref().unwrap();
    assert_eq!(handoff.cycle, 1);
    assert_eq!(handoff.refuted_goals[0].goal_id.as_str(), "goal:correct");
    assert_eq!(handoff.refuted_goals[0].refutation_target_id.as_str(), "goal:correct__refutation");
    assert_eq!(handoff.refuted_goals[0].refutation_nodes, BTreeSet::from(["Correct__Refutation".into()]));
    outcome.state.validate().unwrap();
    assert!(apply_event(outcome.state, ProtocolEvent::StartCycle).is_err());
}

#[test]
fn pv_revision_handoff_opt_in_and_pv_only() {
    for non_pv in [false, true] {
        let mut state = completed_refutation();
        if non_pv { state.pv_tablet_configured = false; } else { state.pv_revision_enabled = false; }
        let outcome = apply_event(state, ProtocolEvent::StartCycle).unwrap();
        assert_eq!(outcome.state.phase, Phase::Cleanup);
        assert!(outcome.state.pv_revision_handoff.is_none());
        assert!(outcome.commands.iter().any(|c| matches!(c, ProtocolCommand::SyncTabletRootForPaperTargets { .. })));
    }
    let json = serde_json::to_value(ProtocolState::default()).unwrap();
    assert!(json.get("pv_revision_enabled").is_none());
    assert!(json.get("pv_revision_handoff").is_none());
    let no_refutations = ProtocolState {
        phase: Phase::ProofFormalization, pv_tablet_configured: true,
        pv_revision_enabled: true, ..Default::default()
    };
    assert_eq!(apply_event(no_refutations, ProtocolEvent::StartCycle).unwrap().state.phase, Phase::Cleanup);
}

#[test]
fn pv_revision_handoff_excludes_open_stale_and_dormant_refutations() {
    let state = completed_refutation();
    let node: NodeId = "Correct__Refutation".into();
    let mut open = state.clone();
    open.live.open_nodes.insert(node.clone());
    assert!(open.pv_revision_refuted_goals().is_empty());
    let mut stale = state.clone();
    stale.local_closure_unverified_nodes.insert(node.clone());
    stale.local_closure_records.remove(&node);
    assert!(stale.pv_revision_refuted_goals().is_empty());
    let mut dormant = state;
    dormant.pv_live_polarity.clear();
    assert!(dormant.pv_revision_refuted_goals().is_empty());
}

struct NeverDispatch;
impl WrapperAdapter for NeverDispatch {
    fn dispatch(&mut self, _: &WrapperRequest) -> Result<WrapperResponse, String> { panic!("parked runtime dispatched") }
}

#[test]
fn pv_revision_handoff_runtime_resume_is_immutable_and_publishes_projection() {
    std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path());
    let mut runtime = SupervisorRuntime::initialize(paths.clone(), completed_refutation()).unwrap();
    let outcome = runtime.step(&mut NeverDispatch).unwrap();
    assert_eq!(outcome.status, RuntimeStepStatus::PvRevisionReady);
    let before = std::fs::read(&paths.state_path).unwrap();
    let event_count = runtime.event_count();
    let marker = dir.path().join("pv_revision_ready.json");
    let projection: serde_json::Value = serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
    assert_eq!(projection["schema_version"], 1);
    assert_eq!(projection["refuted_goals"][0]["goal_id"], "goal:correct");
    std::fs::remove_file(marker).unwrap();
    let mut loaded = SupervisorRuntime::load(paths.clone()).unwrap();
    assert_eq!(loaded.step(&mut NeverDispatch).unwrap().status, RuntimeStepStatus::PvRevisionReady);
    assert_eq!(loaded.event_count(), event_count);
    assert_eq!(std::fs::read(&paths.state_path).unwrap(), before);
    assert!(dir.path().join("pv_revision_ready.json").is_file());
    assert!(!loaded.try_post_load_state_migration(|_| panic!("parked state migrated")).unwrap());
    for action in ["step", "run"] {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new(env!("CARGO_BIN_EXE_trellis_runtime_cli"))
            .env_remove("TRELLIS_CHECKER_SOCKET")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().unwrap();
        child.stdin.take().unwrap().write_all(serde_json::json!({"action": action, "root": dir.path()}).to_string().as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["outcome"]["status"], "PvRevisionReady", "{result}");
        assert_eq!(std::fs::read(&paths.state_path).unwrap(), before);
    }
    }).unwrap().join().unwrap();
}

struct RejectCheckpoint;
impl CheckpointSink for RejectCheckpoint {
    fn commit(&mut self, _: &CheckpointHookPayload) -> Result<(), String> { Err("fixture checkpoint failure".into()) }
}

#[test]
fn pv_revision_handoff_checkpoint_failure_never_publishes_terminal_state() {
    std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path());
        let mut runtime = SupervisorRuntime::initialize(paths.clone(), completed_refutation()).unwrap();
        let before = std::fs::read(&paths.state_path).unwrap();
        assert!(runtime.step_with_checkpoint_sink(&mut NeverDispatch, &mut RejectCheckpoint).is_err());
        assert!(!runtime.state().pv_revision_ready());
        assert_eq!(std::fs::read(&paths.state_path).unwrap(), before);
        assert!(!dir.path().join("pv_revision_ready.json").exists());
        assert_eq!(runtime.event_count(), 0);
    }).unwrap().join().unwrap();
}

#[test]
fn pv_revision_stale_projection_is_removed_on_nonparked_load() {
    std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path());
        SupervisorRuntime::initialize(paths.clone(), ProtocolState::default()).unwrap();
        let marker = dir.path().join("pv_revision_ready.json");
        std::fs::write(&marker, b"{}").unwrap();
        SupervisorRuntime::load(paths).unwrap();
        assert!(!marker.exists());
    }).unwrap().join().unwrap();
}

#[test]
fn pv_revision_activation_only_changes_opt_in_and_replays_at_stopped_boundary() {
    let mut before = completed_refutation();
    before.cycle = 7;
    before.pv_revision_enabled = false;
    let output = apply_event(before.clone(), ProtocolEvent::ActivatePvRevision).unwrap();
    let mut expected = before.clone();
    expected.pv_revision_enabled = true;
    assert_eq!(output.state, expected);
    assert_eq!(output.commands, vec![ProtocolCommand::CommitCheckpoint]);
    assert!(output.state.pv_revision_handoff.is_none());
    let wire = serde_json::to_string(&ProtocolEvent::ActivatePvRevision).unwrap();
    assert_eq!(wire, "{\"event\":\"activate_pv_revision\"}");
    let replay = apply_event(before, serde_json::from_str(&wire).unwrap()).unwrap();
    assert_eq!(replay, output);
    let completed = apply_event(replay.state, ProtocolEvent::StartCycle).unwrap();
    assert!(completed.state.pv_revision_ready());
}

#[test]
fn pv_revision_activation_rejects_nonquiescent_early_nonpv_and_terminal_states() {
    let mut valid = completed_refutation();
    valid.cycle = 1;
    valid.pv_revision_enabled = false;
    for case in 0..6 {
        let mut bad = valid.clone();
        match case {
            0 => bad.cycle = 0,
            1 => bad.pv_tablet_configured = false,
            2 => bad.phase = Phase::Cleanup,
            3 => bad.phase = Phase::Complete,
            4 => bad.stage = Stage::Reviewer,
            5 => { bad.in_flight_request = Some(Box::new(bad.expected_request(1, RequestKind::Review))); },
            _ => unreachable!(),
        }
        assert!(apply_event(bad, ProtocolEvent::ActivatePvRevision).is_err(), "case {case}");
    }
}

fn cli_activation(root: &std::path::Path, confirmed: bool) -> serde_json::Value {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(env!("CARGO_BIN_EXE_trellis_runtime_cli"))
        .env_remove("TRELLIS_RUNTIME_CHECKPOINT_HOOK")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap();
    child.stdin.take().unwrap().write_all(serde_json::json!({
        "action": "activate_pv_revision", "root": root, "confirm_runtime_stopped": confirmed
    }).to_string().as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)))
}

#[test]
fn pv_revision_activation_cli_is_append_only_confirmed_and_idempotent() {
    std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path());
        let mut initial = completed_refutation();
        initial.cycle = 1;
        initial.pv_revision_enabled = false;
        let runtime = SupervisorRuntime::initialize(paths.clone(), initial).unwrap();
        // A stopped imported checkpoint with one historical record. Activation
        // appends to its prefix; the separate pure test checks event replay.
        let prefix = serde_json::json!({"index":0,"event":{"event":"start_cycle"},
            "commands":[],"phase":"ProofFormalization","stage":"Start","cycle":1}).to_string() + "\n";
        std::fs::create_dir_all(runtime.event_log_dir()).unwrap();
        let log = trellis_kernel::event_log_cycle_file(&runtime.event_log_dir(), 1);
        std::fs::write(&log, &prefix).unwrap();
        let unconfirmed = cli_activation(dir.path(), false);
        assert!(unconfirmed.to_string().contains("confirm_runtime_stopped=true"));
        let result = cli_activation(dir.path(), true);
        assert_eq!(result["state"]["pv_revision_enabled"], true, "{result}");
        assert_eq!(result["event_count"], 2);
        assert_eq!(result["outcome"]["event"]["event"], "activate_pv_revision");
        let appended = std::fs::read_to_string(&log).unwrap();
        assert!(appended.starts_with(&prefix));
        assert_eq!(appended.lines().count(), 2);
        let record: EventLogRecord = serde_json::from_str(appended.lines().nth(1).unwrap()).unwrap();
        assert_eq!(record.event, ProtocolEvent::ActivatePvRevision);
        assert_eq!(record.index, 1);
        let repeated = cli_activation(dir.path(), true);
        assert_eq!(repeated["steps_executed"], 0, "{repeated}");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), appended);
        let mut loaded = SupervisorRuntime::load(paths).unwrap();
        assert_eq!(loaded.step(&mut NeverDispatch).unwrap().status, RuntimeStepStatus::PvRevisionReady);
    }).unwrap().join().unwrap();
}

#[test]
fn pv_revision_activation_cli_refuses_held_native_controller_lock() {
    std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("native/generation");
        let root = dir.path().join("runtime");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(dir.path().join("job.json"), b"{}").unwrap();
        let lock = std::fs::File::create(dir.path().join("native-controller.lock")).unwrap();
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        SupervisorRuntime::initialize_with_metadata(RuntimePaths::new(&root), ProtocolState::default(),
            RuntimeMetadata { repo_path: Some(repo), ..Default::default() }).unwrap();
        let result = cli_activation(&root, true);
        assert!(result.to_string().contains("stopped native controller"), "{result}");
    }).unwrap().join().unwrap();
}

#[test]
fn pv_revision_activation_checkpoint_failure_rolls_back_opt_in() {
    std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path());
        let mut initial = completed_refutation();
        initial.cycle = 1;
        initial.pv_revision_enabled = false;
        let mut runtime = SupervisorRuntime::initialize(paths.clone(), initial).unwrap();
        let before = std::fs::read(&paths.state_path).unwrap();
        assert!(runtime.step_injected_event_with_checkpoint_sink(ProtocolEvent::ActivatePvRevision, &mut RejectCheckpoint).is_err());
        assert!(!runtime.state().pv_revision_enabled);
        assert_eq!(std::fs::read(&paths.state_path).unwrap(), before);
        assert_eq!(runtime.event_count(), 0);
    }).unwrap().join().unwrap();
}

#[test]
fn pv_revision_activation_cli_refuses_live_supervisor_identity() {
    std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
        use std::os::unix::process::CommandExt;
        let dir = tempfile::tempdir().unwrap();
        SupervisorRuntime::initialize(RuntimePaths::new(dir.path()), ProtocolState::default()).unwrap();
        let mut live = std::process::Command::new("/bin/sleep")
            .arg0("trellis_runtime_cli").arg("30")
            .env("TRELLIS_RUNTIME_ROOT", dir.path()).spawn().unwrap();
        let result = cli_activation(dir.path(), true);
        let _ = live.kill();
        let _ = live.wait();
        assert!(result.to_string().contains("stopped supervisor (pid"), "{result}");
    }).unwrap().join().unwrap();
}
