#![recursion_limit = "256"]

use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::process::Command;
use std::time::Duration;
use trellis_kernel::trust_base::{
    execute_phase0_checked_json, raw_sha256, self_digest, source_tree_manifest,
    validate_phase0_execution_receipt,
    DomainTag, ExecutionLimits, Phase0ExecutionRequest, PinnedExecutionRequest, Sha256Digest,
};
use trellis_kernel::{git_checkout_source_digest, readonly_closure_digest};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
#[test]
fn genesis_bound_receipt_rejects_one_changed_output_byte() {
    let temp = tempfile::tempdir().unwrap();
    let runner = temp.path().join("runner");
    fs::write(&runner, b"#!/bin/sh\nprintf '{\"ok\":true}'\n").unwrap();
    fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).unwrap();
    let runner_sha = raw_sha256(&fs::read(&runner).unwrap());
    let genesis = raw_sha256(b"phase0 genesis");
    let receipt = execute_phase0_checked_json(Phase0ExecutionRequest {
        phase0_genesis_sha256: genesis,
        purpose: "source_adaptation_checker",
        execution: PinnedExecutionRequest {
            runner_path: &runner,
            expected_runner_sha256: runner_sha,
            command_id: "phase0-check",
            working_directory: temp.path(),
            environment: &BTreeMap::new(),
            input: &serde_json::json!({"candidate": "fixture"}),
            limits: ExecutionLimits {
                timeout: Duration::from_secs(5),
                max_stdin_bytes: 4096,
                max_stdout_bytes: 4096,
                max_stderr_bytes: 4096,
            },
        },
    })
    .unwrap();
    validate_phase0_execution_receipt(
        receipt.value(),
        genesis,
        "source_adaptation_checker",
        runner_sha,
    )
    .unwrap();

    let mut changed = receipt.value().clone();
    changed["stdout_base64"] = Value::String("eyJvayI6bnVsbH0=".to_owned());
    changed["parsed_stdout"] = serde_json::json!({"ok": null});
    let changed_receipt = self_digest(DomainTag::RawArtifact, &changed, "receipt_sha256").unwrap();
    changed["receipt_sha256"] = Value::String(changed_receipt.to_string());
    assert!(validate_phase0_execution_receipt(
        &changed,
        genesis,
        "source_adaptation_checker",
        runner_sha,
    )
    .is_err());
}

#[cfg(unix)]
#[test]
fn zero_genesis_cannot_create_a_phase0_receipt() {
    let temp = tempfile::tempdir().unwrap();
    let runner = temp.path().join("runner");
    fs::write(&runner, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).unwrap();
    let error = execute_phase0_checked_json(Phase0ExecutionRequest {
        phase0_genesis_sha256: Sha256Digest::ZERO,
        purpose: "source_adaptation_checker",
        execution: PinnedExecutionRequest {
            runner_path: &runner,
            expected_runner_sha256: raw_sha256(&fs::read(&runner).unwrap()),
            command_id: "phase0-check",
            working_directory: temp.path(),
            environment: &BTreeMap::new(),
            input: &serde_json::json!({}),
            limits: ExecutionLimits::default(),
        },
    })
    .unwrap_err();
    assert_eq!(error.code, "phase0_execution_genesis_invalid");
}

#[cfg(unix)]
#[test]
fn truncated_stderr_cannot_leave_citable_parsed_output() {
    let temp = tempfile::tempdir().unwrap();
    let runner = temp.path().join("runner");
    fs::write(
        &runner,
        b"#!/bin/sh\ncat >/dev/null\nprintf '{\"status\":\"failed\"}'\nprintf 'diagnostic-too-long' >&2\n",
    )
    .unwrap();
    fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).unwrap();
    let runner_sha = raw_sha256(&fs::read(&runner).unwrap());
    let genesis = raw_sha256(b"phase0 genesis");
    let receipt = execute_phase0_checked_json(Phase0ExecutionRequest {
        phase0_genesis_sha256: genesis,
        purpose: "source_adaptation_checker",
        execution: PinnedExecutionRequest {
            runner_path: &runner,
            expected_runner_sha256: runner_sha,
            command_id: "phase0-check",
            working_directory: temp.path(),
            environment: &BTreeMap::new(),
            input: &serde_json::json!({"candidate": "fixture"}),
            limits: ExecutionLimits {
                timeout: Duration::from_secs(5),
                max_stdin_bytes: 4096,
                max_stdout_bytes: 4096,
                max_stderr_bytes: 4,
            },
        },
    })
    .unwrap();
    assert_eq!(receipt.value()["stderr_truncated"], Value::Bool(true));
    assert_eq!(receipt.value()["parsed_stdout"], Value::Null);
    validate_phase0_execution_receipt(
        receipt.value(),
        genesis,
        "source_adaptation_checker",
        runner_sha,
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn trust_cli_brokers_a_genesis_bound_phase0_receipt() {
    let temp = tempfile::tempdir().unwrap();
    let runner = temp.path().join("runner");
    fs::write(&runner, b"#!/bin/sh\ncat >/dev/null\nprintf '{\"status\":\"failed\"}'\n").unwrap();
    fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).unwrap();
    let runner_sha = raw_sha256(&fs::read(&runner).unwrap());
    let genesis = raw_sha256(b"phase0 genesis");
    let request = serde_json::json!({
        "limits": {"stderr_bytes": 4096, "stdout_bytes": 4096, "timeout_seconds": 5},
        "phase0_genesis_sha256": genesis,
        "repository_root": temp.path(),
        "runner_sha256": runner_sha,
    });
    let request_path = temp.path().join("request.json");
    fs::write(
        &request_path,
        trellis_kernel::trust_base::canonical_json_value(&request).unwrap(),
    )
    .unwrap();
    let receipt_path = temp.path().join("receipt.json");
    let status = Command::new(env!("CARGO_BIN_EXE_trellis_trust_cli"))
        .arg("execute-phase0-checker")
        .arg(&request_path)
        .arg(&runner)
        .arg(temp.path())
        .arg(&receipt_path)
        .status()
        .unwrap();
    assert!(status.success());
    let receipt: Value = serde_json::from_slice(&fs::read(receipt_path).unwrap()).unwrap();
    validate_phase0_execution_receipt(
        &receipt,
        genesis,
        "source_adaptation_checker",
        runner_sha,
    )
    .unwrap();
    assert_eq!(receipt["parsed_stdout"], serde_json::json!({"status": "failed"}));
}

#[cfg(unix)]
#[test]
fn trust_cli_runs_the_real_phase0_checker_with_pinned_repository_pythonpath() {
    let temp = tempfile::tempdir().unwrap();
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().canonicalize().unwrap();
    let runner = repository.join("scripts/pv_phase0_checker_runner.py");
    let extractor = repository.join("scripts/extract_pv_model.py");
    let toolchain = temp.path().join("toolchain");
    fs::create_dir_all(toolchain.join("bin")).unwrap();
    for name in ["cargo", "rustc"] {
        let path = toolchain.join("bin").join(name);
        fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let checkout = |name: &str| {
        let root = temp.path().join(name);
        fs::create_dir_all(root.join("bin")).unwrap();
        let executable = root.join("bin").join(name);
        fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "fixture@example.invalid"],
            vec!["config", "user.name", "Fixture"],
            vec!["add", "."],
            vec!["commit", "-q", "-m", "fixture"],
        ] {
            assert!(Command::new("git").arg("-C").arg(&root).args(args).status().unwrap().success());
        }
        let revision = String::from_utf8(
            Command::new("git").arg("-C").arg(&root).args(["rev-parse", "HEAD"])
                .output().unwrap().stdout,
        ).unwrap().trim().to_owned();
        let digest = git_checkout_source_digest(&root, &revision).unwrap();
        (root, executable, digest)
    };
    let (charon_root, charon, charon_root_sha) = checkout("charon");
    let (aeneas_root, aeneas, aeneas_root_sha) = checkout("aeneas");
    let opam = temp.path().join("opam");
    fs::create_dir_all(opam.join("bin")).unwrap();
    fs::write(opam.join("identity"), b"fixture\n").unwrap();
    let candidate = temp.path().join("candidate");
    fs::create_dir_all(candidate.join("src")).unwrap();
    fs::write(candidate.join("src/lib.rs"), b"pub fn f() {}\n").unwrap();
    let candidate_manifest = source_tree_manifest(&candidate).unwrap();
    let goal = temp.path().join("GOAL.md");
    fs::write(&goal, b"# fixture\n").unwrap();
    let output_root = temp.path().join("output");
    fs::create_dir(&output_root).unwrap();
    let genesis = raw_sha256(b"real runner genesis");
    let cargo = toolchain.join("bin/cargo");
    let rustc = toolchain.join("bin/rustc");
    let request = serde_json::json!({
        "schema": "trellis-pv-phase0-checker-request/v1",
        "phase0_genesis_sha256": genesis,
        "check_id": "cli-real-runner",
        "check_kind": "candidate",
        "ledger_generation": 0,
        "ledger_root_sha256": raw_sha256(b"ledger"),
        "candidate_root": candidate,
        "candidate_manifest": candidate_manifest,
        "candidate_tree_sha256": candidate_manifest.source_tree_sha256,
        "repository_root": repository,
        "goal_path": goal,
        "goal_sha256": raw_sha256(b"# fixture\n"),
        "reverted_entry_id": null,
        "runner_path": runner,
        "runner_sha256": raw_sha256(&fs::read(&runner).unwrap()),
        "extractor_path": extractor,
        "extractor_sha256": raw_sha256(&fs::read(&extractor).unwrap()),
        "filespec_checker_path": runner,
        "filespec_checker_sha256": raw_sha256(&fs::read(&runner).unwrap()),
        "prescribed_region_checker_path": runner,
        "prescribed_region_checker_sha256": raw_sha256(&fs::read(&runner).unwrap()),
        "cargo_path": cargo,
        "cargo_sha256": raw_sha256(&fs::read(&cargo).unwrap()),
        "rustc_path": rustc,
        "rustc_sha256": raw_sha256(&fs::read(&rustc).unwrap()),
        "toolchain_root": toolchain,
        "charon_toolchain_root": toolchain,
        "charon_toolchain_tree_sha256": readonly_closure_digest(&toolchain).unwrap(),
        "charon_path": charon,
        "charon_sha256": raw_sha256(&fs::read(&charon).unwrap()),
        "charon_source_root": charon_root,
        "charon_source_tree_sha256": charon_root_sha,
        "aeneas_path": aeneas,
        "aeneas_sha256": raw_sha256(&fs::read(&aeneas).unwrap()),
        "aeneas_source_root": aeneas_root,
        "aeneas_source_tree_sha256": aeneas_root_sha,
        "dependency_cache_path": "",
        "dependency_cache_tree_sha256": raw_sha256(b""),
        "opam_switch_prefix": opam,
        "opam_switch_prefix_tree_sha256": readonly_closure_digest(&opam).unwrap(),
        "opam_switch_environment": {
            "OPAM_SWITCH_PREFIX": opam,
            "OPAMSWITCH": "fixture",
            "CAML_LD_LIBRARY_PATH": opam,
            "OCAML_TOPLEVEL_PATH": opam,
            "OCAMLTOP_INCLUDE_PATH": opam
        },
        "output_root": output_root,
        "output_root_binding_sha256": raw_sha256(output_root.to_string_lossy().as_bytes()),
        "target_triple": "x86_64-unknown-linux-gnu",
        "extraction_profile": "dev",
        "opam_switch": "fixture",
        "closed_environment": {"LANG": "C.UTF-8"},
        "run_id": "cli-real-runner",
        "limits": {
            "timeout_seconds": 5, "cpu_seconds": 5,
            "address_space_bytes": 1073741824, "file_size_bytes": 1048576,
            "process_count": 32, "stdout_bytes": 65536, "stderr_bytes": 65536
        }
    });
    let request_path = temp.path().join("real-request.json");
    fs::write(&request_path, trellis_kernel::trust_base::canonical_json_value(&request).unwrap()).unwrap();
    let receipt_path = temp.path().join("real-receipt.json");
    let status = Command::new(env!("CARGO_BIN_EXE_trellis_trust_cli"))
        .arg("execute-phase0-checker").arg(&request_path).arg(&runner)
        .arg(temp.path()).arg(&receipt_path).status().unwrap();
    assert!(status.success());
    let receipt: Value = serde_json::from_slice(&fs::read(receipt_path).unwrap()).unwrap();
    validate_phase0_execution_receipt(
        &receipt, genesis, "source_adaptation_checker", raw_sha256(&fs::read(&runner).unwrap()),
    ).unwrap();
    assert_eq!(receipt["exit_code"], 0);
    assert_eq!(receipt["parsed_stdout"]["first_failing_stage"], "standalone_package");
    assert_eq!(receipt["parsed_stdout"]["stages"][0]["status"], "failed");
}
