#![recursion_limit = "256"]

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use trellis_kernel::phase0::*;
use trellis_kernel::trust_base::{
    build_evidence_manifest, canonical_json_value, execute_phase0_checked_json,
    phase0_ablated_tree_digest, raw_sha256, source_tree_manifest,
    verify_evidence_tool_manifest, AdaptationCitation, AdaptationSeamClass, ByteSpan,
    CampaignTrustPlan, CheckerStream, EvidenceInput, ExactBytePatch, ExecutionLimits,
    Phase0AdaptationEntry, Phase0AdaptationLedger, Phase0ExecutionRequest, Phase0FileOperation,
    Phase0LedgerStatus, PinnedExecutionRequest, QuotedCheckerError, Sha256Digest,
    TargetTrustPlan, CAMPAIGN_TRUST_PLAN_SCHEMA_V4, PHASE0_ADAPTATION_LEDGER_SCHEMA,
};

struct Fixture {
    _temp: tempfile::TempDir,
    raw: PathBuf,
    candidate: PathBuf,
    runner: PathBuf,
    context: Phase0ValidationContext,
    genesis: Phase0Genesis,
    identity: Phase0AdaptationLedger,
}

fn write_program(path: &Path) {
    fs::write(
        path,
        b"#!/usr/bin/python3\nimport json,sys\nr=json.load(sys.stdin)\nsys.stdout.write(json.dumps(r['mock_output'],sort_keys=True,separators=(',',':')))\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }
}

#[test]
fn python_and_rust_clean_git_checkout_roots_match_with_tracked_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let checkout = temp.path().join("tool-checkout");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("source.txt"), b"tracked source\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("source.txt", checkout.join("source-link")).unwrap();
    for arguments in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Phase0 Fixture",
            "-c",
            "user.email=phase0@example.invalid",
            "commit",
            "-q",
            "-m",
            "fixture",
        ],
    ] {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(arguments)
            .status()
            .unwrap()
            .success());
    }
    let revision = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let revision = revision.trim();
    let rust = git_checkout_source_digest(&checkout, revision).unwrap();
    let trellis_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let python = Command::new("python3")
        .current_dir(trellis_root)
        .args([
            "-c",
            "import sys; from pathlib import Path; from scripts.pv_source_tree import git_checkout_digest; print(git_checkout_digest(Path(sys.argv[1]), sys.argv[2]))",
        ])
        .arg(&checkout)
        .arg(revision)
        .output()
        .unwrap();
    assert!(python.status.success(), "{}", String::from_utf8_lossy(&python.stderr));
    assert_eq!(String::from_utf8(python.stdout).unwrap().trim(), rust.to_string());

    let closure = temp.path().join("readonly-closure");
    fs::create_dir(&closure).unwrap();
    fs::write(closure.join("data"), b"closure bytes\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("data", closure.join("link")).unwrap();
    let rust_closure = readonly_closure_digest(&closure).unwrap();
    let python_closure = Command::new("python3")
        .current_dir(trellis_root)
        .args([
            "-c",
            "import sys; from pathlib import Path; from scripts.pv_source_tree import readonly_closure_digest; print(readonly_closure_digest(Path(sys.argv[1])))",
        ])
        .arg(&closure)
        .output()
        .unwrap();
    assert!(
        python_closure.status.success(),
        "{}",
        String::from_utf8_lossy(&python_closure.stderr)
    );
    assert_eq!(
        String::from_utf8(python_closure.stdout).unwrap().trim(),
        rust_closure.to_string()
    );

    fs::write(checkout.join("source.txt"), b"drifted source\n").unwrap();
    assert!(git_checkout_source_digest(&checkout, revision).is_err());
}

#[test]
fn python_and_rust_partition_manifests_agree_with_supporting_origin() {
    let temp = tempfile::tempdir().unwrap();
    let union = temp.path().join("union");
    fs::create_dir(&union).unwrap();
    fs::write(union.join("module.rs"), b"use crate::traits::Float;\n").unwrap();
    fs::write(union.join("traits.rs"), b"pub trait Float {}\n").unwrap();
    let union_manifest = source_tree_manifest(&union).unwrap();
    let trellis_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let python = Command::new("python3")
        .current_dir(trellis_root)
        .args([
            "-c",
            "import sys; from pathlib import Path; from scripts.pv_source_tree import build_source_partition_manifest,canonical_json,source_tree_manifest; sys.stdout.buffer.write(canonical_json(build_source_partition_manifest(source_tree_manifest(Path(sys.argv[1])), {'module.rs'})))",
        ])
        .arg(&union)
        .output()
        .unwrap();
    assert!(python.status.success(), "{}", String::from_utf8_lossy(&python.stderr));
    let partition: Phase0SourcePartitionManifest = serde_json::from_slice(&python.stdout).unwrap();
    partition.validate(&union_manifest).unwrap();
    assert_eq!(partition.files.len(), 2);
    assert_eq!(partition.files[0].origin, Phase0SourcePartitionOrigin::Target);
    assert_eq!(partition.files[1].origin, Phase0SourcePartitionOrigin::Supporting);
    assert_ne!(partition.target_source_tree_sha256, partition.supporting_source_tree_sha256);
}

fn lane(id: &str) -> Phase0LaneBinding {
    Phase0LaneBinding {
        lane_id: id.to_owned(),
        provider: "stub".to_owned(),
        model: "deterministic".to_owned(),
        effort: "fixed".to_owned(),
        extra_args: Vec::new(),
        fallback_models: Vec::new(),
        binding_sha256: Sha256Digest::ZERO,
    }
    .seal()
    .unwrap()
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let raw = temp.path().join("raw");
    let candidate = temp.path().join("candidate");
    fs::create_dir(&raw).unwrap();
    fs::write(raw.join("lib.rs"), b"pub fn target(x: u64) -> u64 { x }\n").unwrap();
    fs::create_dir(&candidate).unwrap();
    fs::copy(raw.join("lib.rs"), candidate.join("lib.rs")).unwrap();
    let tools = temp.path().join("tools");
    fs::create_dir(&tools).unwrap();
    let runner = tools.join("checker.py");
    write_program(&runner);
    let goal = b"### `target`\nResolution: decide\n";
    let runner_pin = Phase0PinnedFile {
        path: runner.to_str().unwrap().to_owned(),
        sha256: raw_sha256(&fs::read(&runner).unwrap()),
    };
    let tool_tree = Phase0PinnedTree {
        path: tools.to_str().unwrap().to_owned(),
        source_tree_sha256: source_tree_manifest(&tools).unwrap().source_tree_sha256,
    };
    let opam_switch_prefix = Phase0PinnedTree {
        path: tools.to_str().unwrap().to_owned(),
        source_tree_sha256: readonly_closure_digest(&tools).unwrap(),
    };
    let opam_switch_environment = BTreeMap::from([
        (
            "CAML_LD_LIBRARY_PATH".to_owned(),
            tools.to_str().unwrap().to_owned(),
        ),
        (
            "OCAMLTOP_INCLUDE_PATH".to_owned(),
            tools.to_str().unwrap().to_owned(),
        ),
        (
            "OCAML_TOPLEVEL_PATH".to_owned(),
            tools.to_str().unwrap().to_owned(),
        ),
        ("OPAMSWITCH".to_owned(), "fixture".to_owned()),
        (
            "OPAM_SWITCH_PREFIX".to_owned(),
            tools.to_str().unwrap().to_owned(),
        ),
    ]);
    let checkout = Phase0PinnedCheckoutTool {
        executable: runner_pin.clone(),
        source: tool_tree.clone(),
        revision: "fixture-revision".to_owned(),
    };
    let components = Phase0ComponentClosure {
        repository_root: tools.to_str().unwrap().to_owned(),
        kernel: runner_pin.clone(),
        controller: runner_pin.clone(),
        bridge: runner_pin.clone(),
        checker_broker: runner_pin.clone(),
        checker_runner: runner_pin.clone(),
        extractor: runner_pin.clone(),
        filespec_checker: runner_pin.clone(),
        prescribed_region_checker: runner_pin.clone(),
        production_extraction_runner: runner_pin.clone(),
        goal_binding_reporter: runner_pin.clone(),
        model_opacity_scanner: runner_pin.clone(),
        worker_fragment: runner_pin.clone(),
        seam_audit_fragment: runner_pin.clone(),
        correspondence_audit_fragment: runner_pin.clone(),
    };
    let unadapted_source_manifest = source_tree_manifest(&raw).unwrap();
    let genesis = Phase0Genesis {
        schema: PHASE0_GENESIS_SCHEMA.to_owned(),
        target_upload_sha256: raw_sha256(b"target-upload"),
        supporting_upload_sha256: None,
        source_partition_manifest: Phase0SourcePartitionManifest::target_only(
            &unadapted_source_manifest,
        ).unwrap(),
        unadapted_source_manifest,
        goal_base64: BASE64_STANDARD.encode(goal),
        goal_sha256: raw_sha256(goal),
        policy: Phase0Policy {
            max_phase0_handbacks: MAX_PHASE0_HANDBACKS,
            allow_same_model_lanes: true,
        },
        bindings: Phase0Bindings {
            worker: lane("worker-lane"),
            seam_repair: vec![lane("seam-lane")],
            source_correspondence: vec![lane("correspondence-lane")],
        },
        prompt_contract_version: 64,
        components,
        toolchain: Phase0ToolchainClosure {
            cargo: runner_pin.clone(),
            cargo_identity: "cargo fixture".to_owned(),
            rustc: runner_pin,
            rustc_identity: "rustc fixture".to_owned(),
            toolchain_root: tools.to_str().unwrap().to_owned(),
            charon_toolchain: opam_switch_prefix.clone(),
            charon: checkout.clone(),
            aeneas: checkout,
            dependency_cache: None,
            opam_switch_prefix,
            opam_switch_environment,
            opam_switch: "fixture".to_owned(),
            target_triple: "x86_64-unknown-linux-gnu".to_owned(),
            extraction_profile: "dev".to_owned(),
            closed_environment: BTreeMap::from([("LANG".to_owned(), "C.UTF-8".to_owned())]),
        },
        genesis_sha256: Sha256Digest::ZERO,
    }
    .seal()
    .unwrap();
    let identity = Phase0AdaptationLedger {
        schema: PHASE0_ADAPTATION_LEDGER_SCHEMA.to_owned(),
        generation: 0,
        unadapted_tree_sha256: genesis.unadapted_source_manifest.source_tree_sha256,
        adapted_tree_sha256: genesis.unadapted_source_manifest.source_tree_sha256,
        goal_sha256: genesis.goal_sha256,
        entries: Vec::new(),
        ledger_entries_root: Sha256Digest::ZERO,
        ledger_sha256: Sha256Digest::ZERO,
    }
    .seal()
    .unwrap();
    let context = Phase0ValidationContext {
        schema: PHASE0_CONTEXT_SCHEMA.to_owned(),
        unadapted_tree_path: raw.to_str().unwrap().to_owned(),
        candidate_tree_path: candidate.to_str().unwrap().to_owned(),
        goal_targets: vec!["goal:target".to_owned()],
        ledger_path: temp.path().join("ledger.json").to_str().unwrap().to_owned(),
        receipt_store_path: temp.path().join("receipts").to_str().unwrap().to_owned(),
        goal_binding_report_path: temp.path().join("binding.json").to_str().unwrap().to_owned(),
    };
    Fixture { _temp: temp, raw, candidate, runner, context, genesis, identity }
}

fn stage(name: &str, status: &str, stderr: &[u8]) -> Value {
    json!({
        "name": name,
        "tool": "phase0-checker",
        "status": status,
        "timed_out": false,
        "stdout_base64": "",
        "stdout_byte_length": 0,
        "stdout_sha256": raw_sha256(b""),
        "stdout_truncated": false,
        "stderr_base64": BASE64_STANDARD.encode(stderr),
        "stderr_byte_length": stderr.len(),
        "stderr_sha256": raw_sha256(stderr),
        "stderr_truncated": false
    })
}

fn receipt(
    fixture: &Fixture,
    generation: u64,
    ledger_root: Sha256Digest,
    candidate_root: Sha256Digest,
    kind: &str,
    reverted: Option<&str>,
    passed: bool,
    ordinal: u64,
) -> Value {
    receipt_with_failure_line(
        fixture, generation, ledger_root, candidate_root, kind, reverted,
        passed, ordinal, b"missing standalone Cargo.toml",
    )
}

fn receipt_with_failure_line(
    fixture: &Fixture,
    generation: u64,
    ledger_root: Sha256Digest,
    candidate_root: Sha256Digest,
    kind: &str,
    reverted: Option<&str>,
    passed: bool,
    ordinal: u64,
    failure_line: &[u8],
) -> Value {
    let stages = if kind == "ratification_replay" {
        vec![stage("second_clean_extraction", "passed", b""), stage("filespec_region_gates", "passed", b"")]
    } else if passed {
        vec![stage("generated_model_split", "passed", b"")]
    } else {
        vec![stage("standalone_package", "failed", failure_line)]
    };
    let input = json!({
        "phase0_genesis_sha256": fixture.genesis.genesis_sha256,
        "runner_path": fixture.genesis.components.checker_runner.path,
        "runner_sha256": fixture.genesis.components.checker_runner.sha256,
        "extractor_path": fixture.genesis.components.extractor.path,
        "extractor_sha256": fixture.genesis.components.extractor.sha256,
        "filespec_checker_path": fixture.genesis.components.filespec_checker.path,
        "filespec_checker_sha256": fixture.genesis.components.filespec_checker.sha256,
        "prescribed_region_checker_path": fixture.genesis.components.prescribed_region_checker.path,
        "prescribed_region_checker_sha256": fixture.genesis.components.prescribed_region_checker.sha256,
        "repository_root": fixture.genesis.components.repository_root,
        "cargo_path": fixture.genesis.toolchain.cargo.path,
        "cargo_sha256": fixture.genesis.toolchain.cargo.sha256,
        "rustc_path": fixture.genesis.toolchain.rustc.path,
        "rustc_sha256": fixture.genesis.toolchain.rustc.sha256,
        "toolchain_root": fixture.genesis.toolchain.toolchain_root,
        "charon_toolchain_root": fixture.genesis.toolchain.charon_toolchain.path,
        "charon_toolchain_tree_sha256": fixture.genesis.toolchain.charon_toolchain.source_tree_sha256,
        "charon_path": fixture.genesis.toolchain.charon.executable.path,
        "charon_sha256": fixture.genesis.toolchain.charon.executable.sha256,
        "charon_source_root": fixture.genesis.toolchain.charon.source.path,
        "charon_source_tree_sha256": fixture.genesis.toolchain.charon.source.source_tree_sha256,
        "aeneas_path": fixture.genesis.toolchain.aeneas.executable.path,
        "aeneas_sha256": fixture.genesis.toolchain.aeneas.executable.sha256,
        "aeneas_source_root": fixture.genesis.toolchain.aeneas.source.path,
        "aeneas_source_tree_sha256": fixture.genesis.toolchain.aeneas.source.source_tree_sha256,
        "dependency_cache_path": "",
        "dependency_cache_tree_sha256": raw_sha256(b""),
        "opam_switch_prefix": fixture.genesis.toolchain.opam_switch_prefix.path,
        "opam_switch_prefix_tree_sha256": fixture.genesis.toolchain.opam_switch_prefix.source_tree_sha256,
        "opam_switch_environment": fixture.genesis.toolchain.opam_switch_environment,
        "target_triple": fixture.genesis.toolchain.target_triple,
        "extraction_profile": fixture.genesis.toolchain.extraction_profile,
        "opam_switch": fixture.genesis.toolchain.opam_switch,
        "closed_environment": fixture.genesis.toolchain.closed_environment,
        "goal_sha256": fixture.genesis.goal_sha256,
        "ledger_generation": generation,
        "ledger_root_sha256": ledger_root,
        "candidate_tree_sha256": candidate_root,
        "check_kind": kind,
        "check_id": format!("check-{generation}-{kind}-{ordinal}"),
        "output_root": fixture._temp.path(),
        "run_id": format!("check-{generation}-{kind}-{ordinal}"),
        "reverted_entry_id": reverted,
        "mock_output": {
            "schema": "trellis-pv-phase0-checker-output/v1",
            "status": if passed { "passed" } else { "failed" },
            "phase0_genesis_sha256": fixture.genesis.genesis_sha256,
            "goal_sha256": fixture.genesis.goal_sha256,
            "ledger_generation": generation,
            "ledger_root_sha256": ledger_root,
            "candidate_tree_sha256": candidate_root,
            "check_kind": kind,
            "check_id": format!("check-{generation}-{kind}-{ordinal}"),
            "reverted_entry_id": reverted,
            "first_failing_stage": if passed { Value::Null } else { json!("standalone_package") },
            "model_bodiless_declarations": fixture_bodiless_scan(passed),
            "source_facts": {
                "candidate_tree_sha256": candidate_root,
                "compile_initial_tree_sha256": candidate_root,
                "compile_final_tree_sha256": if passed { json!(candidate_root) } else { Value::Null },
                "extraction_initial_tree_sha256": candidate_root,
                "extraction_final_tree_sha256": if passed { json!(candidate_root) } else { Value::Null },
            },
            "stages": stages,
            "a_b_generated_lean_roots": if kind == "ratification_replay" { json!([raw_sha256(b"lean"), raw_sha256(b"lean")]) } else { json!([]) },
            "a_b_tablet_roots": if kind == "ratification_replay" { json!([raw_sha256(b"tablet"), raw_sha256(b"tablet")]) } else { json!([]) },
            "generated_lean_root_sha256": if kind == "ratification_replay" { json!(raw_sha256(b"lean")) } else { Value::Null },
            "tablet_root_sha256": if kind == "ratification_replay" { json!(raw_sha256(b"tablet")) } else { Value::Null }
        }
    });
    let environment = BTreeMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())]);
    execute_phase0_checked_json(Phase0ExecutionRequest {
        phase0_genesis_sha256: fixture.genesis.genesis_sha256,
        purpose: "source_adaptation_checker",
        execution: PinnedExecutionRequest {
            runner_path: &fixture.runner,
            expected_runner_sha256: fixture.genesis.components.checker_runner.sha256,
            command_id: "phase0-protocol-fixture",
            working_directory: fixture._temp.path(),
            environment: &environment,
            input: &input,
            limits: ExecutionLimits {
                timeout: Duration::from_secs(5),
                max_stdin_bytes: 1024 * 1024,
                max_stdout_bytes: 1024 * 1024,
                max_stderr_bytes: 1024 * 1024,
            },
        },
    })
    .unwrap()
    .value()
    .clone()
}

/// The one declaration the fixture's model leaves without a definition, and
/// the scan the checker reports for it.
const FIXTURE_BODILESS_NAME: &str = "fixture.helper";

fn fixture_bodiless_declarations() -> Vec<Phase0BodilessDeclaration> {
    vec![Phase0BodilessDeclaration {
        name: FIXTURE_BODILESS_NAME.to_owned(),
        kind: BodilessDeclarationKind::Axiom,
        item_path: "fixture::helper".to_owned(),
    }]
}

/// The record the checker emits before any extraction has run, byte for byte
/// as `unscanned_model` builds it. Every run's opening check is this shape, so
/// the kernel must accept it or no generation can be applied at all.
#[test]
fn the_opening_checks_bodiless_scan_is_accepted() {
    let scan: Phase0ModelBodilessDeclarations = serde_json::from_value(json!({
        "status": "not_extracted",
        "read_from": "generated.lean",
        "detail": "no extraction produced a model in this check",
        "declarations": [],
    }))
    .expect("the checker's opening record parses");
    scan.validate().expect("the checker's opening record validates");
}

fn fixture_bodiless_scan(passed: bool) -> Value {
    if passed {
        json!({
            "status": "read",
            "read_from": "generated.lean",
            "detail": "",
            "declarations": [{
                "name": FIXTURE_BODILESS_NAME,
                "kind": "axiom",
                "item_path": "fixture::helper",
            }],
        })
    } else {
        json!({
            "status": "not_extracted",
            "read_from": "generated.lean",
            "detail": "no extraction produced a model in this check",
            "declarations": [],
        })
    }
}

fn draft_entry(discovery: Sha256Digest, after: &[u8]) -> Phase0AdaptationEntry {
    draft_entry_quoting(discovery, after, "missing standalone Cargo.toml", 0)
}

fn draft_entry_quoting(
    discovery: Sha256Digest,
    after: &[u8],
    quote: &str,
    byte_offset: u64,
) -> Phase0AdaptationEntry {
    Phase0AdaptationEntry {
        id: String::new(),
        file: "Cargo.toml".to_owned(),
        operation: Phase0FileOperation::Add,
        before_span: ByteSpan { start: 0, end: 0 },
        after_span: ByteSpan { start: 0, end: after.len() as u64 },
        before_sha256: None,
        after_sha256: Some(raw_sha256(after)),
        patch: ExactBytePatch {
            removed_base64: String::new(),
            inserted_base64: BASE64_STANDARD.encode(after),
            removed_sha256: Sha256Digest::ZERO,
            inserted_sha256: Sha256Digest::ZERO,
        },
        diff_sha256: Sha256Digest::ZERO,
        quoted_error: QuotedCheckerError {
            tool: "phase0-checker".to_owned(),
            receipt_sha256: discovery,
            stage_index: 0,
            stream: CheckerStream::Stderr,
            byte_offset,
            byte_length: quote.len() as u64,
            quote: quote.to_owned(),
            quote_sha256: raw_sha256(quote.as_bytes()),
        },
        discovery_failure_receipt_sha256: discovery,
        ablation_failure_receipt_sha256: Sha256Digest::ZERO,
        behavior_preservation_claim: "Adds only a package boundary around the unchanged source body.".to_owned(),
        seam_class: AdaptationSeamClass::PackagingShim,
        citation: AdaptationCitation::LanguageGuarantee {
            statement: "A library path selects the unchanged Rust source as the crate body.".to_owned(),
        },
        affected_targets: vec!["goal:target".to_owned()],
        meaning_change: false,
        status: Phase0LedgerStatus::Draft,
    }
}

fn submit_single_entry_revision(
    fixture: &Fixture,
    state: Phase0State,
    entry: Phase0AdaptationEntry,
    cargo: &[u8],
    snapshot_name: &str,
) -> Result<Phase0State, trellis_kernel::trust_base::TrustError> {
    let request = match state.next_request(&fixture.context).unwrap() {
        Phase0NextRequest::Worker(request) => request,
        other => panic!("unexpected request: {other:?}"),
    };
    let snapshot = fixture._temp.path().join(snapshot_name);
    fs::create_dir(&snapshot).unwrap();
    fs::copy(fixture.raw.join("lib.rs"), snapshot.join("lib.rs")).unwrap();
    fs::write(fixture.candidate.join("Cargo.toml"), cargo).unwrap();
    let ledger = Phase0AdaptationLedger {
        schema: PHASE0_ADAPTATION_LEDGER_SCHEMA.to_owned(),
        generation: state.generation + 1,
        unadapted_tree_sha256: fixture.genesis.unadapted_source_manifest.source_tree_sha256,
        adapted_tree_sha256: source_tree_manifest(&fixture.candidate).unwrap().source_tree_sha256,
        goal_sha256: fixture.genesis.goal_sha256,
        entries: vec![entry],
        ledger_entries_root: Sha256Digest::ZERO,
        ledger_sha256: Sha256Digest::ZERO,
    }
    .seal()
    .unwrap();
    let response = Phase0WorkerResponse {
        schema: PHASE0_WORKER_RESPONSE_SCHEMA.to_owned(),
        request_id: request.request_id,
        request_sha256: request.request_sha256,
        base_generation: state.generation,
        worker_thread_id: "guard-teeth-worker".to_owned(),
        candidate_manifest: source_tree_manifest(&fixture.candidate).unwrap(),
        ledger,
        discovery_receipts: Vec::new(),
        response_sha256: Sha256Digest::ZERO,
    }
    .seal()
    .unwrap();
    state.apply(
        &fixture.context,
        Phase0Event::WorkerRevision {
            response,
            pre_burst_candidate_path: snapshot.to_str().unwrap().to_owned(),
        },
    )
}

fn draft_overbroad_modify(
    discovery: Sha256Digest,
    before: &[u8],
    after: &[u8],
) -> Phase0AdaptationEntry {
    let quote = "missing standalone Cargo.toml";
    Phase0AdaptationEntry {
        id: String::new(),
        file: "lib.rs".to_owned(),
        operation: Phase0FileOperation::Modify,
        before_span: ByteSpan { start: before.len() as u64, end: before.len() as u64 },
        after_span: ByteSpan { start: before.len() as u64, end: after.len() as u64 },
        before_sha256: Some(raw_sha256(before)),
        after_sha256: Some(raw_sha256(after)),
        patch: ExactBytePatch {
            removed_base64: String::new(),
            inserted_base64: BASE64_STANDARD.encode(&after[before.len()..]),
            removed_sha256: Sha256Digest::ZERO,
            inserted_sha256: Sha256Digest::ZERO,
        },
        diff_sha256: Sha256Digest::ZERO,
        quoted_error: QuotedCheckerError {
            tool: "phase0-checker".to_owned(),
            receipt_sha256: discovery,
            stage_index: 0,
            stream: CheckerStream::Stderr,
            byte_offset: 0,
            byte_length: quote.len() as u64,
            quote: quote.to_owned(),
            quote_sha256: raw_sha256(quote.as_bytes()),
        },
        discovery_failure_receipt_sha256: discovery,
        ablation_failure_receipt_sha256: Sha256Digest::ZERO,
        behavior_preservation_claim: "This unrelated source comment is claimed to be useful.".to_owned(),
        seam_class: AdaptationSeamClass::SeamI,
        citation: AdaptationCitation::LanguageGuarantee {
            statement: "Comments do not change Rust execution.".to_owned(),
        },
        affected_targets: vec!["goal:target".to_owned()],
        meaning_change: false,
        status: Phase0LedgerStatus::Draft,
    }
}

fn audit_pass(request: &Phase0AuditRequest, entry_ids: &[String]) -> Phase0AuditResult {
    Phase0AuditResult {
        schema: PHASE0_AUDIT_RESULT_SCHEMA.to_owned(),
        request_id: request.request_id.clone(),
        request_sha256: request.request_sha256,
        scenario: request.scenario,
        generation: request.generation,
        genesis_sha256: request.genesis_sha256,
        semantic_bundle_sha256: request.semantic_bundle_sha256,
        ledger_sha256: request.ledger_sha256,
        lane_id: request.lane.lane_id.clone(),
        lane_binding_sha256: request.lane.binding_sha256,
        decision: Phase0AuditDecision::Pass,
        entry_verdicts: entry_ids.iter().map(|entry_id| Phase0EntryAuditVerdict {
                entry_id: entry_id.clone(),
                decision: Phase0AuditDecision::Pass,
                judgment: "The edit is necessary, minimal, and behavior preserving.".to_owned(),
            }).collect(),
        whole_tree_verdict: (request.scenario == Phase0AuditScenario::SourceCorrespondence)
            .then(|| Phase0WholeTreeVerdict {
                decision: Phase0AuditDecision::Pass,
                judgment: "The composed source and extracted target retain GOAL continuity.".to_owned(),
            }),
        goal_binding_verdicts: if request.scenario == Phase0AuditScenario::SourceCorrespondence {
            request.goal_binding_targets.iter().map(|target| Phase0GoalBindingAuditVerdict {
                target_id: target.target_id.clone(),
                origin: target.origin,
                goal_binding_outside_target: target.goal_binding_outside_target,
                goal_binding_in_adaptation: target.goal_binding_in_adaptation,
                decision: Phase0AuditDecision::Pass,
                judgment: "The bound target remains source-corresponding.".to_owned(),
            }).collect()
        } else { Vec::new() },
        findings: Vec::new(),
        result_sha256: Sha256Digest::ZERO,
    }
    .seal()
    .unwrap()
}

fn complete_checks(fixture: &Fixture, mut state: Phase0State, ordinal: u64) -> Phase0State {
    let generation = state.generation;
    let candidate_success = receipt(
        fixture, generation, state.ledger.ledger_entries_root,
        state.candidate_manifest.source_tree_sha256, "candidate", None, true, ordinal,
    );
    let final_success = receipt(
        fixture, generation, state.ledger.ledger_entries_root,
        state.candidate_manifest.source_tree_sha256, "ratification_replay", None, true, ordinal + 1,
    );
    let targets = BTreeSet::from(["goal:target".to_owned()]);
    let mut ablations = Vec::new();
    let mut completed = state.ledger.clone();
    for (index, entry) in state.ledger.entries.iter().enumerate() {
        let root = phase0_ablated_tree_digest(&fixture.raw, &state.ledger, &targets, &entry.id).unwrap();
        let ablation = receipt(
            fixture, generation, state.ledger.ledger_entries_root, root,
            "entry_ablation", Some(&entry.id), false, ordinal + 2 + index as u64,
        );
        completed.entries[index].ablation_failure_receipt_sha256 =
            ablation["receipt_sha256"].as_str().unwrap().parse().unwrap();
        completed.entries[index].status = Phase0LedgerStatus::Audited;
        ablations.push(ablation);
    }
    completed = completed.seal().unwrap();
    let goal_report = complete_goal_report(fixture, &state);
    state = state.apply(&fixture.context, Phase0Event::CheckComplete {
        candidate_success_receipt: candidate_success,
        final_success_receipt: final_success,
        ablation_receipts: ablations,
        completed_ledger: completed,
        goal_binding_report: goal_report,
        extracted_model_path: fixture._temp.path()
            .join(format!("check-{generation}-ratification_replay-{}", ordinal + 1))
            .join("extraction-writable/extracted")
            .to_str().unwrap().to_owned(),
    }).unwrap();
    state
}

fn complete_goal_report(fixture: &Fixture, state: &Phase0State) -> Phase0GoalBindingReport {
    Phase0GoalBindingReport {
        schema: PHASE0_GOAL_BINDING_REPORT_SCHEMA.to_owned(),
        goal_base64: fixture.genesis.goal_base64.clone(),
        goal_sha256: fixture.genesis.goal_sha256,
        adapted_tree_sha256: state.candidate_manifest.source_tree_sha256,
        model_bodiless_status: BodilessScanStatus::Read,
        model_bodiless_declarations: fixture_bodiless_declarations(),
        model_bodiless_scope: GOAL_BINDING_BODILESS_SCOPE.to_owned(),
        targets: vec![GoalBindingTarget {
            target_id: "goal:target".to_owned(),
            name: "target".to_owned(),
            resolution: "decide".to_owned(),
            heading_line: 1,
            status: GoalBindingStatus::Bound,
            matches: vec![GoalBindingMatch {
                adapted_item_path: "fixture::target".to_owned(),
                model_node: "Extracted.Target".to_owned(),
                source_sha256: raw_sha256(b"source"),
            }],
            origin: GoalBindingOrigin::Target,
            goal_binding_outside_target: false,
            goal_binding_in_adaptation: false,
            model_declaration_bodiless: false,
            model_bodiless_direct_calls: vec![FIXTURE_BODILESS_NAME.to_owned()],
            model_computation_suppressed: true,
        }],
        report_sha256: Sha256Digest::ZERO,
    }
    .seal()
    .unwrap()
}

#[test]
fn partition_digests_and_outside_target_binding_flags_are_fail_closed() {
    let fixture = fixture();
    let state = Phase0State::initialize(
        fixture.genesis.clone(), fixture.identity.clone(), &fixture.context,
    )
    .unwrap();

    let mut wrong_partition = fixture.genesis.source_partition_manifest.clone();
    wrong_partition.target_source_tree_sha256 = raw_sha256(b"wrong-target-partition");
    assert!(wrong_partition
        .validate(&fixture.genesis.unadapted_source_manifest)
        .is_err());

    let mut supporting = complete_goal_report(&fixture, &state);
    supporting.targets[0].origin = GoalBindingOrigin::Supporting;
    supporting.targets[0].goal_binding_outside_target = true;
    supporting = supporting.seal().unwrap();
    supporting.validate().unwrap();

    supporting.targets[0].goal_binding_outside_target = false;
    supporting = supporting.seal().unwrap();
    let error = supporting.validate().unwrap_err();
    assert!(error.detail.contains("origin flags"), "{error:?}");

    let mut adaptation = complete_goal_report(&fixture, &state);
    adaptation.targets[0].origin = GoalBindingOrigin::Adaptation;
    adaptation.targets[0].goal_binding_outside_target = true;
    adaptation.targets[0].goal_binding_in_adaptation = true;
    adaptation = adaptation.seal().unwrap();
    adaptation.validate().unwrap();
}

fn clean_sealed_fixture() -> (Fixture, Phase0SealedGeneration) {
    let fixture = fixture();
    let mut state = Phase0State::initialize(
        fixture.genesis.clone(),
        fixture.identity.clone(),
        &fixture.context,
    )
    .unwrap();
    let discovery = receipt(
        &fixture,
        0,
        state.ledger.ledger_entries_root,
        state.candidate_manifest.source_tree_sha256,
        "candidate",
        None,
        false,
        900,
    );
    let discovery_digest = discovery["receipt_sha256"].as_str().unwrap().parse().unwrap();
    state = state
        .apply(&fixture.context, Phase0Event::CheckerFailure { receipt: discovery })
        .unwrap();
    let request = match state.next_request(&fixture.context).unwrap() {
        Phase0NextRequest::Worker(request) => request,
        other => panic!("unexpected request: {other:?}"),
    };
    let before = fixture._temp.path().join("clean-before");
    fs::create_dir(&before).unwrap();
    fs::copy(fixture.raw.join("lib.rs"), before.join("lib.rs")).unwrap();
    let cargo = b"[package]\nname='fixture'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n";
    fs::write(fixture.candidate.join("Cargo.toml"), cargo).unwrap();
    let ledger = Phase0AdaptationLedger {
        schema: PHASE0_ADAPTATION_LEDGER_SCHEMA.to_owned(),
        generation: 1,
        unadapted_tree_sha256: fixture.genesis.unadapted_source_manifest.source_tree_sha256,
        adapted_tree_sha256: source_tree_manifest(&fixture.candidate).unwrap().source_tree_sha256,
        goal_sha256: fixture.genesis.goal_sha256,
        entries: vec![draft_entry(discovery_digest, cargo)],
        ledger_entries_root: Sha256Digest::ZERO,
        ledger_sha256: Sha256Digest::ZERO,
    }
    .seal()
    .unwrap();
    state = state
        .apply(
            &fixture.context,
            Phase0Event::WorkerRevision {
                response: Phase0WorkerResponse {
                    schema: PHASE0_WORKER_RESPONSE_SCHEMA.to_owned(),
                    request_id: request.request_id,
                    request_sha256: request.request_sha256,
                    base_generation: 0,
                    worker_thread_id: "clean-worker-thread".to_owned(),
                    candidate_manifest: source_tree_manifest(&fixture.candidate).unwrap(),
                    ledger,
                    discovery_receipts: Vec::new(),
                    response_sha256: Sha256Digest::ZERO,
                }
                .seal()
                .unwrap(),
                pre_burst_candidate_path: before.to_str().unwrap().to_owned(),
            },
        )
        .unwrap();
    state = complete_checks(&fixture, state, 910);
    let entry_ids = vec![state.ledger.entries[0].id.clone()];
    for _ in 0..2 {
        let request = match state.next_request(&fixture.context).unwrap() {
            Phase0NextRequest::Audit(request) => request,
            other => panic!("unexpected request: {other:?}"),
        };
        state = state
            .apply(
                &fixture.context,
                Phase0Event::AuditResult {
                    result: audit_pass(&request, &entry_ids),
                    request,
                },
            )
            .unwrap();
    }
    let (_, sealed) = state.finalize(&fixture.context).unwrap();
    (fixture, sealed)
}

fn write_production_campaign(
    fixture: &Fixture,
    sealed: &Phase0SealedGeneration,
) -> (PathBuf, Phase0ProductionResult) {
    let campaign = fixture._temp.path().join("campaign");
    fs::create_dir(&campaign).unwrap();
    copy_tree(&fixture.raw, &campaign.join("unadapted-crate"));
    copy_tree(&fixture.candidate, &campaign.join("crate"));
    fs::write(
        campaign.join("GOAL.md"),
        BASE64_STANDARD.decode(&fixture.genesis.goal_base64).unwrap(),
    )
    .unwrap();
    let phase0 = campaign.join(".trellis/phase0");
    let extraction = campaign.join("extraction-evidence");
    fs::create_dir_all(&phase0).unwrap();
    fs::create_dir(&extraction).unwrap();
    for (name, value) in [
        ("GENESIS.json", serde_json::to_value(&fixture.genesis).unwrap()),
        ("SEALED_GENERATION.json", serde_json::to_value(sealed).unwrap()),
        ("BUNDLE.json", serde_json::to_value(&sealed.semantic_bundle).unwrap()),
        ("ADAPTATION_LEDGER.json", serde_json::to_value(&sealed.semantic_bundle.ledger).unwrap()),
        ("GOAL_BINDING_REPORT.json", serde_json::to_value(&sealed.semantic_bundle.goal_binding_report).unwrap()),
        ("SOURCE_PARTITION_MANIFEST.json", serde_json::to_value(&sealed.semantic_bundle.source_partition_manifest).unwrap()),
    ] {
        fs::write(phase0.join(name), canonical_json_value(&value).unwrap()).unwrap();
    }
    let tool_root = fixture.runner.parent().unwrap();
    let executable_relative = fixture.runner.file_name().unwrap().to_str().unwrap();
    let steps: Vec<Value> = ["rustc-version", "cargo-metadata", "charon", "aeneas"]
        .into_iter()
        .map(|tool| json!({"tool": tool, "argv": [fixture.runner]}))
        .collect();
    let command = json!({
        "schema": "trellis-extraction-command/v1",
        "extractor_script_sha256": fixture.genesis.components.extractor.sha256,
        "extraction_provenance": {
            "extractor_toolchain_sha256": raw_sha256(b"toolchain"),
            "extraction_toolchain": {
                "target_triple": fixture.genesis.toolchain.target_triple,
                "extraction_profile": fixture.genesis.toolchain.extraction_profile,
                "rustc_vv": format!("{}\n", fixture.genesis.toolchain.rustc_identity),
            }
        },
        "kernel_gate_checkers": [
            {
                "tool":"kernel-scan-tablet-filespec",
                "path":fixture.genesis.components.filespec_checker.path,
                "sha256":fixture.genesis.components.filespec_checker.sha256,
            },
            {
                "tool":"kernel-print-prescribed-region",
                "path":fixture.genesis.components.prescribed_region_checker.path,
                "sha256":fixture.genesis.components.prescribed_region_checker.sha256,
            }
        ],
        "tool_source_checkouts": [
            {"tool":"charon", "repository_root":tool_root, "commit":fixture.genesis.toolchain.charon.revision, "origin":"fixture", "executable_relative_path":executable_relative, "clean_worktree":true},
            {"tool":"aeneas", "repository_root":tool_root, "commit":fixture.genesis.toolchain.aeneas.revision, "origin":"fixture", "executable_relative_path":executable_relative, "clean_worktree":true}
        ],
        "runs": [
            {"run_role":"primary", "steps":steps},
            {"run_role":"determinism_replay", "steps":[]}
        ]
    });
    let command_bytes = canonical_json_value(&command).unwrap();
    let result = json!({
        "schema":"trellis-extraction-result/v2",
        "command_sha256":raw_sha256(&command_bytes),
    });
    let result_bytes = canonical_json_value(&result).unwrap();
    let determinism = json!({
        "schema":"trellis-extraction-determinism/v2",
        "status":"passed",
        "result_sha256":raw_sha256(&result_bytes),
        "generated_lean_sha256":[raw_sha256(b"lean"), raw_sha256(b"lean")],
        "tablet_root_sha256":[raw_sha256(b"tablet"), raw_sha256(b"tablet")],
    });
    fs::write(extraction.join("COMMAND.json"), command_bytes).unwrap();
    fs::write(extraction.join("RESULT.json"), result_bytes).unwrap();
    fs::write(
        extraction.join("DETERMINISM.json"),
        canonical_json_value(&determinism).unwrap(),
    )
    .unwrap();
    let production = verify_phase0_production(&campaign, &fixture.genesis, sealed).unwrap();
    fs::write(
        phase0.join("PRODUCTION_RESULT.json"),
        canonical_json_value(&serde_json::to_value(&production).unwrap()).unwrap(),
    )
    .unwrap();
    (campaign, production)
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
    }
}

fn seam_findings(request: &Phase0AuditRequest, ids: &[String], broad_id: &str) -> Phase0AuditResult {
    Phase0AuditResult {
        schema: PHASE0_AUDIT_RESULT_SCHEMA.to_owned(),
        request_id: request.request_id.clone(),
        request_sha256: request.request_sha256,
        scenario: request.scenario,
        generation: request.generation,
        genesis_sha256: request.genesis_sha256,
        semantic_bundle_sha256: request.semantic_bundle_sha256,
        ledger_sha256: request.ledger_sha256,
        lane_id: request.lane.lane_id.clone(),
        lane_binding_sha256: request.lane.binding_sha256,
        decision: Phase0AuditDecision::Findings,
        entry_verdicts: ids.iter().map(|id| Phase0EntryAuditVerdict {
            entry_id: id.clone(),
            decision: if id == broad_id { Phase0AuditDecision::Findings } else { Phase0AuditDecision::Pass },
            judgment: if id == broad_id {
                "The unrelated edit is behavior-neutral but not necessary for the diagnostic."
            } else {
                "The package boundary is necessary and minimal."
            }.to_owned(),
        }).collect(),
        whole_tree_verdict: None,
        goal_binding_verdicts: Vec::new(),
        findings: vec![Phase0AuditFinding {
            code: "overbroad_patch".to_owned(),
            entry_id: Some(broad_id.to_owned()),
            path: Some("lib.rs".to_owned()),
            span_start: None,
            span_end: None,
            judgment: "The source comment is unrelated to the package-boundary failure.".to_owned(),
            required_revision: "Remove the unrelated source edit and retain the package shim.".to_owned(),
        }],
        result_sha256: Sha256Digest::ZERO,
    }.seal().unwrap()
}

#[test]
fn exact_generation_reaches_sealed_only_after_receipts_bindings_and_both_audits() {
    let fixture = fixture();
    let mut state = Phase0State::initialize(
        fixture.genesis.clone(), fixture.identity.clone(), &fixture.context,
    ).unwrap();
    let discovery = receipt(
        &fixture, 0, state.ledger.ledger_entries_root,
        state.candidate_manifest.source_tree_sha256, "candidate", None, false, 1,
    );
    let discovery_digest: Sha256Digest = discovery["receipt_sha256"].as_str().unwrap().parse().unwrap();
    state = state.apply(&fixture.context, Phase0Event::CheckerFailure { receipt: discovery }).unwrap();
    assert_eq!(state.stage, Phase0Stage::Worker);
    assert_eq!(state.phase0_handbacks, 0, "initial discovery is not a handback");

    let request = match state.next_request(&fixture.context).unwrap() {
        Phase0NextRequest::Worker(request) => request,
        other => panic!("unexpected request: {other:?}"),
    };
    assert_eq!(request.goal_base64, fixture.genesis.goal_base64);
    assert_eq!(request.goal_targets, fixture.context.goal_targets);
    assert_eq!(request.unadapted_tree_path, fixture.context.unadapted_tree_path);
    assert_eq!(request.response_contract["affected_targets_values"].as_str().unwrap().contains("goal_targets"), true);
    // The worker is told that the model's bodiless declarations are recorded,
    // which edit is preferred, and what an entry that removes a computation
    // from the model has to state.
    let opacity = request.response_contract["model_opacity_semantics"].as_str().unwrap();
    assert!(opacity.contains("without a body"), "{opacity}");
    assert!(opacity.contains("is preferred to"), "{opacity}");
    assert!(opacity.contains("behavior_preservation_claim"), "{opacity}");
    assert_eq!(request.ledger, state.ledger);
    assert_eq!(
        request.checker_failure_receipt.as_ref().map(|evidence| evidence.receipt_sha256),
        Some(discovery_digest),
    );
    let cargo = b"[package]\nname='fixture'\nversion='0.1.0'\nedition='2021'\n[lib]\npath='lib.rs'\n";
    let pre_burst = fixture._temp.path().join("pre-burst");
    fs::create_dir(&pre_burst).unwrap();
    fs::copy(fixture.raw.join("lib.rs"), pre_burst.join("lib.rs")).unwrap();
    fs::write(fixture.candidate.join("Cargo.toml"), cargo).unwrap();
    let source_before = fs::read(fixture.raw.join("lib.rs")).unwrap();
    let mut source_overbroad = source_before.clone();
    source_overbroad.extend_from_slice(b"// unrelated comment\n");
    fs::write(fixture.candidate.join("lib.rs"), &source_overbroad).unwrap();
    let ledger = Phase0AdaptationLedger {
        schema: PHASE0_ADAPTATION_LEDGER_SCHEMA.to_owned(),
        generation: 1,
        unadapted_tree_sha256: fixture.genesis.unadapted_source_manifest.source_tree_sha256,
        adapted_tree_sha256: source_tree_manifest(&fixture.candidate).unwrap().source_tree_sha256,
        goal_sha256: fixture.genesis.goal_sha256,
        entries: vec![
            draft_entry(discovery_digest, cargo),
            draft_overbroad_modify(discovery_digest, &source_before, &source_overbroad),
        ],
        ledger_entries_root: Sha256Digest::ZERO,
        ledger_sha256: Sha256Digest::ZERO,
    }.seal().unwrap();
    let response = Phase0WorkerResponse {
        schema: PHASE0_WORKER_RESPONSE_SCHEMA.to_owned(),
        request_id: request.request_id,
        request_sha256: request.request_sha256,
        base_generation: 0,
        worker_thread_id: "persistent-worker-thread".to_owned(),
        candidate_manifest: source_tree_manifest(&fixture.candidate).unwrap(),
        ledger,
        discovery_receipts: Vec::new(),
        response_sha256: Sha256Digest::ZERO,
    }.seal().unwrap();
    state = state.apply(&fixture.context, Phase0Event::WorkerRevision {
        response,
        pre_burst_candidate_path: pre_burst.to_str().unwrap().to_owned(),
    }).unwrap();
    state = complete_checks(&fixture, state, 2);
    assert_eq!(state.stage, Phase0Stage::SeamAudit);

    // The seam lane rejects the behavior-neutral but unnecessary source edit.
    let first_audit = match state.next_request(&fixture.context).unwrap() {
        Phase0NextRequest::Audit(request) => request,
        other => panic!("unexpected request: {other:?}"),
    };
    let mut old_ids: Vec<_> = state.ledger.entries.iter().map(|entry| entry.id.clone()).collect();
    old_ids.sort();
    let broad_id = state.ledger.entries.iter().find(|entry| entry.file == "lib.rs").unwrap().id.clone();
    let finding = seam_findings(&first_audit, &old_ids, &broad_id);
    state = state.apply(&fixture.context, Phase0Event::AuditResult {
        request: first_audit,
        result: finding,
    }).unwrap();
    assert_eq!(state.stage, Phase0Stage::Worker);
    assert_eq!(state.phase0_handbacks, 1);

    // The same persistent worker thread removes the overbroad row and submits
    // a new exact generation. The rejected generation remains in history.
    let worker_request = match state.next_request(&fixture.context).unwrap() {
        Phase0NextRequest::Worker(request) => request,
        other => panic!("unexpected request: {other:?}"),
    };
    let overbroad_snapshot = fixture._temp.path().join("pre-reduction");
    fs::create_dir(&overbroad_snapshot).unwrap();
    fs::copy(fixture.candidate.join("lib.rs"), overbroad_snapshot.join("lib.rs")).unwrap();
    fs::copy(fixture.candidate.join("Cargo.toml"), overbroad_snapshot.join("Cargo.toml")).unwrap();
    fs::write(fixture.candidate.join("lib.rs"), &source_before).unwrap();
    let reduced = Phase0AdaptationLedger {
        schema: PHASE0_ADAPTATION_LEDGER_SCHEMA.to_owned(),
        generation: 2,
        unadapted_tree_sha256: fixture.genesis.unadapted_source_manifest.source_tree_sha256,
        adapted_tree_sha256: source_tree_manifest(&fixture.candidate).unwrap().source_tree_sha256,
        goal_sha256: fixture.genesis.goal_sha256,
        entries: vec![draft_entry(discovery_digest, cargo)],
        ledger_entries_root: Sha256Digest::ZERO,
        ledger_sha256: Sha256Digest::ZERO,
    }.seal().unwrap();
    let response = Phase0WorkerResponse {
        schema: PHASE0_WORKER_RESPONSE_SCHEMA.to_owned(),
        request_id: worker_request.request_id,
        request_sha256: worker_request.request_sha256,
        base_generation: 1,
        worker_thread_id: "persistent-worker-thread".to_owned(),
        candidate_manifest: source_tree_manifest(&fixture.candidate).unwrap(),
        ledger: reduced,
        discovery_receipts: Vec::new(),
        response_sha256: Sha256Digest::ZERO,
    }.seal().unwrap();
    state = state.apply(&fixture.context, Phase0Event::WorkerRevision {
        response,
        pre_burst_candidate_path: overbroad_snapshot.to_str().unwrap().to_owned(),
    }).unwrap();
    assert_eq!(state.generation_history.len(), 2);
    assert_eq!(state.generation_history[1].terminal_findings[0].code, "overbroad_patch");
    state = complete_checks(&fixture, state, 20);
    let entry_id = state.ledger.entries[0].id.clone();

    for scenario in [Phase0AuditScenario::SeamRepair, Phase0AuditScenario::SourceCorrespondence] {
        let request = match state.next_request(&fixture.context).unwrap() {
            Phase0NextRequest::Audit(request) => request,
            other => panic!("unexpected request: {other:?}"),
        };
        assert_eq!(request.scenario, scenario);
        let missing = audit_pass(&request, &[]);
        assert!(state.clone().apply(
            &fixture.context,
            Phase0Event::AuditResult { request: request.clone(), result: missing },
        ).is_err());
        let mut wrong_lane = audit_pass(&request, std::slice::from_ref(&entry_id));
        wrong_lane.lane_binding_sha256 = raw_sha256(b"wrong-lane");
        wrong_lane = wrong_lane.seal().unwrap();
        assert!(state.clone().apply(
            &fixture.context,
            Phase0Event::AuditResult { request: request.clone(), result: wrong_lane },
        ).is_err());
        if scenario == Phase0AuditScenario::SourceCorrespondence {
            let mut missing_binding_answer =
                audit_pass(&request, std::slice::from_ref(&entry_id));
            missing_binding_answer.goal_binding_verdicts.clear();
            missing_binding_answer = missing_binding_answer.seal().unwrap();
            assert!(state.clone().apply(
                &fixture.context,
                Phase0Event::AuditResult {
                    request: request.clone(),
                    result: missing_binding_answer,
                },
            ).is_err());
        }
        let result = audit_pass(&request, std::slice::from_ref(&entry_id));
        state = state.apply(&fixture.context, Phase0Event::AuditResult { request, result }).unwrap();
    }
    assert!(matches!(
        state.next_request(&fixture.context).unwrap(),
        Phase0NextRequest::Seal(_)
    ));
    let (state, sealed) = state.finalize(&fixture.context).unwrap();
    assert_eq!(state.stage, Phase0Stage::Sealed);
    assert_eq!(sealed.generation, 2);
    assert_eq!(sealed.seam_audit_requests.len(), 1);
    assert_eq!(sealed.correspondence_audit_requests.len(), 1);
    sealed.validate(&fixture.genesis).unwrap();

    let mut wrong_omitted_root = sealed.clone();
    wrong_omitted_root.semantic_bundle.entry_receipts[0].ablation_omitted_tree_sha256 =
        raw_sha256(b"wrong-omitted-tree");
    wrong_omitted_root.semantic_bundle = wrong_omitted_root.semantic_bundle.seal().unwrap();
    wrong_omitted_root = wrong_omitted_root.seal().unwrap();
    let error = wrong_omitted_root.validate(&fixture.genesis).unwrap_err();
    assert!(
        error.detail.contains("bound failure"),
        "sealed validation did not recheck the omitted-tree root: {error:?}",
    );

    let mut wrong_lane = sealed.clone();
    wrong_lane.correspondence_audit_results[0].lane_binding_sha256 = raw_sha256(b"wrong");
    assert!(wrong_lane.validate(&fixture.genesis).is_err());
}

#[test]
fn bootstrap_projection_disclosure_package_and_mutation_surfaces_share_one_verifier() {
    let (fixture, sealed) = clean_sealed_fixture();
    let (campaign, production) = write_production_campaign(&fixture, &sealed);
    let roots = production.trust_roots();
    verify_phase0_repository(&campaign, &roots).unwrap();

    for wrong_roots in [
        Phase0TrustRoots {
            target_source_tree_sha256: raw_sha256(b"wrong-target-partition-root"),
            ..roots.clone()
        },
        Phase0TrustRoots {
            supporting_source_tree_sha256: raw_sha256(b"wrong-supporting-partition-root"),
            ..roots.clone()
        },
        Phase0TrustRoots {
            source_partition_manifest_sha256: raw_sha256(b"wrong-partition-manifest"),
            ..roots.clone()
        },
    ] {
        assert!(verify_phase0_repository(&campaign, &wrong_roots).is_err());
    }

    let command_path = campaign.join("extraction-evidence/COMMAND.json");
    let command_bytes = fs::read(&command_path).unwrap();
    let mut wrong_gate_checker: Value = serde_json::from_slice(&command_bytes).unwrap();
    wrong_gate_checker["kernel_gate_checkers"][0]["sha256"] =
        json!(raw_sha256(b"wrong-gate-checker"));
    fs::write(
        &command_path,
        canonical_json_value(&wrong_gate_checker).unwrap(),
    )
    .unwrap();
    let error = verify_phase0_production(&campaign, &fixture.genesis, &sealed).unwrap_err();
    assert!(
        error.detail.contains("Genesis pin"),
        "production gate checker digest was not compared with Genesis: {error:?}",
    );
    fs::write(&command_path, command_bytes).unwrap();

    let determinism_path = campaign.join("extraction-evidence/DETERMINISM.json");
    let determinism_bytes = fs::read(&determinism_path).unwrap();
    let mut wrong_checker_facts: Value = serde_json::from_slice(&determinism_bytes).unwrap();
    wrong_checker_facts["generated_lean_sha256"] = json!([
        raw_sha256(b"wrong-generated"),
        raw_sha256(b"wrong-generated")
    ]);
    fs::write(
        &determinism_path,
        canonical_json_value(&wrong_checker_facts).unwrap(),
    )
    .unwrap();
    let error = verify_phase0_production(&campaign, &fixture.genesis, &sealed).unwrap_err();
    assert!(
        error.detail.contains("final checker facts"),
        "production roots were not compared with the final checker: {error:?}",
    );
    fs::write(&determinism_path, determinism_bytes).unwrap();

    let section = phase0_advance_gate_section(&campaign, &roots).unwrap();
    assert_eq!(section["section"], "phase0_frozen_source_adaptation");
    assert_eq!(section["source_partitions"]["target_files"], json!(["lib.rs"]));
    assert_eq!(section["source_partitions"]["supporting_files"], json!([]));
    assert_eq!(section["source_partitions"]["shim_items"][0]["file"], "Cargo.toml");
    assert_eq!(section["source_partitions"]["goal_target_origins"][0]["origin"], "target");
    assert_eq!(section["ledger"]["entries"].as_array().unwrap().len(), 1);
    assert_eq!(
        section["ledger"]["entries"][0]["exact_patch_diff"]["inserted_base64"],
        sealed.semantic_bundle.ledger.entries[0].patch.inserted_base64,
    );
    assert_eq!(
        section["ledger"]["entries"][0]["unified_diff"]["rendered"],
        sealed.seam_audit_requests[0].unified_diffs[0].rendered,
    );
    assert!(
        section["model_opacity"]["summary"]
            .as_str()
            .unwrap()
            .contains("without a definition"),
        "the gate section does not say what the model left open: {}",
        section["model_opacity"],
    );
    let artifacts = phase0_package_artifacts(&campaign, &roots).unwrap();
    let roles: BTreeSet<_> = artifacts.iter().map(|artifact| artifact.role.as_str()).collect();
    for required in [
        "phase0_model_opacity",
        "phase0_sealed_generation",
        "phase0_checker_receipt",
        "phase0_audit_verdict",
        "phase0_unadapted_source_snapshot",
        "phase0_adapted_source_snapshot",
        "phase0_goal",
        "phase0_goal_binding_report",
        "phase0_source_partition_manifest",
    ] {
        assert!(roles.contains(required), "missing package role {required}");
    }

    let plan = CampaignTrustPlan {
        schema: CAMPAIGN_TRUST_PLAN_SCHEMA_V4.to_owned(),
        adaptation_ledger: phase0_campaign_ledger_projection(&sealed),
        targets: vec![TargetTrustPlan {
            target_id: "goal:target".to_owned(),
            resolution: trellis_kernel::ChallengeResolution::Decide,
        }],
    };
    verify_phase0_trust_plan(&plan, &sealed).unwrap();
    let mut wrong_plan = CampaignTrustPlan {
        schema: CAMPAIGN_TRUST_PLAN_SCHEMA_V4.to_owned(),
        adaptation_ledger: phase0_campaign_ledger_projection(&sealed),
        targets: vec![TargetTrustPlan {
            target_id: "goal:target".to_owned(),
            resolution: trellis_kernel::ChallengeResolution::Decide,
        }],
    };
    wrong_plan.adaptation_ledger[0].paths[0].after_sha256 = raw_sha256(b"wrong-plan-row");
    assert!(verify_phase0_trust_plan(&wrong_plan, &sealed).is_err());

    let phase0 = campaign.join(".trellis/phase0");
    let mutations = [
        campaign.join("unadapted-crate/lib.rs"),
        campaign.join("crate/lib.rs"),
        campaign.join("GOAL.md"),
        phase0.join("ADAPTATION_LEDGER.json"),
        phase0.join("GOAL_BINDING_REPORT.json"),
        phase0.join("SOURCE_PARTITION_MANIFEST.json"),
        phase0.join("PRODUCTION_RESULT.json"),
    ];
    for path in mutations {
        let original = fs::read(&path).unwrap();
        let mut changed = original.clone();
        changed.push(b'!');
        fs::write(&path, changed).unwrap();
        assert!(
            verify_phase0_repository(&campaign, &roots).is_err(),
            "mutation of {} was accepted",
            path.display()
        );
        fs::write(path, original).unwrap();
    }
    let seal_path = phase0.join("SEALED_GENERATION.json");
    let seal_bytes = fs::read(&seal_path).unwrap();
    for pointer in [
        "/semantic_bundle/checker_receipts/0/receipt/exit_code",
        "/seam_audit_results/0/entry_verdicts/0/judgment",
    ] {
        let mut value: Value = serde_json::from_slice(&seal_bytes).unwrap();
        *value.pointer_mut(pointer).unwrap() = json!("mutated");
        fs::write(&seal_path, canonical_json_value(&value).unwrap()).unwrap();
        assert!(verify_phase0_repository(&campaign, &roots).is_err());
        fs::write(&seal_path, &seal_bytes).unwrap();
    }

    let evidence = fixture._temp.path().join("phase0-evidence-mutation");
    fs::create_dir(&evidence).unwrap();
    let leaf = evidence.join("copied-binding.json");
    fs::write(&leaf, fs::read(phase0.join("GOAL_BINDING_REPORT.json")).unwrap()).unwrap();
    let input = EvidenceInput {
        kind: "phase0_goal_binding".to_owned(),
        logical_id: "phase0-goal-binding-report".to_owned(),
        relative_path: "copied-binding.json".to_owned(),
        dependency_ids: Vec::new(),
    };
    let (_, manifest) = build_evidence_manifest(&evidence, &[input]).unwrap();
    verify_evidence_tool_manifest(&evidence, &manifest).unwrap();
    fs::write(&leaf, b"mutated copied evidence leaf").unwrap();
    assert!(verify_evidence_tool_manifest(&evidence, &manifest).is_err());
}

/// A model that leaves a computation without a definition is recorded with the
/// other check facts and shown where a person decides — not merely implied by
/// an entry's prose.
#[test]
fn bodiless_model_declarations_are_recorded_and_shown_at_the_gate() {
    let (fixture, sealed) = clean_sealed_fixture();
    let bundle = &sealed.semantic_bundle;
    assert_eq!(
        bundle.model_bodiless_declarations.status,
        BodilessScanStatus::Read
    );
    assert_eq!(
        bundle.model_bodiless_declarations.declarations,
        fixture_bodiless_declarations()
    );
    assert_eq!(
        bundle.goal_binding_report.model_bodiless_scope,
        GOAL_BINDING_BODILESS_SCOPE
    );
    assert_eq!(
        bundle.goal_binding_report.targets[0].model_bodiless_direct_calls,
        vec![FIXTURE_BODILESS_NAME.to_owned()]
    );
    assert!(bundle.goal_binding_report.targets[0].model_computation_suppressed);

    let opacity = phase0_model_opacity_section(bundle);
    let summary = opacity["summary"].as_str().unwrap();
    assert!(summary.contains(FIXTURE_BODILESS_NAME), "{summary}");
    assert!(summary.contains("goal:target"), "{summary}");
    assert!(summary.contains("Direct calls only"), "{summary}");
    assert_eq!(opacity["scope"], GOAL_BINDING_BODILESS_SCOPE);
    assert_eq!(
        opacity["affected_goal_targets"][0]["target_id"],
        "goal:target"
    );

    // A model with nothing left open says so in the same place.
    let mut clean = bundle.clone();
    clean.model_bodiless_declarations.declarations.clear();
    for target in &mut clean.goal_binding_report.targets {
        target.model_bodiless_direct_calls.clear();
        target.model_computation_suppressed = false;
    }
    let clean_summary = phase0_model_opacity_section(&clean);
    assert!(
        clean_summary["summary"]
            .as_str()
            .unwrap()
            .contains("no computation"),
        "{clean_summary}"
    );
    drop(fixture);
}

/// The recorded facts are the final checker's own scan of its own model, and
/// the reporter cannot substitute a different account of it.
#[test]
fn bodiless_facts_must_be_the_final_checkers_own_scan() {
    let fixture = fixture();
    let mut state = Phase0State::initialize(
        fixture.genesis.clone(),
        fixture.identity.clone(),
        &fixture.context,
    )
    .unwrap();
    let discovery = receipt(
        &fixture,
        0,
        state.ledger.ledger_entries_root,
        state.candidate_manifest.source_tree_sha256,
        "candidate",
        None,
        false,
        1,
    );
    let discovery_digest = discovery["receipt_sha256"].as_str().unwrap().parse().unwrap();
    state = state
        .apply(&fixture.context, Phase0Event::CheckerFailure { receipt: discovery })
        .unwrap();
    let cargo = b"[package]\nname='fixture'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n";
    state = submit_single_entry_revision(
        &fixture,
        state,
        draft_entry(discovery_digest, cargo),
        cargo,
        "snapshot-bodiless",
    )
    .unwrap();

    let generation = state.generation;
    let candidate_success = receipt(
        &fixture, generation, state.ledger.ledger_entries_root,
        state.candidate_manifest.source_tree_sha256, "candidate", None, true, 2,
    );
    let final_success = receipt(
        &fixture, generation, state.ledger.ledger_entries_root,
        state.candidate_manifest.source_tree_sha256, "ratification_replay", None, true, 3,
    );
    let targets = BTreeSet::from(["goal:target".to_owned()]);
    let mut completed = state.ledger.clone();
    let entry_id = state.ledger.entries[0].id.clone();
    let root =
        phase0_ablated_tree_digest(&fixture.raw, &state.ledger, &targets, &entry_id).unwrap();
    let ablation = receipt(
        &fixture, generation, state.ledger.ledger_entries_root, root,
        "entry_ablation", Some(&entry_id), false, 4,
    );
    completed.entries[0].ablation_failure_receipt_sha256 =
        ablation["receipt_sha256"].as_str().unwrap().parse().unwrap();
    completed.entries[0].status = Phase0LedgerStatus::Audited;
    let completed = completed.seal().unwrap();

    // A report that leaves the suppression out of its own account of the model.
    let mut understated = complete_goal_report(&fixture, &state);
    understated.model_bodiless_declarations.clear();
    understated.targets[0].model_bodiless_direct_calls.clear();
    understated.targets[0].model_computation_suppressed = false;
    let understated = understated.seal().unwrap();
    understated.validate().unwrap();
    let error = state
        .clone()
        .apply(
            &fixture.context,
            Phase0Event::CheckComplete {
                candidate_success_receipt: candidate_success.clone(),
                final_success_receipt: final_success.clone(),
                ablation_receipts: vec![ablation.clone()],
                completed_ledger: completed.clone(),
                goal_binding_report: understated,
                extracted_model_path: fixture._temp.path()
                    .join(format!("check-{generation}-ratification_replay-3"))
                    .join("extraction-writable/extracted")
                    .to_str().unwrap().to_owned(),
            },
        )
        .unwrap_err();
    assert!(
        error.detail.contains("bodiless declarations"),
        "reporter was allowed its own account of the model: {error:?}",
    );

    // A direct call the model's own scan never listed is not a fact.
    let mut invented = complete_goal_report(&fixture, &state);
    invented.targets[0].model_bodiless_direct_calls = vec!["fixture.invented".to_owned()];
    let invented = invented.seal().unwrap();
    let error = invented.validate().unwrap_err();
    assert!(error.detail.contains("direct bodiless calls"), "{error:?}");

    // The derived "suppressed" flag cannot disagree with what it summarizes.
    let mut inconsistent = complete_goal_report(&fixture, &state);
    inconsistent.targets[0].model_computation_suppressed = false;
    let inconsistent = inconsistent.seal().unwrap();
    let error = inconsistent.validate().unwrap_err();
    assert!(error.detail.contains("opacity facts"), "{error:?}");

    // A scan that says it read nothing cannot also list declarations.
    let mut impossible = complete_goal_report(&fixture, &state);
    impossible.model_bodiless_status = BodilessScanStatus::NotExtracted;
    let impossible = impossible.seal().unwrap();
    let error = impossible.validate().unwrap_err();
    assert!(error.detail.contains("unread model"), "{error:?}");
}

#[test]
fn ablation_substring_without_exact_diagnostic_line_is_a_semantic_handback() {
    let fixture = fixture();
    let mut state = Phase0State::initialize(
        fixture.genesis.clone(), fixture.identity.clone(), &fixture.context,
    ).unwrap();
    let initial = receipt(
        &fixture, 0, state.ledger.ledger_entries_root,
        state.candidate_manifest.source_tree_sha256, "candidate", None, false, 700,
    );
    let discovery: Sha256Digest = initial["receipt_sha256"].as_str().unwrap().parse().unwrap();
    state = state.apply(&fixture.context, Phase0Event::CheckerFailure { receipt: initial }).unwrap();
    let request = match state.next_request(&fixture.context).unwrap() {
        Phase0NextRequest::Worker(request) => request,
        other => panic!("unexpected request: {other:?}"),
    };
    let snapshot = fixture._temp.path().join("ablation-pre-burst");
    fs::create_dir(&snapshot).unwrap();
    fs::copy(fixture.raw.join("lib.rs"), snapshot.join("lib.rs")).unwrap();
    let cargo = b"[package]\nname='fixture'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n";
    fs::write(fixture.candidate.join("Cargo.toml"), cargo).unwrap();
    let ledger = Phase0AdaptationLedger {
        schema: PHASE0_ADAPTATION_LEDGER_SCHEMA.to_owned(),
        generation: 1,
        unadapted_tree_sha256: fixture.genesis.unadapted_source_manifest.source_tree_sha256,
        adapted_tree_sha256: source_tree_manifest(&fixture.candidate).unwrap().source_tree_sha256,
        goal_sha256: fixture.genesis.goal_sha256,
        entries: vec![draft_entry(discovery, cargo)],
        ledger_entries_root: Sha256Digest::ZERO,
        ledger_sha256: Sha256Digest::ZERO,
    }.seal().unwrap();
    let response = Phase0WorkerResponse {
        schema: PHASE0_WORKER_RESPONSE_SCHEMA.to_owned(),
        request_id: request.request_id,
        request_sha256: request.request_sha256,
        base_generation: 0,
        worker_thread_id: "ablation-thread".to_owned(),
        candidate_manifest: source_tree_manifest(&fixture.candidate).unwrap(),
        ledger,
        discovery_receipts: Vec::new(),
        response_sha256: Sha256Digest::ZERO,
    }.seal().unwrap();
    state = state.apply(&fixture.context, Phase0Event::WorkerRevision {
        response,
        pre_burst_candidate_path: snapshot.to_str().unwrap().to_owned(),
    }).unwrap();
    let candidate = receipt(
        &fixture, 1, state.ledger.ledger_entries_root,
        state.candidate_manifest.source_tree_sha256, "candidate", None, true, 701,
    );
    let final_receipt = receipt(
        &fixture, 1, state.ledger.ledger_entries_root,
        state.candidate_manifest.source_tree_sha256, "ratification_replay", None, true, 702,
    );
    let entry_id = state.ledger.entries[0].id.clone();
    let omitted = phase0_ablated_tree_digest(
        &fixture.raw,
        &state.ledger,
        &BTreeSet::from(["goal:target".to_owned()]),
        &entry_id,
    ).unwrap();
    let ablation = receipt_with_failure_line(
        &fixture, 1, state.ledger.ledger_entries_root,
        omitted, "entry_ablation", Some(&entry_id), false, 703,
        b"missing standalone Cargo.toml plus unrelated suffix",
    );
    let mut completed = state.ledger.clone();
    completed.entries[0].ablation_failure_receipt_sha256 =
        ablation["receipt_sha256"].as_str().unwrap().parse().unwrap();
    completed.entries[0].status = Phase0LedgerStatus::Audited;
    completed = completed.seal().unwrap();
    let goal_report = complete_goal_report(&fixture, &state);
    state = state.apply(&fixture.context, Phase0Event::CheckComplete {
        candidate_success_receipt: candidate,
        final_success_receipt: final_receipt,
        ablation_receipts: vec![ablation],
        completed_ledger: completed,
        goal_binding_report: goal_report,
        extracted_model_path: fixture._temp.path()
            .join("check-1-ratification_replay-702/extraction-writable/extracted")
            .to_str().unwrap().to_owned(),
    }).unwrap();
    assert_eq!(state.stage, Phase0Stage::Worker);
    assert_eq!(state.phase0_handbacks, 1);
    assert_eq!(state.ledger.entries[0].status, Phase0LedgerStatus::Draft);
    assert_eq!(state.pending_findings[0].code, "ablation_did_not_reproduce_diagnostic");
    let worker = match state.next_request(&fixture.context).unwrap() {
        Phase0NextRequest::Worker(request) => request,
        other => panic!("unexpected request: {other:?}"),
    };
    assert_eq!(worker.prior_findings, state.pending_findings);
    assert!(worker.goal_binding_report.as_ref().is_some_and(Phase0GoalBindingReport::is_complete));
}

/// The byte offset at which `quote` starts as a whole line of `stderr`.
fn quote_offset(stderr: &[u8], quote: &str) -> u64 {
    let mut cursor = 0_u64;
    for line in stderr.split(|byte| *byte == b'\n') {
        if line == quote.as_bytes() {
            return cursor;
        }
        cursor += line.len() as u64 + 1;
    }
    panic!("the fixture stream lacks the quoted line");
}

/// One revision whose single entry quotes `quote` out of a discovery run that
/// printed `discovery_stderr`, checked out with an ablation run that printed
/// `ablation_stderr`. Returns the state after `check_complete`, which either
/// carries the entry into the seam audit or hands the revision back.
fn ablation_outcome(
    fixture: &Fixture,
    discovery_stderr: &[u8],
    quote: &str,
    ablation_stderr: &[u8],
) -> Phase0State {
    let mut state = Phase0State::initialize(
        fixture.genesis.clone(), fixture.identity.clone(), &fixture.context,
    ).unwrap();
    let initial = receipt_with_failure_line(
        fixture, 0, state.ledger.ledger_entries_root,
        state.candidate_manifest.source_tree_sha256, "candidate", None, false, 900,
        discovery_stderr,
    );
    let discovery: Sha256Digest = initial["receipt_sha256"].as_str().unwrap().parse().unwrap();
    state = state.apply(&fixture.context, Phase0Event::CheckerFailure { receipt: initial }).unwrap();
    let request = match state.next_request(&fixture.context).unwrap() {
        Phase0NextRequest::Worker(request) => request,
        other => panic!("unexpected request: {other:?}"),
    };
    let snapshot = fixture._temp.path().join("ablation-identity-pre-burst");
    fs::create_dir(&snapshot).unwrap();
    fs::copy(fixture.raw.join("lib.rs"), snapshot.join("lib.rs")).unwrap();
    let cargo = b"[package]\nname='fixture'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n";
    fs::write(fixture.candidate.join("Cargo.toml"), cargo).unwrap();
    let ledger = Phase0AdaptationLedger {
        schema: PHASE0_ADAPTATION_LEDGER_SCHEMA.to_owned(),
        generation: 1,
        unadapted_tree_sha256: fixture.genesis.unadapted_source_manifest.source_tree_sha256,
        adapted_tree_sha256: source_tree_manifest(&fixture.candidate).unwrap().source_tree_sha256,
        goal_sha256: fixture.genesis.goal_sha256,
        entries: vec![draft_entry_quoting(
            discovery, cargo, quote, quote_offset(discovery_stderr, quote),
        )],
        ledger_entries_root: Sha256Digest::ZERO,
        ledger_sha256: Sha256Digest::ZERO,
    }.seal().unwrap();
    let response = Phase0WorkerResponse {
        schema: PHASE0_WORKER_RESPONSE_SCHEMA.to_owned(),
        request_id: request.request_id,
        request_sha256: request.request_sha256,
        base_generation: 0,
        worker_thread_id: "ablation-identity-thread".to_owned(),
        candidate_manifest: source_tree_manifest(&fixture.candidate).unwrap(),
        ledger,
        discovery_receipts: Vec::new(),
        response_sha256: Sha256Digest::ZERO,
    }.seal().unwrap();
    state = state.apply(&fixture.context, Phase0Event::WorkerRevision {
        response,
        pre_burst_candidate_path: snapshot.to_str().unwrap().to_owned(),
    }).unwrap();
    let candidate = receipt(
        &fixture, 1, state.ledger.ledger_entries_root,
        state.candidate_manifest.source_tree_sha256, "candidate", None, true, 901,
    );
    let final_receipt = receipt(
        &fixture, 1, state.ledger.ledger_entries_root,
        state.candidate_manifest.source_tree_sha256, "ratification_replay", None, true, 902,
    );
    let entry_id = state.ledger.entries[0].id.clone();
    let omitted = phase0_ablated_tree_digest(
        &fixture.raw,
        &state.ledger,
        &BTreeSet::from(["goal:target".to_owned()]),
        &entry_id,
    ).unwrap();
    let ablation = receipt_with_failure_line(
        fixture, 1, state.ledger.ledger_entries_root,
        omitted, "entry_ablation", Some(&entry_id), false, 903,
        ablation_stderr,
    );
    let mut completed = state.ledger.clone();
    completed.entries[0].ablation_failure_receipt_sha256 =
        ablation["receipt_sha256"].as_str().unwrap().parse().unwrap();
    completed.entries[0].status = Phase0LedgerStatus::Audited;
    completed = completed.seal().unwrap();
    let goal_report = complete_goal_report(fixture, &state);
    state.apply(&fixture.context, Phase0Event::CheckComplete {
        candidate_success_receipt: candidate,
        final_success_receipt: final_receipt,
        ablation_receipts: vec![ablation],
        completed_ledger: completed,
        goal_binding_report: goal_report,
        extracted_model_path: fixture._temp.path()
            .join("check-1-ratification_replay-902/extraction-writable/extracted")
            .to_str().unwrap().to_owned(),
    }).unwrap()
}

fn assert_reproduced(state: &Phase0State) {
    assert_eq!(state.stage, Phase0Stage::SeamAudit);
    assert_eq!(state.phase0_handbacks, 0);
    assert_eq!(state.ledger.entries[0].status, Phase0LedgerStatus::Audited);
}

fn assert_not_reproduced(state: &Phase0State) {
    assert_eq!(state.stage, Phase0Stage::Worker);
    assert_eq!(state.phase0_handbacks, 1);
    assert_eq!(state.ledger.entries[0].status, Phase0LedgerStatus::Draft);
    assert_eq!(state.pending_findings[0].code, "ablation_did_not_reproduce_diagnostic");
}

const MESSAGE_QUOTE: &str = "error[E0001]: the package manifest is absent";

#[test]
fn ablation_of_the_same_message_for_another_file_does_not_reproduce_the_diagnostic() {
    let fixture = fixture();
    let state = ablation_outcome(
        &fixture,
        b"error[E0001]: the package manifest is absent\n  --> module_one.rs:12:1\n",
        MESSAGE_QUOTE,
        b"error[E0001]: the package manifest is absent\n  --> module_two.rs:12:1\n",
    );
    assert_not_reproduced(&state);
}

#[test]
fn ablation_reproduces_the_quoted_message_moved_within_its_own_file() {
    let fixture = fixture();
    let state = ablation_outcome(
        &fixture,
        b"error[E0001]: the package manifest is absent\n  --> module_one.rs:12:1\n",
        MESSAGE_QUOTE,
        b"error[E0001]: the package manifest is absent\n  --> module_one.rs:31:7\n",
    );
    assert_reproduced(&state);
}

#[test]
fn a_quoted_location_line_reproduces_at_another_line_of_the_same_file_only() {
    let moved = fixture();
    let state = ablation_outcome(
        &moved,
        b"[Error] The packaged sources do not compose\nSource: 'module_one.rs', lines 229:4-231:5\n",
        "Source: 'module_one.rs', lines 229:4-231:5",
        b"[Error] The packaged sources do not compose\nSource: 'module_one.rs', lines 231:4-233:5\n",
    );
    assert_reproduced(&state);

    let elsewhere = fixture();
    let state = ablation_outcome(
        &elsewhere,
        b"[Error] The packaged sources do not compose\nSource: 'module_one.rs', lines 229:4-231:5\n",
        "Source: 'module_one.rs', lines 229:4-231:5",
        b"[Error] The packaged sources do not compose\nSource: 'module_two.rs', lines 229:4-231:5\n",
    );
    assert_not_reproduced(&state);
}

#[test]
fn a_discovery_quote_with_no_location_line_reproduces_on_its_text_alone() {
    let fixture = fixture();
    let state = ablation_outcome(
        &fixture,
        b"error[E0001]: the package manifest is absent\nthe candidate package layout was refused\n",
        MESSAGE_QUOTE,
        b"a preamble the discovery run never printed\nerror[E0001]: the package manifest is absent\n",
    );
    assert_reproduced(&state);
}

#[test]
fn unreadable_discovery_location_lines_fall_back_to_the_quoted_text_alone() {
    for tail in [
        b"Source: 'an unterminated file name\n".to_vec(),
        b"  -->\n".to_vec(),
        b"  --> module_one.rs\n".to_vec(),
        b"  --> :\n".to_vec(),
    ] {
        let fixture = fixture();
        let mut discovery = b"error[E0001]: the package manifest is absent\n".to_vec();
        discovery.extend_from_slice(&tail);
        let state = ablation_outcome(
            &fixture,
            &discovery,
            MESSAGE_QUOTE,
            b"error[E0001]: the package manifest is absent\n  --> module_two.rs:12:1\n",
        );
        assert_reproduced(&state);
    }
}

#[test]
fn discovery_binding_rejects_stale_stage_and_partial_line_quotes() {
    let cargo = b"[package]\nname='fixture'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n";
    for (case, mutate) in [
        ("wrong-stage", 0_u8),
        ("partial-line", 1_u8),
    ] {
        let fixture = fixture();
        let mut state = Phase0State::initialize(
            fixture.genesis.clone(), fixture.identity.clone(), &fixture.context,
        ).unwrap();
        let failed = receipt(
            &fixture, 0, state.ledger.ledger_entries_root,
            state.candidate_manifest.source_tree_sha256, "candidate", None, false, 880,
        );
        let discovery = failed["receipt_sha256"].as_str().unwrap().parse().unwrap();
        state = state.apply(
            &fixture.context, Phase0Event::CheckerFailure { receipt: failed },
        ).unwrap();
        let mut entry = draft_entry(discovery, cargo);
        if mutate == 0 {
            entry.quoted_error.stage_index = 1;
        } else {
            let quote = "standalone Cargo.toml";
            entry.quoted_error.byte_offset = 8;
            entry.quoted_error.byte_length = quote.len() as u64;
            entry.quoted_error.quote = quote.to_owned();
            entry.quoted_error.quote_sha256 = raw_sha256(quote.as_bytes());
        }
        let error = submit_single_entry_revision(&fixture, state, entry, cargo, case)
            .unwrap_err();
        assert!(
            error.detail.contains("quoted error"),
            "{case} was not rejected by exact diagnostic-line validation: {error:?}",
        );
    }
}

#[test]
fn new_edit_must_cite_the_latest_authoritative_candidate_failure() {
    let fixture = fixture();
    let mut state = Phase0State::initialize(
        fixture.genesis.clone(), fixture.identity.clone(), &fixture.context,
    ).unwrap();
    let first = receipt(
        &fixture, 0, state.ledger.ledger_entries_root,
        state.candidate_manifest.source_tree_sha256, "candidate", None, false, 890,
    );
    let stale = first["receipt_sha256"].as_str().unwrap().parse().unwrap();
    state = state.apply(&fixture.context, Phase0Event::CheckerFailure { receipt: first }).unwrap();
    // An unchanged worker generation is not required merely to retry the
    // authoritative checker, so put the protocol back in Check for this
    // adversarial receipt-order fixture.
    let mut raw_state = serde_json::to_value(&state).unwrap();
    raw_state["stage"] = json!("check");
    raw_state["state_sha256"] = json!(Sha256Digest::ZERO);
    let resealed = {
        let digest = trellis_kernel::trust_base::self_digest(
            trellis_kernel::trust_base::DomainTag::RawArtifact,
            &raw_state,
            "state_sha256",
        ).unwrap();
        raw_state["state_sha256"] = json!(digest);
        serde_json::from_value::<Phase0State>(raw_state).unwrap()
    };
    state = resealed;
    let latest = receipt(
        &fixture, 0, state.ledger.ledger_entries_root,
        state.candidate_manifest.source_tree_sha256, "candidate", None, false, 891,
    );
    state = state.apply(&fixture.context, Phase0Event::CheckerFailure { receipt: latest }).unwrap();
    let cargo = b"[package]\nname='fixture'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n";
    let error = submit_single_entry_revision(
        &fixture, state, draft_entry(stale, cargo), cargo, "stale-authority",
    ).unwrap_err();
    assert!(error.detail.contains("unavailable") || error.detail.contains("forged"));
}

#[test]
fn genesis_rejects_any_handback_override_and_json_unknown_fields() {
    let fixture = fixture();
    let mut genesis = fixture.genesis.clone();
    genesis.policy.max_phase0_handbacks = 31;
    genesis = genesis.seal().unwrap();
    assert!(genesis.validate().is_err());

    let mut escaped_opam = fixture.genesis.clone();
    escaped_opam.toolchain.opam_switch_environment.insert(
        "OCAML_TOPLEVEL_PATH".to_owned(),
        "/outside-the-sealed-switch".to_owned(),
    );
    escaped_opam = escaped_opam.seal().unwrap();
    let error = escaped_opam.validate().unwrap_err();
    assert!(error.detail.contains("escapes the switch prefix"));

    let mut same_model = fixture.genesis.clone();
    same_model.policy.allow_same_model_lanes = false;
    same_model = same_model.seal().unwrap();
    assert!(same_model.validate().is_err());
    same_model.bindings.seam_repair[0].provider = "independent".to_owned();
    same_model.bindings.seam_repair[0] = same_model.bindings.seam_repair[0].clone().seal().unwrap();
    same_model.bindings.source_correspondence[0].provider = "independent".to_owned();
    same_model.bindings.source_correspondence[0] =
        same_model.bindings.source_correspondence[0].clone().seal().unwrap();
    same_model = same_model.seal().unwrap();
    same_model.validate().unwrap();

    let bytes = serde_json::to_vec(&fixture.context).unwrap();
    let mut value: Value = serde_json::from_slice(&bytes).unwrap();
    value["untrusted_override"] = json!(true);
    let canonical = trellis_kernel::trust_base::canonical_json_value(&value).unwrap();
    assert!(Phase0ValidationContext::parse_and_validate(&canonical).is_err());
}

#[test]
fn trust_cli_phase0_commands_use_the_validated_state_machine() {
    let fixture = fixture();
    let root = fixture._temp.path();
    let genesis_path = root.join("genesis.json");
    let ledger_path = root.join("identity-ledger.json");
    let context_path = root.join("context.json");
    let state_path = root.join("state.json");
    let canonical = |value: Value| {
        trellis_kernel::trust_base::canonical_json_value(&value).unwrap()
    };
    fs::write(&genesis_path, canonical(serde_json::to_value(&fixture.genesis).unwrap())).unwrap();
    fs::write(&ledger_path, canonical(serde_json::to_value(&fixture.identity).unwrap())).unwrap();
    fs::write(&context_path, canonical(serde_json::to_value(&fixture.context).unwrap())).unwrap();
    let cli = env!("CARGO_BIN_EXE_trellis_trust_cli");
    let run = |args: &[&str]| Command::new(cli).args(args).output().unwrap();

    let init = run(&[
        "phase0-init",
        genesis_path.to_str().unwrap(),
        ledger_path.to_str().unwrap(),
        context_path.to_str().unwrap(),
        state_path.to_str().unwrap(),
    ]);
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    let check = run(&[
        "phase0-check",
        state_path.to_str().unwrap(),
        context_path.to_str().unwrap(),
    ]);
    assert!(check.status.success(), "{}", String::from_utf8_lossy(&check.stderr));
    let next = run(&[
        "phase0-next",
        state_path.to_str().unwrap(),
        context_path.to_str().unwrap(),
    ]);
    assert!(next.status.success(), "{}", String::from_utf8_lossy(&next.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&next.stdout).unwrap()["kind"], "check");

    let event_path = root.join("failure-event.json");
    let next_state = root.join("worker-state.json");
    let failure = receipt(
        &fixture,
        0,
        fixture.identity.ledger_entries_root,
        fixture.identity.adapted_tree_sha256,
        "candidate",
        None,
        false,
        900,
    );
    fs::write(
        &event_path,
        canonical(json!({"kind": "checker_failure", "receipt": failure})),
    )
    .unwrap();
    let apply = run(&[
        "phase0-apply",
        state_path.to_str().unwrap(),
        context_path.to_str().unwrap(),
        event_path.to_str().unwrap(),
        next_state.to_str().unwrap(),
    ]);
    assert!(apply.status.success(), "{}", String::from_utf8_lossy(&apply.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&apply.stdout).unwrap()["stage"], "worker");

    let finalize = run(&[
        "phase0-finalize",
        next_state.to_str().unwrap(),
        context_path.to_str().unwrap(),
        root.join("sealed.json").to_str().unwrap(),
        root.join("sealed-state.json").to_str().unwrap(),
    ]);
    assert!(!finalize.status.success(), "an unclean worker stage cannot finalize");
}

#[test]
fn repeated_checker_failures_retain_receipts_without_semantic_handback_charge() {
    let fixture = fixture();
    let mut state = Phase0State::initialize(
        fixture.genesis.clone(), fixture.identity.clone(), &fixture.context,
    ).unwrap();
    let initial = receipt(
        &fixture, 0, state.ledger.ledger_entries_root,
        state.candidate_manifest.source_tree_sha256, "candidate", None, false, 100,
    );
    let discovery: Sha256Digest = initial["receipt_sha256"].as_str().unwrap().parse().unwrap();
    state = state.apply(&fixture.context, Phase0Event::CheckerFailure { receipt: initial }).unwrap();
    let cargo = b"[package]\nname='fixture'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n";
    let mut entry = draft_entry(discovery, cargo);
    let mut generation = 1;
    let mut ledger = Phase0AdaptationLedger {
        schema: PHASE0_ADAPTATION_LEDGER_SCHEMA.to_owned(),
        generation,
        unadapted_tree_sha256: fixture.genesis.unadapted_source_manifest.source_tree_sha256,
        adapted_tree_sha256: raw_sha256(b"placeholder until the worker writes its proposal"),
        goal_sha256: fixture.genesis.goal_sha256,
        entries: vec![entry.clone()],
        ledger_entries_root: Sha256Digest::ZERO,
        ledger_sha256: Sha256Digest::ZERO,
    }.seal().unwrap();
    let mut request = match state.next_request(&fixture.context).unwrap() {
        Phase0NextRequest::Worker(request) => request,
        other => panic!("unexpected request: {other:?}"),
    };
    fs::write(fixture.candidate.join("Cargo.toml"), cargo).unwrap();
    ledger.adapted_tree_sha256 = source_tree_manifest(&fixture.candidate)
        .unwrap()
        .source_tree_sha256;
    ledger = ledger.seal().unwrap();
    let first_snapshot = fixture._temp.path().join("bound-pre-0");
    fs::create_dir(&first_snapshot).unwrap();
    fs::copy(fixture.raw.join("lib.rs"), first_snapshot.join("lib.rs")).unwrap();
    let response = Phase0WorkerResponse {
        schema: PHASE0_WORKER_RESPONSE_SCHEMA.to_owned(),
        request_id: request.request_id,
        request_sha256: request.request_sha256,
        base_generation: 0,
        worker_thread_id: "same-thread".to_owned(),
        candidate_manifest: source_tree_manifest(&fixture.candidate).unwrap(),
        ledger: ledger.clone(),
        discovery_receipts: Vec::new(),
        response_sha256: Sha256Digest::ZERO,
    }.seal().unwrap();
    state = state.apply(&fixture.context, Phase0Event::WorkerRevision {
        response,
        pre_burst_candidate_path: first_snapshot.to_str().unwrap().to_owned(),
    }).unwrap();

    for ordinal in 0..MAX_PHASE0_HANDBACKS {
        let failed = receipt(
            &fixture, generation, state.ledger.ledger_entries_root,
            state.candidate_manifest.source_tree_sha256, "candidate", None, false,
            200 + ordinal as u64,
        );
        let current_discovery: Sha256Digest = failed["receipt_sha256"]
            .as_str().unwrap().parse().unwrap();
        state = state.apply(&fixture.context, Phase0Event::CheckerFailure { receipt: failed }).unwrap();
        if ordinal + 1 == MAX_PHASE0_HANDBACKS {
            break;
        }
        assert_eq!(state.stage, Phase0Stage::Worker);
        request = match state.next_request(&fixture.context).unwrap() {
            Phase0NextRequest::Worker(request) => request,
            other => panic!("unexpected request: {other:?}"),
        };
        let snapshot = fixture._temp.path().join(format!("bound-pre-{generation}"));
        fs::create_dir(&snapshot).unwrap();
        fs::copy(fixture.candidate.join("lib.rs"), snapshot.join("lib.rs")).unwrap();
        fs::copy(fixture.candidate.join("Cargo.toml"), snapshot.join("Cargo.toml")).unwrap();
        generation += 1;
        entry.status = Phase0LedgerStatus::Draft;
        entry.ablation_failure_receipt_sha256 = Sha256Digest::ZERO;
        entry.discovery_failure_receipt_sha256 = current_discovery;
        entry.quoted_error.receipt_sha256 = current_discovery;
        ledger.generation = generation;
        ledger.entries = vec![entry.clone()];
        ledger = ledger.seal().unwrap();
        let response = Phase0WorkerResponse {
            schema: PHASE0_WORKER_RESPONSE_SCHEMA.to_owned(),
            request_id: request.request_id,
            request_sha256: request.request_sha256,
            base_generation: generation - 1,
            worker_thread_id: "same-thread".to_owned(),
            candidate_manifest: source_tree_manifest(&fixture.candidate).unwrap(),
            ledger: ledger.clone(),
            discovery_receipts: Vec::new(),
            response_sha256: Sha256Digest::ZERO,
        }.seal().unwrap();
        state = state.apply(&fixture.context, Phase0Event::WorkerRevision {
            response,
            pre_burst_candidate_path: snapshot.to_str().unwrap().to_owned(),
        }).unwrap();
    }
    assert_eq!(state.stage, Phase0Stage::Worker);
    assert_eq!(state.phase0_handbacks, 0);
    assert_eq!(state.checker_receipts.len(), MAX_PHASE0_HANDBACKS as usize + 1);
    assert_eq!(state.generation_history.len(), MAX_PHASE0_HANDBACKS as usize);
    assert!(state.sealed_generation.is_none());
}

#[test]
fn tla_stage_names_match_the_protocol_wire_stages() {
    let pairs = [
        (Phase0Stage::Worker, "Worker"),
        (Phase0Stage::Check, "Check"),
        (Phase0Stage::SeamAudit, "SeamAudit"),
        (Phase0Stage::CorrespondenceAudit, "CorrespondenceAudit"),
        (Phase0Stage::Sealed, "Sealed"),
        (Phase0Stage::Incomplete, "Incomplete"),
    ];
    for (stage, expected) in pairs {
        assert_eq!(stage.tla_name(), expected);
    }
    assert_eq!(MAX_PHASE0_HANDBACKS, 32);
    assert_eq!(BTreeSet::from(["goal:target".to_owned()]).len(), 1);
}


#[test]
fn trust_cli_phase0_apply_accepts_a_worker_revision_with_the_delta_in_the_live_candidate() {
    // Regression: the CLI validated the pre-state against the live candidate
    // before `apply` could use the pre-burst snapshot, so the first real
    // worker revision of a run (candidate = raw tree + Cargo.toml) was
    // rejected with "state source trees do not equal ledger replay" and
    // Phase 0 could never advance past its first edit.
    let fixture = fixture();
    let root = fixture._temp.path();
    let genesis_path = root.join("genesis.json");
    let ledger_path = root.join("identity-ledger.json");
    let context_path = root.join("context.json");
    let state_path = root.join("state.json");
    let canonical = |value: Value| {
        trellis_kernel::trust_base::canonical_json_value(&value).unwrap()
    };
    fs::write(&genesis_path, canonical(serde_json::to_value(&fixture.genesis).unwrap())).unwrap();
    fs::write(&ledger_path, canonical(serde_json::to_value(&fixture.identity).unwrap())).unwrap();
    fs::write(&context_path, canonical(serde_json::to_value(&fixture.context).unwrap())).unwrap();
    let cli = env!("CARGO_BIN_EXE_trellis_trust_cli");
    let run = |args: &[&str]| Command::new(cli).args(args).output().unwrap();

    let init = run(&[
        "phase0-init",
        genesis_path.to_str().unwrap(),
        ledger_path.to_str().unwrap(),
        context_path.to_str().unwrap(),
        state_path.to_str().unwrap(),
    ]);
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));

    // Raw-tree diagnostic: the checker fails, the state returns to the worker.
    let failure = receipt(
        &fixture,
        0,
        fixture.identity.ledger_entries_root,
        fixture.identity.adapted_tree_sha256,
        "candidate",
        None,
        false,
        900,
    );
    let failure_digest: Sha256Digest = failure["receipt_sha256"].as_str().unwrap().parse().unwrap();
    let failure_event = root.join("failure-event.json");
    let worker_state = root.join("worker-state.json");
    fs::write(&failure_event, canonical(json!({"kind": "checker_failure", "receipt": failure}))).unwrap();
    let apply = run(&[
        "phase0-apply",
        state_path.to_str().unwrap(),
        context_path.to_str().unwrap(),
        failure_event.to_str().unwrap(),
        worker_state.to_str().unwrap(),
    ]);
    assert!(apply.status.success(), "{}", String::from_utf8_lossy(&apply.stderr));

    // The worker burst: snapshot the pre-burst candidate, then write the
    // delta into the LIVE candidate exactly as the sandboxed worker does.
    let state = Phase0State::parse_and_validate(&fs::read(&worker_state).unwrap(), &fixture.context).unwrap();
    let request = match state.next_request(&fixture.context).unwrap() {
        Phase0NextRequest::Worker(request) => request,
        other => panic!("unexpected request: {other:?}"),
    };
    let snapshot = root.join("pre-burst-snapshot");
    fs::create_dir(&snapshot).unwrap();
    fs::copy(fixture.raw.join("lib.rs"), snapshot.join("lib.rs")).unwrap();
    let cargo = b"[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[lib]\npath = \"lib.rs\"\n";
    fs::write(fixture.candidate.join("Cargo.toml"), cargo).unwrap();
    let ledger = Phase0AdaptationLedger {
        schema: PHASE0_ADAPTATION_LEDGER_SCHEMA.to_owned(),
        generation: state.generation + 1,
        unadapted_tree_sha256: fixture.genesis.unadapted_source_manifest.source_tree_sha256,
        adapted_tree_sha256: source_tree_manifest(&fixture.candidate).unwrap().source_tree_sha256,
        goal_sha256: fixture.genesis.goal_sha256,
        entries: vec![draft_entry(failure_digest, cargo)],
        ledger_entries_root: Sha256Digest::ZERO,
        ledger_sha256: Sha256Digest::ZERO,
    }
    .seal()
    .unwrap();
    let response = Phase0WorkerResponse {
        schema: PHASE0_WORKER_RESPONSE_SCHEMA.to_owned(),
        request_id: request.request_id,
        request_sha256: request.request_sha256,
        base_generation: state.generation,
        worker_thread_id: "cli-worker".to_owned(),
        candidate_manifest: source_tree_manifest(&fixture.candidate).unwrap(),
        ledger,
        discovery_receipts: Vec::new(),
        response_sha256: Sha256Digest::ZERO,
    }
    .seal()
    .unwrap();
    let revision_event = root.join("revision-event.json");
    let check_state = root.join("check-state.json");
    fs::write(
        &revision_event,
        canonical(
            serde_json::to_value(&Phase0Event::WorkerRevision {
                response,
                pre_burst_candidate_path: snapshot.to_str().unwrap().to_owned(),
            })
            .unwrap(),
        ),
    )
    .unwrap();
    let apply = run(&[
        "phase0-apply",
        worker_state.to_str().unwrap(),
        context_path.to_str().unwrap(),
        revision_event.to_str().unwrap(),
        check_state.to_str().unwrap(),
    ]);
    assert!(
        apply.status.success(),
        "worker revision through the CLI must be accepted: {}",
        String::from_utf8_lossy(&apply.stderr)
    );
    let result: Value = serde_json::from_slice(&apply.stdout).unwrap();
    assert_eq!(result["stage"], "check");
    assert_eq!(result["generation"], 1);

    // The new state validates against the live candidate that now carries the delta.
    let check = run(&[
        "phase0-check",
        check_state.to_str().unwrap(),
        context_path.to_str().unwrap(),
    ]);
    assert!(check.status.success(), "{}", String::from_utf8_lossy(&check.stderr));

    // A worker revision whose snapshot does not match the recorded pre-state
    // is still refused: the snapshot-aware check is what runs, not nothing.
    let bad_snapshot = root.join("bad-snapshot");
    fs::create_dir(&bad_snapshot).unwrap();
    fs::write(bad_snapshot.join("lib.rs"), b"// not the pre-burst tree\n").unwrap();
    let bad_event = root.join("bad-revision-event.json");
    let mut bad: Value = serde_json::from_slice(&fs::read(&revision_event).unwrap()).unwrap();
    bad["pre_burst_candidate_path"] = json!(bad_snapshot.to_str().unwrap());
    fs::write(&bad_event, canonical(bad)).unwrap();
    let refused = run(&[
        "phase0-apply",
        worker_state.to_str().unwrap(),
        context_path.to_str().unwrap(),
        bad_event.to_str().unwrap(),
        root.join("unused-state.json").to_str().unwrap(),
    ]);
    assert!(!refused.status.success(), "a mismatching pre-burst snapshot must be refused");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("ledger replay"));
}

#[test]
fn interactive_self_check_receipts_do_not_displace_the_authoritative_failure() {
    // Regression from the first live run to reach `apply` with a real
    // worker draft: the worker quoted the raw-tree diagnostic that motivated
    // its Cargo.toml and also delivered the receipt of its own interactive
    // self-check. `apply` inserted that receipt as the new authoritative
    // failure before validating, so the honest quote was refused as
    // "unavailable discovery evidence". A new entry may cite either the
    // authoritative failure or an interactive receipt delivered with it.
    let submit = |quote_interactive: bool, forge_unknown: bool| {
        let fixture = fixture();
        let mut state = Phase0State::initialize(
            fixture.genesis.clone(), fixture.identity.clone(), &fixture.context,
        )
        .unwrap();
        let discovery = receipt(
            &fixture, 0, state.ledger.ledger_entries_root,
            state.candidate_manifest.source_tree_sha256, "candidate", None, false, 900,
        );
        let discovery_digest: Sha256Digest =
            discovery["receipt_sha256"].as_str().unwrap().parse().unwrap();
        state = state
            .apply(&fixture.context, Phase0Event::CheckerFailure { receipt: discovery })
            .unwrap();
        let request = match state.next_request(&fixture.context).unwrap() {
            Phase0NextRequest::Worker(request) => request,
            other => panic!("unexpected request: {other:?}"),
        };
        let snapshot = fixture._temp.path().join("pre-burst");
        fs::create_dir(&snapshot).unwrap();
        fs::copy(fixture.raw.join("lib.rs"), snapshot.join("lib.rs")).unwrap();
        let cargo = b"[package]\nname='fixture'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n";
        fs::write(fixture.candidate.join("Cargo.toml"), cargo).unwrap();
        let edited_root = source_tree_manifest(&fixture.candidate).unwrap().source_tree_sha256;
        // The worker's own check of the edited tree: still generation 0,
        // still the empty ledger root, failed on the next diagnostic.
        // (Same failure line as `draft_entry`'s quote: the quote must be
        // byte-exact against whichever receipt the entry cites.)
        let interactive = receipt(
            &fixture, 0, state.ledger.ledger_entries_root, edited_root, "candidate",
            None, false, 901,
        );
        let interactive_digest: Sha256Digest =
            interactive["receipt_sha256"].as_str().unwrap().parse().unwrap();
        let cited = if forge_unknown {
            raw_sha256(b"no such receipt")
        } else if quote_interactive {
            interactive_digest
        } else {
            discovery_digest
        };
        let ledger = Phase0AdaptationLedger {
            schema: PHASE0_ADAPTATION_LEDGER_SCHEMA.to_owned(),
            generation: 1,
            unadapted_tree_sha256: fixture.genesis.unadapted_source_manifest.source_tree_sha256,
            adapted_tree_sha256: edited_root,
            goal_sha256: fixture.genesis.goal_sha256,
            entries: vec![draft_entry(cited, cargo)],
            ledger_entries_root: Sha256Digest::ZERO,
            ledger_sha256: Sha256Digest::ZERO,
        }
        .seal()
        .unwrap();
        let response = Phase0WorkerResponse {
            schema: PHASE0_WORKER_RESPONSE_SCHEMA.to_owned(),
            request_id: request.request_id,
            request_sha256: request.request_sha256,
            base_generation: state.generation,
            worker_thread_id: "self-checking-worker".to_owned(),
            candidate_manifest: source_tree_manifest(&fixture.candidate).unwrap(),
            ledger,
            discovery_receipts: vec![interactive],
            response_sha256: Sha256Digest::ZERO,
        }
        .seal()
        .unwrap();
        let outcome = state.apply(
            &fixture.context,
            Phase0Event::WorkerRevision {
                response,
                pre_burst_candidate_path: snapshot.to_str().unwrap().to_owned(),
            },
        );
        (outcome, discovery_digest, interactive_digest)
    };

    let (accepted, discovery_digest, interactive_digest) = submit(false, false);
    let state = accepted.expect("quoting the authoritative failure after a self-check is accepted");
    assert_eq!(state.stage, Phase0Stage::Check);
    assert_eq!(state.generation, 1);
    // Both receipts are retained as evidence; the authoritative pointer did not move.
    let digests: BTreeSet<_> = state.checker_receipts.iter().map(|r| r.receipt_sha256).collect();
    assert!(digests.contains(&discovery_digest) && digests.contains(&interactive_digest));
    assert_eq!(state.last_failure_receipt_sha256, Some(discovery_digest));

    let (accepted, _, _) = submit(true, false);
    assert_eq!(
        accepted.expect("an entry motivated by the delivered self-check is accepted").generation,
        1
    );

    let (refused, _, _) = submit(false, true);
    let error = refused.expect_err("an entry citing a receipt nobody holds is refused");
    assert!(error.detail.contains("unavailable discovery"), "{error:?}");
}
