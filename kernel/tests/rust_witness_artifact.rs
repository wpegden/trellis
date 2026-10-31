use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use trellis_kernel::trust_base::{
    accept_rust_witness_correspondence, attach_execution_receipt,
    build_rust_witness_correspondence_request, close_rust_witness_correspondence_verdict,
    execute_approved_json, raw_sha256, rust_witness_relative_path, source_tree_digest,
    ApprovedExecutionRequest, ExecutionLimits, PinnedExecutionRequest, RustWitnessArtifactDeclaration,
    RustWitnessCorrespondenceDecision, RustWitnessCorrespondenceLaneVerdict, RustWitnessExecution,
    RUST_WITNESS_EXECUTION_PURPOSE, RUST_WITNESS_RUNNER_LOGICAL_ID,
};
use trellis_kernel::trust_base::{VerifiedEvidenceClosure, VerifiedEvidenceLeaf};
use trellis_kernel::{
    ChallengePolarity, ChallengeResolution, ChallengeTargetId, ChallengeTargetKind,
    ChallengeTargetProvenance, ChallengeTargetSpec, LocalClosureRecord, ProtocolState,
    StatementProvenance, WrapperRequest,
};

fn copy_regular(source: &Path, destination: &Path) {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::copy(source, destination).unwrap();
}

fn target_state() -> (ProtocolState, ChallengeTargetId) {
    let target = ChallengeTargetId::from("goal:synthetic_add_offset");
    let primary = ChallengeTargetSpec {
        kind: ChallengeTargetKind::Theorem,
        name: "add_offset_spec".into(),
        lean: "theorem add_offset_spec (value offset : Int) : value + offset = value + offset := by".into(),
        informal: "bounded addition agrees with the source function".into(),
        provenance: ChallengeTargetProvenance::default(),
        resolution: ChallengeResolution::Decide,
        statement_provenance: StatementProvenance::SeedPinned,
        ..ChallengeTargetSpec::default()
    };
    let twin_id = trellis_kernel::refutation_target_id(&target);
    let twin = ChallengeTargetSpec {
        kind: ChallengeTargetKind::Theorem,
        name: trellis_kernel::refutation_node_name(&primary.name),
        lean: trellis_kernel::refutation_statement(&primary).unwrap(),
        informal: trellis_kernel::refutation_informal(&primary),
        provenance: ChallengeTargetProvenance::default(),
        resolution: ChallengeResolution::Prove,
        statement_provenance: StatementProvenance::KernelDerived,
        ..ChallengeTargetSpec::default()
    };
    let mut state = ProtocolState::default();
    state.configured_challenge_targets.insert(target.clone(), primary);
    state.configured_challenge_targets.insert(twin_id, twin);
    state.pv_live_polarity.insert(target.clone(), ChallengePolarity::Disprove);
    state.trust_base.advance_gate_episode_id = Some("episode-host-run".into());
    (state, target)
}

/// This is intentionally a real host test. A managed environment which cannot
/// nest bwrap fails here; the production host must execute it without a bypass.
#[test]
fn real_runner_receipt_record_and_correspondence_binding() {
    let manifest_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let trellis_root = manifest_root.parent().unwrap();
    let fixture = manifest_root.join("tests/fixtures/pv_generic_source");
    let work = manifest_root
        .join("target/rust-witness-host-test")
        .join(std::process::id().to_string());
    if work.exists() {
        fs::remove_dir_all(&work).unwrap();
    }
    let crate_root = work.join("evidence/source/crate");
    for relative in ["Cargo.toml", "Cargo.lock", "src/lib.rs"] {
        copy_regular(&fixture.join(relative), &crate_root.join(relative));
    }
    let repo = work.join("repo");
    copy_regular(&fixture.join("GOAL.md"), &repo.join("GOAL.md"));
    let (state, target) = target_state();
    let declaration = RustWitnessArtifactDeclaration {
        target_id: target.clone(),
        relative_path: rust_witness_relative_path(&target),
    };
    let witness_path = repo.join(&declaration.relative_path);
    fs::create_dir_all(witness_path.parent().unwrap()).unwrap();
    let witness = b"use pv_generic_fixture::add_offset;\n#[test]\nfn witness() {\n    let observed = add_offset(i16::MAX, 1);\n    println!(\"observed={observed}\");\n    assert_eq!(observed, i16::MAX);\n}\n";
    fs::write(&witness_path, witness).unwrap();
    let runtime = work.join("runtime");
    fs::create_dir_all(&runtime).unwrap();

    let runner = trellis_root.join("scripts/pv_artifact_runner.py");
    let runner_sha = raw_sha256(&fs::read(&runner).unwrap());
    let evidence = VerifiedEvidenceClosure {
        manifest_sha256: raw_sha256(b"manifest"),
        evidence_tool_input_root: raw_sha256(b"evidence-root"),
        file_count: 1,
        leaves_by_logical_id: BTreeMap::from([(
            RUST_WITNESS_RUNNER_LOGICAL_ID.into(),
            VerifiedEvidenceLeaf {
                kind: "runtime_source".into(),
                relative_path: "scripts/pv_artifact_runner.py".into(),
                byte_length: fs::metadata(&runner).unwrap().len(),
                raw_sha256: runner_sha,
                dependency_ids: vec!["trellis-runtime-source:trellis/sandbox.py".into()],
            },
        )]),
    };
    let sysroot = String::from_utf8(
        Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let toolchain = PathBuf::from(sysroot.trim()).canonicalize().unwrap();
    let rustc = toolchain.join("bin/rustc");
    let cargo = toolchain.join("bin/cargo");
    let rustc_vv = String::from_utf8(Command::new(&rustc).arg("-vV").output().unwrap().stdout).unwrap();
    let artifact_sha = raw_sha256(witness);
    let crate_sha = source_tree_digest(&crate_root).unwrap();
    let lock_sha = raw_sha256(&fs::read(crate_root.join("Cargo.lock")).unwrap());
    let goal_sha = raw_sha256(&fs::read(repo.join("GOAL.md")).unwrap());
    let run_id = "host-real-run";
    let request = json!({
        "schema": "trellis-pv-rust-witness-runner-request/v1",
        "target_id": target,
        "relative_path": declaration.relative_path,
        "artifact_source_base64": BASE64_STANDARD.encode(witness),
        "artifact_sha256": artifact_sha,
        "worker_source_path": witness_path.to_string_lossy(),
        "pinned_crate_root": crate_root.to_string_lossy(),
        "pinned_crate_tree_sha256": crate_sha,
        "goal_path": repo.join("GOAL.md").to_string_lossy(),
        "goal_sha256": goal_sha,
        "cargo_lock_sha256": lock_sha,
        "rustc_path": rustc.to_string_lossy(),
        "rustc_vv": rustc_vv,
        "cargo_path": cargo.to_string_lossy(),
        "toolchain_root": toolchain.to_string_lossy(),
        "dependency_cache_path": "",
        "runtime_root": runtime.to_string_lossy(),
        "run_id": run_id,
        "limits": {
            "timeout_seconds": 60,
            "cpu_seconds": 45,
            "address_space_bytes": 8_589_934_592_u64,
            "file_size_bytes": 67_108_864,
            "process_count": 4096,
            "stdout_bytes": 65_536,
            "stderr_bytes": 65_536,
        },
    });
    let (mut record, mut payload) = trellis_kernel::trust_base::snapshot_rust_witness_source(
        &state,
        &repo,
        &declaration,
        crate_sha,
        request,
    )
    .unwrap();
    let environment = BTreeMap::from([
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("PYTHONPATH".into(), trellis_root.to_string_lossy().into_owned()),
        ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
    ]);
    let launch = raw_sha256(b"launch");
    let receipt = execute_approved_json(ApprovedExecutionRequest {
        evidence: &evidence,
        tool_logical_id: RUST_WITNESS_RUNNER_LOGICAL_ID,
        purpose: RUST_WITNESS_EXECUTION_PURPOSE,
        launch_acknowledgment_sha256: launch,
        execution: PinnedExecutionRequest {
            runner_path: &runner,
            expected_runner_sha256: runner_sha,
            command_id: "pv-rust-witness-artifact-runner",
            working_directory: &runtime,
            environment: &environment,
            input: &payload.runner_request,
            limits: ExecutionLimits {
                timeout: Duration::from_secs(90),
                max_stdin_bytes: 2 * 1024 * 1024,
                max_stdout_bytes: 4 * 1024 * 1024,
                max_stderr_bytes: 4 * 1024 * 1024,
            },
        },
    })
    .unwrap();
    assert!(
        receipt.parsed_output().is_some(),
        "host sandbox did not produce a receipt; stderr={}",
        String::from_utf8_lossy(
            &BASE64_STANDARD
                .decode(receipt.value()["stderr_base64"].as_str().unwrap())
                .unwrap()
        )
    );
    attach_execution_receipt(&mut record, &mut payload, &receipt, &evidence, launch).unwrap();
    assert!(matches!(record.execution, RustWitnessExecution::Receipt { exit_code: Some(0), .. }));
    let corr_request = build_rust_witness_correspondence_request(
        &record,
        &payload,
        "bounded addition agrees with the source function".into(),
        state.configured_challenge_targets[&target].lean.clone(),
        state.configured_challenge_targets[&trellis_kernel::refutation_target_id(&target)]
            .lean
            .clone(),
        LocalClosureRecord::default(),
    )
    .unwrap();
    payload.correspondence_request = Some(corr_request.clone());
    let mut wrapper_request = WrapperRequest::default();
    wrapper_request.kind = trellis_kernel::RequestKind::Corr;
    wrapper_request.rust_witness_artifact_correspondence = Some(corr_request.clone());
    let rendered = trellis_kernel::request_contracts::correspondence_contract_payload(
        &wrapper_request,
        Some(trellis_root),
    );
    assert_eq!(
        rendered.pointer("/request_summary/rust_witness_artifact_correspondence"),
        Some(&serde_json::to_value(&corr_request).unwrap())
    );
    assert_eq!(
        rendered
            .pointer("/request_summary/rust_witness_artifact_correspondence")
            .and_then(serde_json::Value::as_object)
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([
            "artifact_source_utf8".into(),
            "checked_negative_proof_closure".into(),
            "execution_receipt".into(),
            "goal_target_prose_utf8".into(),
            "negated_target_lean_utf8".into(),
            "request_sha256".into(),
            "reviewed_digests".into(),
            "schema".into(),
            "target_id".into(),
            "target_lean_utf8".into(),
        ])
    );
    let lane = RustWitnessCorrespondenceLaneVerdict {
        request_sha256: corr_request.request_sha256,
        same_witness_as_lean_disproof: true,
        invokes_pinned_crate_operation: true,
        observation_contradicts_goal_obligation: true,
        reviewed_digests: corr_request.reviewed_digests.clone(),
        decision: RustWitnessCorrespondenceDecision::Pass,
        reason: "the observed boundary case is the formal disproof witness".into(),
    };
    let verdict = close_rust_witness_correspondence_verdict(&lane).unwrap();
    accept_rust_witness_correspondence(
        &mut record,
        &mut payload,
        &corr_request,
        verdict,
    )
    .unwrap();
    let mut mismatched = lane;
    mismatched.request_sha256 = raw_sha256(b"mismatch");
    assert!(accept_rust_witness_correspondence(
        &mut record,
        &mut payload,
        &corr_request,
        close_rust_witness_correspondence_verdict(&mismatched).unwrap(),
    )
    .is_err());
}
