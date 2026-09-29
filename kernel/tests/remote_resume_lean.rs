//! Opt-in real Lean fixture helper, driven by tests/test_remote_resume_lean.py.
//! No provider adapter is ever constructed here.
use serde_json::json;
use std::{fs, path::PathBuf, io::Write, process::{Command, Stdio}};
use trellis_kernel::{runtime::*, *};

#[test]
#[ignore = "requires the isolated real checker prepared by test_remote_resume_lean.py"]
fn remote_resume_seed_and_epoch() {
    let repo = PathBuf::from(std::env::var("TRELLIS_RESUME_TEST_REPO").unwrap());
    let root = PathBuf::from(std::env::var("TRELLIS_RESUME_TEST_ROOT").unwrap());
    if std::env::var("TRELLIS_RESUME_TEST_MODE").as_deref() == Ok("load") {
        let expected: serde_json::Value = serde_json::from_slice(&fs::read(root.join("protocol_state.json")).unwrap()).unwrap();
        let runtime = SupervisorRuntime::load(RuntimePaths::new(&root)).unwrap();
        assert_eq!(serde_json::to_value(runtime.state()).unwrap(), expected);
        validate_rollback_artifact_epoch(&root, runtime.state()).unwrap();
        let config: serde_json::Value = serde_json::from_slice(&fs::read(repo.join("trellis.config.json")).unwrap()).unwrap();
        assert_eq!(config["repo_path"], json!(repo));
        return;
    }
    if std::env::var("TRELLIS_RESUME_TEST_MODE").as_deref() == Ok("legacy_checkpoint") {
        // Real Lean evidence, but a repository-less runtime for this exact
        // response/hook boundary: no filesystem refresh or provider adapter.
        // The real hook commits post-response state before the runtime appends
        // the final Review response to its own log.
        let mut state: ProtocolState = serde_json::from_slice(&fs::read(root.join("protocol_state.json")).unwrap()).unwrap();
        model::recompute_local_closure_reverse_indices(&mut state);
        state.stage = Stage::Reviewer;
        let request = state.issue_request(RequestKind::Review);
        let response = ReviewResponse { request_id:request.id, cycle:state.cycle,
            status:ResponseStatus::Ok, decision:ReviewDecisionKind::Continue, next_mode:state.current_mode(),
            paper_focus_ranges:vec![PaperFocusRange{start_line:1,end_line:1,reason:"fixture source".into(),doc:None}],
            paper_grounding:PaperGrounding{consulted_cited_ranges:true,basis_summary:"The fixture paper states that seven is seven.".into()},
            ..Default::default() };
        assert!(state.review_response_legal(&response), "{:?}", state.review_response_rejection_reasons(&response));
        let isolated = root.join("legacy-producer"); fs::create_dir_all(&isolated).unwrap();
        let metadata = RuntimeMetadata { config_path:Some(repo.join("trellis.config.json")), ..Default::default() };
        let log_dir = event_log_dir_for(&isolated, &metadata); fs::create_dir_all(&log_dir).unwrap();
        let mut count = 0;
        for path in event_log_cycle_files(&repo.join(".trellis-history/event-log")).unwrap() {
            count += fs::read_to_string(&path).unwrap().lines().count();
            fs::copy(&path, log_dir.join(path.file_name().unwrap())).unwrap();
        }
        fs::write(isolated.join("protocol_state.json"),serde_json::to_vec(&state).unwrap()).unwrap();
        fs::write(isolated.join("runtime_metadata.json"),serde_json::to_vec(&metadata).unwrap()).unwrap();
        fs::copy(root.join("checkpoint.json"),isolated.join("checkpoint.json")).unwrap();
        let mut runtime = SupervisorRuntime::load(RuntimePaths::new(&isolated)).unwrap();
        fs::write(root.join("legacy-predecessor.json"),json!({"event_count":count-1,"state":runtime.state()}).to_string()).unwrap();
        struct Hook { repo: PathBuf }
        impl CheckpointSink for Hook {
            fn commit(&mut self, payload: &CheckpointHookPayload) -> Result<(), String> {
                let mut archived=payload.clone(); archived.metadata.repo_path=Some(self.repo.clone());
                let mut child=Command::new("python3").args(["-m","trellis.runtime.git_checkpoint_hook"])
                    .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
                child.stdin.take().unwrap().write_all(&serde_json::to_vec(&archived).unwrap()).unwrap();
                let output=child.wait_with_output().unwrap();
                assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr)); Ok(())
            }
        }
        runtime.step_injected_event_with_checkpoint_sink(ProtocolEvent::WrapperResponse {
            response:WrapperResponse::Review(response) }, &mut Hook {repo}).unwrap();
        assert!(runtime.state().human_input_outstanding);
        return;
    }
    if std::env::var("TRELLIS_RESUME_TEST_MODE").as_deref() == Ok("last_clean") {
        let mut state: ProtocolState =
            serde_json::from_slice(&fs::read(root.join("protocol_state.json")).unwrap()).unwrap();
        trellis_kernel::model::recompute_local_closure_reverse_indices(&mut state);
        let expected_records = state.last_clean_local_closure_records.clone();
        state.stage = Stage::Reviewer;
        state.verifier_lanes = build_verifier_lanes(1);
        state.human_input_outstanding = false;
        state.cycles_since_clean = 2;
        let request = state.issue_request(RequestKind::Review);
        let response = ReviewResponse {
            request_id: request.id,
            cycle: state.cycle,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            reset: ResetChoice::LastClean,
            next_mode: TaskMode::Local,
            ..Default::default()
        };
        assert!(
            state.review_response_legal(&response),
            "{:?}",
            state.review_response_rejection_reasons(&response)
        );
        fs::write(
            root.join("protocol_state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        let mut runtime = SupervisorRuntime::load(RuntimePaths::new(&root)).unwrap();
        let result = runtime
            .step_injected_event_with_checkpoint_sink(
                ProtocolEvent::WrapperResponse {
                    response: WrapperResponse::Review(response),
                },
                &mut NoopCheckpointSink,
            )
            .unwrap();
        assert!(result
            .commands
            .iter()
            .any(|command| matches!(command, ProtocolCommand::RestoreWorktreeToLastClean)));
        assert_eq!(runtime.state().local_closure_records, expected_records);
        assert!(fs::read_to_string(repo.join("Tablet/Value.lean"))
            .unwrap()
            .contains(":= 7"));
        validate_rollback_artifact_epoch(&root, runtime.state()).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(repo.join("trellis.config.json")).unwrap()).unwrap();
        assert_eq!(config["repo_path"], json!(repo));
        return;
    }
    if std::env::var("TRELLIS_RESUME_TEST_MODE").as_deref() == Ok("epoch") {
        let state: ProtocolState =
            serde_json::from_slice(&fs::read(root.join("protocol_state.json")).unwrap()).unwrap();
        validate_rollback_artifact_epoch(&root, &state).unwrap();
        activate_rollback_artifact_epoch(&root, &repo, &state).unwrap();
        let mut future = state.clone();
        for record in future.local_closure_records.values_mut() {
            record.accepted_at_snapshot_id.push_str("-future");
        }
        retain_rollback_artifact_epoch(&root, &repo, &future).unwrap();
        validate_rollback_artifact_epoch(&root, &future).unwrap();
        activate_rollback_artifact_epoch(&root, &repo, &future).unwrap();
        activate_rollback_artifact_epoch(&root, &repo, &state).unwrap();
        return;
    }
    let mut state = ProtocolState::default();
    state.corr_fingerprint_schema_version = 4;
    state.verifier_lanes = build_verifier_lanes(1);
    state.phase = Phase::ProofFormalization;
    state.cycle = 7;
    state.human_input_outstanding = true;
    state.node_kinds = [
        ("Preamble".into(), NodeKind::Preamble),
        ("Value".into(), NodeKind::Definition),
        ("Fact".into(), NodeKind::Proof),
    ]
    .into();
    state.proof_nodes.insert("Fact".into());
    state.live.present_nodes = state.node_kinds.keys().cloned().collect();
    state.deps = trellis_kernel::worker_normalization::direct_deps_from_repo(
        &repo,
        &state.live.present_nodes,
    );
    state.live = observe_trusted_rebind_view(&repo, &state, &repo.join("paper.tex")).unwrap();
    state.corr_approved_fingerprints = state.live.corr_current_fingerprints.clone();
    for node in &state.live.present_nodes {
        state.corr_status.insert(node.clone(), CorrStatus::Pass);
    }
    state.committed = state.live.clone();
    state.committed_node_kinds = state.node_kinds.clone();
    state.committed_proof_nodes = state.proof_nodes.clone();
    state.committed_deps = state.deps.clone();
    state.committed_target_claims = state.target_claims.clone();
    state.ensure_node_metadata();
    state.validate().unwrap();
    let metadata = RuntimeMetadata {
        repo_path: Some(repo.clone()),
        config_path: Some(repo.join("trellis.config.json")),
        ..Default::default()
    };
    let checkpoint = RuntimeCheckpoint {
        cycle: state.cycle,
        phase: state.phase,
        gate_kind: state.gate_kind,
        active_node: None,
        committed: state.committed.clone(),
        cleanup_unreachable_deletion: None,
    };
    fs::write(
        root.join("protocol_state.json"),
        serde_json::to_vec_pretty(&state).unwrap(),
    )
    .unwrap();
    fs::write(
        root.join("checkpoint.json"),
        serde_json::to_vec_pretty(&checkpoint).unwrap(),
    )
    .unwrap();
    fs::create_dir_all(repo.join(".trellis-history/event-log")).unwrap();
    let event = ProtocolEvent::TrustedArtifactRebind {
        payload: trusted_artifact_rebind::TrustedArtifactRebind::between(
            &state,
            &state,
            "0".repeat(64),
        )
        .unwrap(),
    };
    let record = EventLogRecord {
        index: 0,
        event,
        commands: vec![],
        cycle: state.cycle,
        phase: state.phase,
        stage: state.stage,
        ts_ms: 0,
        trust_record: None,
        additional_trust_records: vec![],
    };
    fs::write(
        repo.join(".trellis-history/event-log/cycle-000007.jsonl"),
        format!("{}\n", serde_json::to_string(&record).unwrap()),
    )
    .unwrap();
    fs::write(repo.join(".trellis-history/supervisor_state.json"), serde_json::to_vec_pretty(&json!({"event_count":1,"event_count_convention":"record_count","state":state,"checkpoint":checkpoint,"metadata":metadata,"commands":[]})).unwrap()).unwrap();
}
