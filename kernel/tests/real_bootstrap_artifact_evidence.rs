#![recursion_limit = "256"]

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;
use trellis_kernel::phase0::*;
use trellis_kernel::trust_base::{
    canonical_json_value, execute_phase0_checked_json, parse_json_strict, raw_sha256,
    source_tree_manifest, verify_evidence_tool_manifest, ExecutionLimits,
    Phase0AdaptationLedger, Phase0ExecutionRequest, PinnedExecutionRequest, Sha256Digest,
    PHASE0_ADAPTATION_LEDGER_SCHEMA, RUST_WITNESS_RUNNER_LOGICAL_ID,
};

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let file_type = entry.file_type().unwrap();
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            assert!(file_type.is_file(), "fixture tree contains a special member");
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn checked(label: &str, output: Output) -> Output {
    assert!(
        output.status.success(),
        "{label} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn git(checkout: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(checkout)
        .args(args)
        .output()
        .unwrap()
}

fn write_executable(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }
}

fn pin_file(path: &Path) -> Phase0PinnedFile {
    Phase0PinnedFile {
        path: path.canonicalize().unwrap().to_string_lossy().into_owned(),
        sha256: raw_sha256(&fs::read(path).unwrap()),
    }
}

fn lane(id: &str) -> Phase0LaneBinding {
    Phase0LaneBinding {
        lane_id: id.to_owned(),
        provider: "stub".to_owned(),
        model: "sealed-bootstrap-fixture".to_owned(),
        effort: "fixed".to_owned(),
        extra_args: Vec::new(),
        fallback_models: Vec::new(),
        binding_sha256: Sha256Digest::ZERO,
    }
    .seal()
    .unwrap()
}

fn command_tool(command: &Value, tool: &str) -> PathBuf {
    PathBuf::from(
        command["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|run| run["run_role"] == "primary")
            .unwrap()["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["tool"] == tool)
            .unwrap()["argv"][0]
            .as_str()
            .unwrap(),
    )
}

fn checkout_tool(command: &Value, tool: &str) -> Phase0PinnedCheckoutTool {
    let record = command["tool_source_checkouts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["tool"] == tool)
        .unwrap();
    let root = PathBuf::from(record["repository_root"].as_str().unwrap());
    let revision = record["commit"].as_str().unwrap().to_owned();
    Phase0PinnedCheckoutTool {
        executable: pin_file(&command_tool(command, tool)),
        source: Phase0PinnedTree {
            path: root.canonicalize().unwrap().to_string_lossy().into_owned(),
            source_tree_sha256: git_checkout_source_digest(&root, &revision).unwrap(),
        },
        revision,
    }
}

fn checker_stage(name: &str) -> Value {
    json!({
        "name": name,
        "tool": "phase0-checker",
        "status": "passed",
        "timed_out": false,
        "stdout_base64": "",
        "stdout_byte_length": 0,
        "stdout_sha256": raw_sha256(b""),
        "stdout_truncated": false,
        "stderr_base64": "",
        "stderr_byte_length": 0,
        "stderr_sha256": raw_sha256(b""),
        "stderr_truncated": false,
    })
}

fn checker_receipt(
    genesis: &Phase0Genesis,
    runner: &Path,
    work: &Path,
    ledger: &Phase0AdaptationLedger,
    candidate_root: Sha256Digest,
    kind: &str,
    ordinal: u64,
    generated_roots: &Value,
    tablet_roots: &Value,
) -> Value {
    let final_replay = kind == "ratification_replay";
    let stages = if final_replay {
        vec![
            checker_stage("second_clean_extraction"),
            checker_stage("filespec_region_gates"),
        ]
    } else {
        vec![checker_stage("generated_model_split")]
    };
    let input = json!({
        "phase0_genesis_sha256": genesis.genesis_sha256,
        "runner_path": genesis.components.checker_runner.path,
        "runner_sha256": genesis.components.checker_runner.sha256,
        "extractor_path": genesis.components.extractor.path,
        "extractor_sha256": genesis.components.extractor.sha256,
        "filespec_checker_path": genesis.components.filespec_checker.path,
        "filespec_checker_sha256": genesis.components.filespec_checker.sha256,
        "prescribed_region_checker_path": genesis.components.prescribed_region_checker.path,
        "prescribed_region_checker_sha256": genesis.components.prescribed_region_checker.sha256,
        "repository_root": genesis.components.repository_root,
        "cargo_path": genesis.toolchain.cargo.path,
        "cargo_sha256": genesis.toolchain.cargo.sha256,
        "rustc_path": genesis.toolchain.rustc.path,
        "rustc_sha256": genesis.toolchain.rustc.sha256,
        "toolchain_root": genesis.toolchain.toolchain_root,
        "charon_toolchain_root": genesis.toolchain.charon_toolchain.path,
        "charon_toolchain_tree_sha256": genesis.toolchain.charon_toolchain.source_tree_sha256,
        "charon_path": genesis.toolchain.charon.executable.path,
        "charon_sha256": genesis.toolchain.charon.executable.sha256,
        "charon_source_root": genesis.toolchain.charon.source.path,
        "charon_source_tree_sha256": genesis.toolchain.charon.source.source_tree_sha256,
        "aeneas_path": genesis.toolchain.aeneas.executable.path,
        "aeneas_sha256": genesis.toolchain.aeneas.executable.sha256,
        "aeneas_source_root": genesis.toolchain.aeneas.source.path,
        "aeneas_source_tree_sha256": genesis.toolchain.aeneas.source.source_tree_sha256,
        "dependency_cache_path": "",
        "dependency_cache_tree_sha256": raw_sha256(b""),
        "opam_switch_prefix": genesis.toolchain.opam_switch_prefix.path,
        "opam_switch_prefix_tree_sha256": genesis.toolchain.opam_switch_prefix.source_tree_sha256,
        "opam_switch_environment": genesis.toolchain.opam_switch_environment,
        "target_triple": genesis.toolchain.target_triple,
        "extraction_profile": genesis.toolchain.extraction_profile,
        "opam_switch": genesis.toolchain.opam_switch,
        "closed_environment": genesis.toolchain.closed_environment,
        "goal_sha256": genesis.goal_sha256,
        "ledger_generation": ledger.generation,
        "ledger_root_sha256": ledger.ledger_entries_root,
        "candidate_tree_sha256": candidate_root,
        "check_kind": kind,
        "check_id": format!("identity-{kind}-{ordinal}"),
        "output_root": work,
        "run_id": format!("identity-{kind}-{ordinal}"),
        "reverted_entry_id": null,
        "mock_output": {
            "schema": "trellis-pv-phase0-checker-output/v1",
            "status": "passed",
            "phase0_genesis_sha256": genesis.genesis_sha256,
            "goal_sha256": genesis.goal_sha256,
            "ledger_generation": ledger.generation,
            "ledger_root_sha256": ledger.ledger_entries_root,
            "candidate_tree_sha256": candidate_root,
            "check_kind": kind,
            "check_id": format!("identity-{kind}-{ordinal}"),
            "reverted_entry_id": null,
            "first_failing_stage": null,
            // This identity fixture records no model scan, and the GOAL
            // binding below is built without one, so the two agree.
            "model_bodiless_declarations": {
                "status": "not_extracted",
                "read_from": "generated.lean",
                "detail": "no extraction produced a model in this check",
                "declarations": [],
            },
            "source_facts": {
                "candidate_tree_sha256": candidate_root,
                "compile_initial_tree_sha256": candidate_root,
                "compile_final_tree_sha256": candidate_root,
                "extraction_initial_tree_sha256": candidate_root,
                "extraction_final_tree_sha256": candidate_root,
            },
            "stages": stages,
            "a_b_generated_lean_roots": if final_replay { generated_roots.clone() } else { json!([]) },
            "a_b_tablet_roots": if final_replay { tablet_roots.clone() } else { json!([]) },
            "generated_lean_root_sha256": if final_replay { generated_roots[0].clone() } else { Value::Null },
            "tablet_root_sha256": if final_replay { tablet_roots[0].clone() } else { Value::Null },
        },
    });
    let environment = BTreeMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())]);
    execute_phase0_checked_json(Phase0ExecutionRequest {
        phase0_genesis_sha256: genesis.genesis_sha256,
        purpose: "source_adaptation_checker",
        execution: PinnedExecutionRequest {
            runner_path: runner,
            expected_runner_sha256: genesis.components.checker_runner.sha256,
            command_id: "real-bootstrap-phase0-fixture",
            working_directory: work,
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

fn audit_pass(request: &Phase0AuditRequest) -> Phase0AuditResult {
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
        entry_verdicts: Vec::new(),
        whole_tree_verdict: (request.scenario == Phase0AuditScenario::SourceCorrespondence)
            .then(|| Phase0WholeTreeVerdict {
                decision: Phase0AuditDecision::Pass,
                judgment: "The unchanged package and extracted targets preserve GOAL continuity."
                    .to_owned(),
            }),
        goal_binding_verdicts: if request.scenario == Phase0AuditScenario::SourceCorrespondence {
            request.goal_binding_targets.iter().map(|target| Phase0GoalBindingAuditVerdict {
                target_id: target.target_id.clone(),
                origin: target.origin,
                goal_binding_outside_target: target.goal_binding_outside_target,
                goal_binding_in_adaptation: target.goal_binding_in_adaptation,
                decision: Phase0AuditDecision::Pass,
                judgment: "The binding is explicitly accepted.".to_owned(),
            }).collect()
        } else { Vec::new() },
        findings: Vec::new(),
        result_sha256: Sha256Digest::ZERO,
    }
    .seal()
    .unwrap()
}

fn seal_identity_phase0(
    trellis_root: &Path,
    campaign: &Path,
    work: &Path,
) -> (PathBuf, PathBuf) {
    let extraction = campaign.join("extraction-evidence");
    let command: Value = serde_json::from_slice(&fs::read(extraction.join("COMMAND.json")).unwrap())
        .unwrap();
    let determinism: Value =
        serde_json::from_slice(&fs::read(extraction.join("DETERMINISM.json")).unwrap()).unwrap();
    let tools = work.join("phase0-tools");
    fs::create_dir(&tools).unwrap();
    let runner = tools.join("checker.py");
    write_executable(
        &runner,
        b"#!/usr/bin/python3\nimport json,sys\nr=json.load(sys.stdin)\nsys.stdout.write(json.dumps(r['mock_output'],sort_keys=True,separators=(',',':')))\n",
    );
    let runner_pin = pin_file(&runner);
    let extractor = trellis_root.join("scripts/extract_pv_model.py");
    let cargo = command_tool(&command, "cargo-metadata");
    let rustc = command_tool(&command, "rustc-version");
    let toolchain_root = cargo.parent().unwrap().parent().unwrap().canonicalize().unwrap();
    let charon = checkout_tool(&command, "charon");
    let aeneas = checkout_tool(&command, "aeneas");
    let filespec_checker = command_tool(&command, "kernel-scan-tablet-filespec");
    let prescribed_region_checker =
        command_tool(&command, "kernel-print-prescribed-region");
    let opam_switch_prefix = Phase0PinnedTree {
        path: tools.canonicalize().unwrap().to_string_lossy().into_owned(),
        source_tree_sha256: readonly_closure_digest(&tools).unwrap(),
    };
    let opam_prefix_path = tools.canonicalize().unwrap().to_string_lossy().into_owned();
    let opam_switch_environment = BTreeMap::from([
        ("CAML_LD_LIBRARY_PATH".to_owned(), opam_prefix_path.clone()),
        ("OCAMLTOP_INCLUDE_PATH".to_owned(), opam_prefix_path.clone()),
        ("OCAML_TOPLEVEL_PATH".to_owned(), opam_prefix_path.clone()),
        ("OPAMSWITCH".to_owned(), "fixture".to_owned()),
        ("OPAM_SWITCH_PREFIX".to_owned(), opam_prefix_path),
    ]);
    let goal = fs::read(campaign.join("GOAL.md")).unwrap();
    let source_manifest = source_tree_manifest(&campaign.join("crate")).unwrap();
    let provenance = &command["extraction_provenance"]["extraction_toolchain"];
    let genesis = Phase0Genesis {
        schema: PHASE0_GENESIS_SCHEMA.to_owned(),
        target_upload_sha256: raw_sha256(b"real-bootstrap-generic-target-upload"),
        supporting_upload_sha256: None,
        source_partition_manifest: Phase0SourcePartitionManifest::target_only(
            &source_manifest,
        ).unwrap(),
        unadapted_source_manifest: source_manifest.clone(),
        goal_base64: BASE64_STANDARD.encode(&goal),
        goal_sha256: raw_sha256(&goal),
        policy: Phase0Policy {
            max_phase0_handbacks: MAX_PHASE0_HANDBACKS,
            allow_same_model_lanes: true,
        },
        bindings: Phase0Bindings {
            worker: lane("worker"),
            seam_repair: vec![lane("seam")],
            source_correspondence: vec![lane("correspondence")],
        },
        prompt_contract_version: 64,
        components: Phase0ComponentClosure {
            repository_root: trellis_root.to_string_lossy().into_owned(),
            kernel: runner_pin.clone(),
            controller: runner_pin.clone(),
            bridge: runner_pin.clone(),
            checker_broker: runner_pin.clone(),
            checker_runner: runner_pin.clone(),
            extractor: pin_file(&extractor),
            filespec_checker: pin_file(&filespec_checker),
            prescribed_region_checker: pin_file(&prescribed_region_checker),
            production_extraction_runner: pin_file(
                &trellis_root.join("scripts/pv_source_adaptation_sandbox.py"),
            ),
            goal_binding_reporter: pin_file(&trellis_root.join("scripts/pv_goal_binding.py")),
            model_opacity_scanner: pin_file(&trellis_root.join("scripts/pv_model_opacity.py")),
            worker_fragment: runner_pin.clone(),
            seam_audit_fragment: runner_pin.clone(),
            correspondence_audit_fragment: runner_pin.clone(),
        },
        toolchain: Phase0ToolchainClosure {
            cargo: pin_file(&cargo),
            cargo_identity: String::from_utf8(
                checked(
                    "cargo identity",
                    Command::new(&cargo).args(["--version", "--verbose"]).output().unwrap(),
                )
                .stdout,
            )
            .unwrap()
            .trim()
            .to_owned(),
            rustc: pin_file(&rustc),
            rustc_identity: provenance["rustc_vv"].as_str().unwrap().trim().to_owned(),
            toolchain_root: toolchain_root.to_string_lossy().into_owned(),
            charon_toolchain: Phase0PinnedTree {
                path: toolchain_root.to_string_lossy().into_owned(),
                source_tree_sha256: readonly_closure_digest(&toolchain_root).unwrap(),
            },
            charon,
            aeneas,
            dependency_cache: None,
            opam_switch_prefix,
            opam_switch_environment,
            opam_switch: "fixture".to_owned(),
            target_triple: provenance["target_triple"].as_str().unwrap().to_owned(),
            extraction_profile: provenance["extraction_profile"].as_str().unwrap().to_owned(),
            closed_environment: BTreeMap::from([("LANG".to_owned(), "C.UTF-8".to_owned())]),
        },
        genesis_sha256: Sha256Digest::ZERO,
    }
    .seal()
    .unwrap();
    let ledger = Phase0AdaptationLedger {
        schema: PHASE0_ADAPTATION_LEDGER_SCHEMA.to_owned(),
        generation: 0,
        unadapted_tree_sha256: source_manifest.source_tree_sha256,
        adapted_tree_sha256: source_manifest.source_tree_sha256,
        goal_sha256: genesis.goal_sha256,
        entries: Vec::new(),
        ledger_entries_root: Sha256Digest::ZERO,
        ledger_sha256: Sha256Digest::ZERO,
    }
    .seal()
    .unwrap();

    let binding_path = work.join("goal-binding-report.json");
    checked(
        "GOAL binding",
        Command::new("python3")
            .current_dir(trellis_root)
            .arg(trellis_root.join("scripts/pv_goal_binding.py"))
            .arg("--goal")
            .arg(campaign.join("GOAL.md"))
            .arg("--pv-tablet")
            .arg(campaign.join("pv_tablet.json"))
            .arg("--adapted-tree")
            .arg(campaign.join("crate"))
            .arg("--unadapted-tree")
            .arg(campaign.join("unadapted-crate"))
            .arg("--require-complete")
            .arg("--out")
            .arg(&binding_path)
            .output()
            .unwrap(),
    );
    let goal_binding_report: Phase0GoalBindingReport =
        serde_json::from_slice(&fs::read(&binding_path).unwrap()).unwrap();
    goal_binding_report.validate().unwrap();
    let context = Phase0ValidationContext {
        schema: PHASE0_CONTEXT_SCHEMA.to_owned(),
        unadapted_tree_path: campaign
            .join("unadapted-crate")
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        candidate_tree_path: campaign
            .join("crate")
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        goal_targets: goal_binding_report
            .targets
            .iter()
            .map(|target| target.target_id.clone())
            .collect(),
        ledger_path: work.join("ledger.json").to_string_lossy().into_owned(),
        receipt_store_path: work.join("receipts").to_string_lossy().into_owned(),
        goal_binding_report_path: binding_path.to_string_lossy().into_owned(),
    };
    fs::create_dir(work.join("receipts")).unwrap();
    let mut state = Phase0State::initialize(genesis.clone(), ledger.clone(), &context).unwrap();
    let candidate = checker_receipt(
        &genesis,
        &runner,
        work,
        &ledger,
        source_manifest.source_tree_sha256,
        "candidate",
        1,
        &determinism["generated_lean_sha256"],
        &determinism["tablet_root_sha256"],
    );
    let replay = checker_receipt(
        &genesis,
        &runner,
        work,
        &ledger,
        source_manifest.source_tree_sha256,
        "ratification_replay",
        2,
        &determinism["generated_lean_sha256"],
        &determinism["tablet_root_sha256"],
    );
    state = state
        .apply(
            &context,
            Phase0Event::CheckComplete {
                candidate_success_receipt: candidate,
                final_success_receipt: replay,
                ablation_receipts: Vec::new(),
                completed_ledger: ledger,
                goal_binding_report,
                extracted_model_path: work
                    .join("identity-ratification_replay-2/extraction-writable/extracted")
                    .to_str().unwrap().to_owned(),
            },
        )
        .unwrap();
    for _ in 0..2 {
        let request = match state.next_request(&context).unwrap() {
            Phase0NextRequest::Audit(request) => request,
            other => panic!("unexpected Phase-0 request: {other:?}"),
        };
        let result = audit_pass(&request);
        state = state
            .apply(&context, Phase0Event::AuditResult { request, result })
            .unwrap();
    }
    let (_, sealed) = state.finalize(&context).unwrap();
    let phase0 = campaign.join(".trellis/phase0");
    fs::create_dir_all(&phase0).unwrap();
    for (name, value) in [
        ("GENESIS.json", serde_json::to_value(&genesis).unwrap()),
        ("SEALED_GENERATION.json", serde_json::to_value(&sealed).unwrap()),
        ("BUNDLE.json", serde_json::to_value(&sealed.semantic_bundle).unwrap()),
        (
            "ADAPTATION_LEDGER.json",
            serde_json::to_value(&sealed.semantic_bundle.ledger).unwrap(),
        ),
        (
            "GOAL_BINDING_REPORT.json",
            serde_json::to_value(&sealed.semantic_bundle.goal_binding_report).unwrap(),
        ),
        (
            "SOURCE_PARTITION_MANIFEST.json",
            serde_json::to_value(&sealed.semantic_bundle.source_partition_manifest).unwrap(),
        ),
    ] {
        fs::write(phase0.join(name), canonical_json_value(&value).unwrap()).unwrap();
    }
    let production = verify_phase0_production(campaign, &genesis, &sealed).unwrap();
    fs::write(
        phase0.join("PRODUCTION_RESULT.json"),
        canonical_json_value(&serde_json::to_value(&production).unwrap()).unwrap(),
    )
    .unwrap();
    (phase0.join("GENESIS.json"), phase0.join("SEALED_GENERATION.json"))
}

/// Runs the installed extraction tools and the separately compiled, non-test
/// bootstrap CLI. This deliberately bypasses none of trusted-platform or
/// evidence-closure capture: it guards dependencies that cfg(test) fixtures
/// cannot exercise.
#[test]
fn real_bootstrap_registers_and_closes_the_artifact_runner_leaf() {
    let manifest_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let trellis_root = manifest_root.parent().unwrap();
    let fixture = manifest_root.join("tests/fixtures/pv_generic_source");
    let test_scratch = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_root.join("target"));
    let work = test_scratch
        .join("real-bootstrap-artifact-evidence")
        .join(std::process::id().to_string());
    if work.exists() {
        fs::remove_dir_all(&work).unwrap();
    }
    fs::create_dir_all(&work).unwrap();
    let extracted = work.join("extracted");
    let extraction_target = work.join("extraction-target");
    let extraction_work = work.join("extraction-work");
    let profile_root = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let filespec_checker = profile_root.join("examples/scan_tablet_filespec");
    let prescribed_region_checker = profile_root.join("examples/print_prescribed_region");
    assert!(filespec_checker.is_file(), "Cargo did not build the FILESPEC example");
    assert!(
        prescribed_region_checker.is_file(),
        "Cargo did not build the prescribed-region example"
    );
    checked(
        "real gated extraction",
        Command::new("python3")
            .current_dir(trellis_root)
            .arg(trellis_root.join("scripts/extract_pv_model.py"))
            .arg("--crate")
            .arg(&fixture)
            .arg("--out")
            .arg(&extracted)
            .arg("--cargo-target-dir")
            .arg(&extraction_target)
            .arg("--work-root")
            .arg(&extraction_work)
            .arg("--filespec-checker")
            .arg(&filespec_checker)
            .arg("--prescribed-region-checker")
            .arg(&prescribed_region_checker)
            .arg("--gate")
            .output()
            .unwrap(),
    );

    let campaign = work.join("campaign");
    fs::create_dir(&campaign).unwrap();
    copy_tree(&fixture, &campaign.join("crate"));
    copy_tree(&fixture, &campaign.join("unadapted-crate"));
    fs::copy(fixture.join("GOAL.md"), campaign.join("GOAL.md")).unwrap();
    copy_tree(&extracted.join("Tablet"), &campaign.join("Tablet"));
    copy_tree(
        &extracted.join("extraction-evidence"),
        &campaign.join("extraction-evidence"),
    );
    fs::copy(
        extracted.join("tcb_manifest.json"),
        campaign.join("tcb_manifest.json"),
    )
    .unwrap();
    let mut pv_tablet: Value = serde_json::from_slice(
        &fs::read(extracted.join("pv_tablet.json")).unwrap(),
    )
    .unwrap();
    pv_tablet.as_object_mut().unwrap().insert(
        "verification_targets".into(),
        json!([
            {"name":"add_offset","resolution":"decide"},
            {"name":"select_larger","resolution":"decide"},
            {"name":"mutate","resolution":"decide"}
        ]),
    );
    fs::write(
        campaign.join("pv_tablet.json"),
        serde_json::to_vec_pretty(&pv_tablet).unwrap(),
    )
    .unwrap();
    let tcb: Value = serde_json::from_slice(
        &fs::read(campaign.join("tcb_manifest.json")).unwrap(),
    )
    .unwrap();
    let approved: Vec<String> = tcb["global"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| format!("pv_generic_fixture.{}", name.as_str().unwrap()))
        .collect();
    fs::write(
        campaign.join("APPROVED_AXIOMS.json"),
        serde_json::to_vec_pretty(&json!({"global": approved, "nodes": {}})).unwrap(),
    )
    .unwrap();
    fs::write(
        campaign.join("lean-toolchain"),
        b"leanprover/lean4:v4.30.0-rc2\n",
    )
    .unwrap();
    fs::write(
        campaign.join("lakefile.lean"),
        b"import Lake\nopen Lake DSL\npackage `bootstrap_fixture`\n@[default_target] lean_lib `Tablet` where\n  srcDir := \".\"\n",
    )
    .unwrap();

    // The trusted-platform check requires at least one exact, clean git Lake
    // dependency. A local inert package exercises that check without network.
    let dependency = campaign.join(".lake/packages/bootstrap_support");
    fs::create_dir_all(&dependency).unwrap();
    checked("git init", git(&dependency, &["init", "-q"]));
    fs::write(
        dependency.join("lakefile.lean"),
        b"import Lake\nopen Lake DSL\npackage `bootstrap_support`\n",
    )
    .unwrap();
    checked("git add", git(&dependency, &["add", "lakefile.lean"]));
    checked(
        "git commit",
        git(
            &dependency,
            &[
                "-c",
                "user.name=Trellis Test",
                "-c",
                "user.email=trellis@example.invalid",
                "commit",
                "-q",
                "-m",
                "fixture",
            ],
        ),
    );
    checked(
        "git remote",
        git(
            &dependency,
            &["remote", "add", "origin", "https://example.invalid/bootstrap_support"],
        ),
    );
    let revision = String::from_utf8(checked("git rev-parse", git(&dependency, &["rev-parse", "HEAD"])).stdout)
        .unwrap()
        .trim()
        .to_owned();
    fs::write(
        campaign.join("lake-manifest.json"),
        serde_json::to_vec(&json!({
            "version": "1.2.0",
            "packagesDir": ".lake/packages",
            "packages": [{
                "name": "bootstrap_support",
                "type": "git",
                "url": "https://example.invalid/bootstrap_support",
                "rev": revision,
                "subDir": null,
                "configFile": "lakefile.lean",
                "manifestFile": "lake-manifest.json"
            }],
            "name": "bootstrap_fixture",
            "lakeDir": ".lake",
            "fixedToolchain": false
        }))
        .unwrap(),
    )
    .unwrap();
    let config = json!({
        "repo_path": campaign.canonicalize().unwrap(),
        "state_dir": ".trellis",
        "goal_file": "GOAL.md",
        "workflow": {
            "default_target": "lean",
            "approved_axioms_path": "APPROVED_AXIOMS.json"
        },
        "pv_tablet": pv_tablet
    });
    fs::write(
        campaign.join("trellis.config.json"),
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .unwrap();

    let (genesis, sealed_generation) =
        seal_identity_phase0(trellis_root, &campaign, &work);

    let plan = work.join("TRUST_PLAN.json");
    checked(
        "trust plan",
        Command::new("python3")
            .current_dir(trellis_root)
            .arg(trellis_root.join("scripts/build_pv_trust_plan.py"))
            .arg("--prose-targets")
            .arg(campaign.join("pv_tablet.json"))
            .arg("--sealed-generation")
            .arg(&sealed_generation)
            .arg("--goal-binding-report")
            .arg(campaign.join(".trellis/phase0/GOAL_BINDING_REPORT.json"))
            .arg("--out")
            .arg(&plan)
            .output()
            .unwrap(),
    );
    let authority = work.join("authority");
    let trust_cli = PathBuf::from(env!("CARGO_BIN_EXE_trellis_trust_cli"));
    checked(
        "real bootstrap-campaign",
        Command::new(&trust_cli)
            .current_dir(trellis_root)
            .arg("bootstrap-campaign")
            .arg(&campaign)
            .arg(&plan)
            .arg(&trust_cli)
            .arg(&authority)
            .arg("generic-bootstrap-artifact-test")
            .arg("generic-bootstrap-artifact-seed")
            .arg(&genesis)
            .arg(&sealed_generation)
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_BUILD_TARGET")
            .output()
            .unwrap(),
    );

    let evidence_root = authority.join("evidence-root");
    let manifest_bytes = fs::read(authority.join("EVIDENCE_TOOL_MANIFEST.json")).unwrap();
    let closure = verify_evidence_tool_manifest(&evidence_root, &manifest_bytes).unwrap();
    let runner = closure
        .leaves_by_logical_id
        .get(RUST_WITNESS_RUNNER_LOGICAL_ID)
        .expect("real evidence closure omitted the Rust witness runner");
    assert_eq!(
        runner.dependency_ids,
        [
            "trellis-runtime-source:trellis/config.py",
            "trellis-runtime-source:trellis/sandbox.py",
        ]
    );
    let manifest = parse_json_strict(&manifest_bytes).unwrap();
    let receipt = manifest["leaves"]
        .as_array()
        .unwrap()
        .iter()
        .find(|leaf| {
            leaf["logical_id"].as_str() == Some("trellis-runtime-kernel-build-receipt")
        })
        .unwrap();
    assert!(receipt["dependency_ids"]
        .as_array()
        .unwrap()
        .iter()
        .any(|dependency| dependency.as_str() == Some(RUST_WITNESS_RUNNER_LOGICAL_ID)));

    // This fixture contains complete toolchain and evidence trees and was
    // previously leaving roughly 11 GiB per successful run. Keep it on any
    // panic for diagnosis; a fully successful run owns and removes its PID
    // directory here.
    fs::remove_dir_all(&work).unwrap();
}
