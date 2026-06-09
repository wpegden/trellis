use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

use serde_json::Value;
use sha2::{Digest, Sha256};
mod common;
use common::project_tempdir;

fn run_runtime_cli(input: &Value) -> std::process::Output {
    run_runtime_cli_with_env(input, &[])
}

fn run_runtime_cli_with_env(
    input: &Value,
    envs: &[(&str, &std::path::Path)],
) -> std::process::Output {
    let exe = env!("CARGO_BIN_EXE_trellis_runtime_cli");
    let mut command = Command::new(exe);
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn runtime cli");
    child
        .stdin
        .as_mut()
        .expect("stdin available")
        .write_all(input.to_string().as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for runtime cli")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn seed_support_script(repo: &std::path::Path) {
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        script_dir.join("check.py"),
        "#!/usr/bin/env python3\nimport json,sys\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}, sys.stdout)\n    sys.exit(0)\nif cmd == 'prepare-compiled-support':\n    json.dump({'returncode': 0, 'stdout': 'prepared', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    json.dump({'returncode': 0, 'stdout': 'materialized', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {cmd}')\n",
    )
    .expect("write support script");
    let mut perms = fs::metadata(script_dir.join("check.py"))
        .expect("metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(script_dir.join("check.py"), perms).expect("chmod support script");
}

fn write_runtime_cli_config(repo: &std::path::Path) -> std::path::PathBuf {
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
    .expect("write runtime cli config");
    config_path
}

#[test]
fn runtime_cli_normalize_worker_derives_structural_snapshot() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("Tablet")).expect("tablet dir");
    fs::write(
        repo.join("Tablet/Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("write preamble lean");
    fs::write(repo.join("Tablet/Preamble.tex"), "").expect("write preamble tex");
    fs::write(
        repo.join("Tablet/A.lean"),
        "import Tablet.Preamble\n\ntheorem A : True := by\n  sorry\n",
    )
    .expect("write A lean");
    fs::write(
        repo.join("Tablet/A.tex"),
        "\\begin{theorem}A\\end{theorem}\n\\begin{proof}TODO\\end{proof}\n",
    )
    .expect("write A tex");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "normalize_worker",
        "input": {
            "repo_path": repo,
            "configured_targets": ["t.a"],
            "current_target_claims": {"A": ["t.a"]},
            "target_claim_updates": {"A": ["t.a"]},
            "target_fingerprints": {"Preamble": "", "A": "corr-A"},
            "sound_current_fingerprints": {"Preamble": "", "A": "sound-A"}
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse normalize_worker output");
    assert_eq!(json["status"], "normalize_worker_ok");
    assert_eq!(
        json["output"]["snapshot"]["present_nodes"],
        serde_json::json!(["A", "Preamble"])
    );
    assert_eq!(
        json["output"]["snapshot"]["open_nodes"],
        serde_json::json!(["A"])
    );
    assert_eq!(
        json["output"]["snapshot"]["paper_current_fingerprints"],
        serde_json::json!({})
    );
    assert_eq!(
        json["output"]["dep_updates"]["A"],
        serde_json::json!({"Set": ["Preamble"]})
    );
}
#[test]
fn runtime_cli_normalizes_human_gate_payloads() {
    let ok_output = run_runtime_cli(&serde_json::json!({
        "action": "normalize_human_gate",
        "request_id": 13,
        "cycle": 6,
        "raw_payload_text": "{\"choice\":\"feedback\"}"
    }));
    assert_eq!(ok_output.status.code(), Some(0));
    let ok_json: Value =
        serde_json::from_slice(&ok_output.stdout).expect("parse normalize_human_gate output");
    assert_eq!(ok_json["status"], "normalize_human_gate_ok");
    assert_eq!(ok_json["output"]["kind"], "human_gate");
    assert_eq!(ok_json["output"]["status"], "Ok");
    assert_eq!(ok_json["output"]["choice"], "Feedback");

    let malformed_output = run_runtime_cli(&serde_json::json!({
        "action": "normalize_human_gate",
        "request_id": 14,
        "cycle": 7,
        "raw_payload_text": "{\"choice\":\"maybe\"}"
    }));
    assert_eq!(malformed_output.status.code(), Some(0));
    let malformed_json: Value =
        serde_json::from_slice(&malformed_output.stdout).expect("parse malformed human gate");
    assert_eq!(malformed_json["status"], "normalize_human_gate_ok");
    assert_eq!(malformed_json["output"]["kind"], "human_gate");
    assert_eq!(malformed_json["output"]["status"], "Malformed");
    assert_eq!(
        malformed_json["output"]["request_id"],
        serde_json::json!(14)
    );
}

#[test]
fn runtime_cli_validates_trellis_worker_result_payload() {
    let output = run_runtime_cli(&serde_json::json!({
        "action": "validate_trellis_worker_result",
        "raw_payload": {
            "outcome": "VaLiD",
            "summary": " Applied a small theorem repair. ",
            "comments": "",
            "semantic_dep_updates": {
                "main_node": ["dep_a", "dep_a", " dep_b "]
            },
            "target_claim_updates": {
                "main_node": ["target_1"]
            },
            "difficulty_updates": {
                "main_node": "hard"
            }
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value = serde_json::from_slice(&output.stdout).expect("parse validate worker output");
    assert_eq!(json["status"], "validate_trellis_worker_result_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(true));
    assert_eq!(
        json["output"]["data"]["outcome"],
        serde_json::json!("valid")
    );
    assert_eq!(
        json["output"]["data"]["semantic_dep_updates"]["main_node"],
        serde_json::json!(["dep_a", "dep_b"])
    );
    assert_eq!(
        json["output"]["data"]["target_claim_updates"]["main_node"],
        serde_json::json!(["target_1"])
    );
}

#[test]
fn runtime_cli_validates_worker_payload_against_cleanup_allowed_outcomes() {
    let output = run_runtime_cli(&serde_json::json!({
        "action": "validate_trellis_worker_result",
        "raw_payload": {
            "outcome": "stuck",
            "summary": "cleanup could not proceed",
            "comments": "",
            "semantic_dep_updates": {},
            "target_claim_updates": {},
            "difficulty_updates": {}
        },
        "acceptance_context": {
            "worker_acceptance": {
                "validation_kind": "cleanup"
            }
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value = serde_json::from_slice(&output.stdout).expect("parse validate worker output");
    assert_eq!(json["status"], "validate_trellis_worker_result_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(false));
    assert_eq!(
        json["output"]["errors"][0],
        serde_json::json!("outcome must be one of ['valid', 'invalid']")
    );
}

#[test]
fn runtime_cli_builds_malformed_worker_and_review_responses() {
    let worker_output = run_runtime_cli(&serde_json::json!({
        "action": "build_malformed_response",
        "kind": "worker",
        "request_id": 11,
        "cycle": 7
    }));
    assert_eq!(worker_output.status.code(), Some(0));
    let worker_json: Value =
        serde_json::from_slice(&worker_output.stdout).expect("parse malformed worker output");
    assert_eq!(worker_json["status"], "build_malformed_response_ok");
    assert_eq!(worker_json["output"]["kind"], "worker");
    assert_eq!(worker_json["output"]["status"], "Malformed");
    assert_eq!(worker_json["output"]["request_id"], serde_json::json!(11));

    let review_output = run_runtime_cli(&serde_json::json!({
        "action": "build_malformed_response",
        "kind": "review",
        "request_id": 12,
        "cycle": 8
    }));
    assert_eq!(review_output.status.code(), Some(0));
    let review_json: Value =
        serde_json::from_slice(&review_output.stdout).expect("parse malformed review output");
    assert_eq!(review_json["status"], "build_malformed_response_ok");
    assert_eq!(review_json["output"]["kind"], "review");
    assert_eq!(review_json["output"]["status"], "Malformed");
    assert_eq!(review_json["output"]["request_id"], serde_json::json!(12));
}

#[test]
fn runtime_cli_validates_trellis_reviewer_result_payload() {
    let output = run_runtime_cli(&serde_json::json!({
        "action": "validate_trellis_reviewer_result",
        "raw_payload": {
            "decision": "CONTINUE",
            "reason": "Need to keep working.",
            "task_blocker_ids": ["b1", "b1"],
            "reset_blocker_ids": [],
            "next_active": "main_node",
            "next_mode": "TARGETED",
            "reset": "NONE",
            "difficulty_updates": {
                "main_node": "hard"
            },
            "allow_new_obligations": true,
            "must_close_active": false,
            "clear_human_input": true
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse validate reviewer output");
    assert_eq!(json["status"], "validate_trellis_reviewer_result_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(true));
    assert_eq!(
        json["output"]["data"]["decision"],
        serde_json::json!("continue")
    );
    assert_eq!(
        json["output"]["data"]["next_mode"],
        serde_json::json!("targeted")
    );
    assert_eq!(json["output"]["data"]["reset"], serde_json::json!("none"));
    assert_eq!(
        json["output"]["data"]["task_blocker_ids"],
        serde_json::json!(["b1"])
    );
    assert_eq!(
        json["output"]["data"]["clear_human_input"],
        serde_json::json!(true)
    );
}

#[test]
fn runtime_cli_checks_node_via_repo_check_script() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("preamble tex");
    fs::write(
        tablet_dir.join("foo.lean"),
        "-- [TABLET NODE: foo]\ntheorem foo : True := by\n  trivial\n",
    )
    .expect("foo lean");
    fs::write(
        tablet_dir.join("foo.tex"),
        "\\begin{theorem}Foo\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
    )
    .expect("foo tex");
    fs::write(
        tablet_dir.join("bar.lean"),
        "-- [TABLET NODE: bar]\ntheorem bar : True := by\n  trivial\n",
    )
    .expect("bar lean");
    fs::write(
        tablet_dir.join("bar.tex"),
        "\\begin{theorem}Bar\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
    )
    .expect("bar tex");
    let script = script_dir.join("check.py");
    fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json
import sys

cmd = sys.argv[1]
if cmd == "sync-tablet-support":
    json.dump(
        {
            "updated_paths": ["Tablet/INDEX.md", "Tablet/README.md"],
            "header_tex_path": "Tablet/header.tex",
            "index_md_path": "Tablet/INDEX.md",
            "readme_md_path": "Tablet/README.md",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "prepare-compiled-support":
    json.dump(
        {
            "returncode": 0,
            "stdout": "prepared",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "materialize-tablet-oleans":
    json.dump(
        {
            "returncode": 0,
            "stdout": "materialized",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "lean-compile-node":
    node = sys.argv[2]
    if node == "bar":
        json.dump(
            {
                "returncode": 1,
                "stdout": "",
                "stderr": "out of scope failure",
                "timed_out": False,
                "spawn_error": "",
            },
            sys.stdout,
        )
        sys.exit(0)
    json.dump(
        {
            "returncode": 0,
            "stdout": "node build ok",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "print-axioms":
    json.dump(
        {
            "returncode": 0,
            "stdout": "foo does not depend on any axioms",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
raise SystemExit(f"unexpected command: {cmd}")
"#,
    )
    .expect("write check script");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "check_node",
        "repo_path": repo,
        "node_name": "foo"
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value = serde_json::from_slice(&output.stdout).expect("parse check_node output");
    assert_eq!(json["status"], "check_node_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(true));
    assert_eq!(json["output"]["compiles"], serde_json::json!(true));
    assert_eq!(json["output"]["axioms_valid"], serde_json::json!(true));
    assert_eq!(
        json["output"]["build_output"],
        serde_json::json!("node build ok")
    );
}

#[test]
fn runtime_cli_checks_tablet_via_repo_check_script() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("preamble tex");
    fs::write(
        tablet_dir.join("foo.lean"),
        "-- [TABLET NODE: foo]\ntheorem foo : True := by\n  trivial\n",
    )
    .expect("foo lean");
    fs::write(
        tablet_dir.join("foo.tex"),
        "\\begin{theorem}Foo\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
    )
    .expect("foo tex");
    let script = script_dir.join("check.py");
    fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json
import sys

cmd = sys.argv[1]
if cmd == "sync-tablet-support":
    json.dump(
        {
            "updated_paths": ["Tablet/INDEX.md", "Tablet/README.md"],
            "header_tex_path": "Tablet/header.tex",
            "index_md_path": "Tablet/INDEX.md",
            "readme_md_path": "Tablet/README.md",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "prepare-compiled-support":
    json.dump(
        {
            "returncode": 0,
            "stdout": "prepared",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "materialize-tablet-oleans":
    json.dump(
        {
            "returncode": 0,
            "stdout": "materialized",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "lean-compile-node":
    json.dump(
        {
            "returncode": 0,
            "stdout": "node build ok",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "print-axioms":
    json.dump(
        {
            "returncode": 0,
            "stdout": "foo does not depend on any axioms",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "lean-build-tablet":
    json.dump(
        {
            "returncode": 0,
            "stdout": "tablet build ok",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
raise SystemExit(f"unexpected command: {cmd}")
"#,
    )
    .expect("write check script");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "check_tablet",
        "repo_path": repo
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value = serde_json::from_slice(&output.stdout).expect("parse check_tablet output");
    assert_eq!(json["status"], "check_tablet_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(true));
    assert_eq!(
        json["output"]["nodes"]["foo"]["ok"],
        serde_json::json!(true)
    );
    assert_eq!(json["output"]["build_output"], serde_json::json!(""));
}

#[test]
fn runtime_cli_check_tablet_skips_axiom_audit_for_open_nodes() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("preamble tex");
    fs::write(
        tablet_dir.join("foo.lean"),
        "-- [TABLET NODE: foo]\ntheorem foo : True := by\n  sorry\n",
    )
    .expect("foo lean");
    fs::write(
        tablet_dir.join("foo.tex"),
        "\\begin{theorem}Foo\\end{theorem}\n\\begin{proof}TODO\\end{proof}\n",
    )
    .expect("foo tex");
    let script = script_dir.join("check.py");
    let command_log = repo.join("check_tablet_open_command.log");
    fs::write(
        &script,
        format!(
            r#"#!/usr/bin/env python3
import json
import sys
from pathlib import Path

cmd = sys.argv[1]
with Path({:?}).open("a", encoding="utf-8") as handle:
    handle.write(cmd + "\n")
if cmd == "sync-tablet-support":
    json.dump(
        {{
            "updated_paths": ["Tablet/INDEX.md", "Tablet/README.md"],
            "header_tex_path": "Tablet/header.tex",
            "index_md_path": "Tablet/INDEX.md",
            "readme_md_path": "Tablet/README.md",
        }},
        sys.stdout,
    )
    sys.exit(0)
if cmd == "prepare-compiled-support":
    json.dump(
        {{
            "returncode": 0,
            "stdout": "prepared",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }},
        sys.stdout,
    )
    sys.exit(0)
if cmd == "materialize-tablet-oleans":
    json.dump(
        {{
            "returncode": 0,
            "stdout": "materialized",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }},
        sys.stdout,
    )
    sys.exit(0)
if cmd == "lean-compile-node":
    json.dump(
        {{
            "returncode": 0,
            "stdout": "warning: declaration uses sorry\n",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }},
        sys.stdout,
    )
    sys.exit(0)
if cmd == "print-axioms":
    json.dump(
        {{
            "returncode": 0,
            "stdout": "foo does not depend on any axioms",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }},
        sys.stdout,
    )
    sys.exit(0)
if cmd == "lean-build-tablet":
    json.dump(
        {{
            "returncode": 0,
            "stdout": "tablet build ok",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }},
        sys.stdout,
    )
    sys.exit(0)
raise SystemExit(f"unexpected command: {{cmd}}")
"#,
            command_log.display().to_string()
        ),
    )
    .expect("write check script");
    let mut perms = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("script perms");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "check_tablet",
        "repo_path": repo
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value = serde_json::from_slice(&output.stdout).expect("parse check_tablet output");
    assert_eq!(json["status"], "check_tablet_ok");
    assert_eq!(
        json["output"]["nodes"]["foo"]["ok"],
        serde_json::json!(false)
    );
    let commands = fs::read_to_string(&command_log).expect("read command log");
    assert!(commands.contains("lean-compile-node"));
    assert!(!commands.contains("print-axioms"));
}

#[test]
fn runtime_cli_check_tablet_runs_axiom_audit_for_closed_nodes() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("preamble tex");
    fs::write(
        tablet_dir.join("foo.lean"),
        "-- [TABLET NODE: foo]\ndef foo : Nat := 0\n",
    )
    .expect("foo lean");
    fs::write(
        tablet_dir.join("foo.tex"),
        "\\begin{definition}Foo\\end{definition}\n",
    )
    .expect("foo tex");
    let script = script_dir.join("check.py");
    let command_log = repo.join("check_tablet_closed_command.log");
    fs::write(
        &script,
        format!(
            r#"#!/usr/bin/env python3
import json
import sys
from pathlib import Path

cmd = sys.argv[1]
with Path({:?}).open("a", encoding="utf-8") as handle:
    handle.write(cmd + "\n")
if cmd == "sync-tablet-support":
    json.dump(
        {{
            "updated_paths": ["Tablet/INDEX.md", "Tablet/README.md"],
            "header_tex_path": "Tablet/header.tex",
            "index_md_path": "Tablet/INDEX.md",
            "readme_md_path": "Tablet/README.md",
        }},
        sys.stdout,
    )
    sys.exit(0)
if cmd == "prepare-compiled-support":
    json.dump(
        {{
            "returncode": 0,
            "stdout": "prepared",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }},
        sys.stdout,
    )
    sys.exit(0)
if cmd == "materialize-tablet-oleans":
    json.dump(
        {{
            "returncode": 0,
            "stdout": "materialized",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }},
        sys.stdout,
    )
    sys.exit(0)
if cmd == "lean-compile-node":
    json.dump(
        {{
            "returncode": 0,
            "stdout": "node build ok",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }},
        sys.stdout,
    )
    sys.exit(0)
if cmd == "print-axioms":
    json.dump(
        {{
            "returncode": 0,
            "stdout": "foo does not depend on any axioms",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }},
        sys.stdout,
    )
    sys.exit(0)
if cmd == "lean-build-tablet":
    json.dump(
        {{
            "returncode": 0,
            "stdout": "tablet build ok",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }},
        sys.stdout,
    )
    sys.exit(0)
raise SystemExit(f"unexpected command: {{cmd}}")
"#,
            command_log.display().to_string()
        ),
    )
    .expect("write check script");
    let mut perms = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("script perms");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "check_tablet",
        "repo_path": repo
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value = serde_json::from_slice(&output.stdout).expect("parse check_tablet output");
    assert_eq!(json["status"], "check_tablet_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(true));
    let commands = fs::read_to_string(&command_log).expect("read command log");
    assert!(commands.contains("lean-compile-node"));
    assert!(commands.contains("print-axioms"));
}

#[test]
fn runtime_cli_check_tablet_rejects_textual_sorry_ax_even_with_imported_sorry_warning() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("preamble tex");
    fs::write(
        tablet_dir.join("foo.lean"),
        "-- [TABLET NODE: foo]\ntheorem foo : True := by\n  exact sorryAx\n",
    )
    .expect("foo lean");
    fs::write(
        tablet_dir.join("foo.tex"),
        "\\begin{theorem}Foo\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
    )
    .expect("foo tex");
    let script = script_dir.join("check.py");
    let command_log = repo.join("check_tablet_sorry_ax_command.log");
    fs::write(
        &script,
        format!(
            r#"#!/usr/bin/env python3
import json
import sys
from pathlib import Path

cmd = sys.argv[1]
with Path({:?}).open("a", encoding="utf-8") as handle:
    handle.write(cmd + "\n")
if cmd == "sync-tablet-support":
    json.dump(
        {{
            "updated_paths": ["Tablet/INDEX.md", "Tablet/README.md"],
            "header_tex_path": "Tablet/header.tex",
            "index_md_path": "Tablet/INDEX.md",
            "readme_md_path": "Tablet/README.md",
        }},
        sys.stdout,
    )
    sys.exit(0)
if cmd == "prepare-compiled-support":
    json.dump(
        {{
            "returncode": 0,
            "stdout": "prepared",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }},
        sys.stdout,
    )
    sys.exit(0)
if cmd == "materialize-tablet-oleans":
    json.dump(
        {{
            "returncode": 0,
            "stdout": "materialized",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }},
        sys.stdout,
    )
    sys.exit(0)
if cmd == "lean-compile-node":
    json.dump(
        {{
            "returncode": 0,
            "stdout": "warning: Tablet/Helper.lean:8:8: declaration uses 'sorry'\n",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }},
        sys.stdout,
    )
    sys.exit(0)
if cmd == "print-axioms":
    json.dump(
        {{
            "returncode": 0,
            "stdout": "foo depends on axioms: [sorryAx]",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }},
        sys.stdout,
    )
    sys.exit(0)
if cmd == "lean-build-tablet":
    json.dump(
        {{
            "returncode": 0,
            "stdout": "tablet build ok",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }},
        sys.stdout,
    )
    sys.exit(0)
raise SystemExit(f"unexpected command: {{cmd}}")
"#,
            command_log.display().to_string()
        ),
    )
    .expect("write check script");
    let mut perms = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("script perms");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "check_tablet",
        "repo_path": repo
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value = serde_json::from_slice(&output.stdout).expect("parse check_tablet output");
    assert_eq!(json["status"], "check_tablet_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(false));
    assert!(
        json["output"]["nodes"]["foo"]["errors"]
            .as_array()
            .expect("errors array")
            .iter()
            .any(|err| err
                .as_str()
                .unwrap_or("")
                .contains("sorryAx is forbidden, use sorry instead")),
        "expected worker-facing sorryAx checker error: {json}"
    );
    let commands = fs::read_to_string(&command_log).expect("read command log");
    assert!(commands.contains("lean-compile-node"));
    assert!(
        !commands.contains("print-axioms"),
        "textual `sorryAx` should be rejected directly before axiom audit"
    );
}

#[test]
fn runtime_cli_checks_tablet_scoped_via_repo_check_script() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("preamble tex");
    fs::write(
        tablet_dir.join("foo.lean"),
        "-- [TABLET NODE: foo]\ntheorem foo : True := by\n  trivial\n",
    )
    .expect("foo lean");
    fs::write(
        tablet_dir.join("foo.tex"),
        "\\begin{theorem}Foo\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
    )
    .expect("foo tex");
    let script = script_dir.join("check.py");
    fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json
import sys

cmd = sys.argv[1]
if cmd == "sync-tablet-support":
    json.dump(
        {
            "updated_paths": ["Tablet/INDEX.md", "Tablet/README.md"],
            "header_tex_path": "Tablet/header.tex",
            "index_md_path": "Tablet/INDEX.md",
            "readme_md_path": "Tablet/README.md",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "prepare-compiled-support":
    json.dump(
        {
            "returncode": 0,
            "stdout": "prepared",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "materialize-tablet-oleans":
    json.dump(
        {
            "returncode": 0,
            "stdout": "materialized",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "lean-compile-node":
    json.dump(
        {
            "returncode": 0,
            "stdout": "node build ok",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "print-axioms":
    json.dump(
        {
            "returncode": 0,
            "stdout": "foo does not depend on any axioms",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "lean-build-tablet":
    json.dump(
        {
            "returncode": 0,
            "stdout": "tablet build ok",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
raise SystemExit(f"unexpected command: {cmd}")
"#,
    )
    .expect("write check script");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "check_tablet_scoped",
        "repo_path": repo,
        "baseline_errors": [],
        "allowed_nodes": ["foo"]
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse check_tablet_scoped output");
    assert_eq!(json["status"], "check_tablet_scoped_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(true));
    assert_eq!(json["output"]["allowed_nodes"], serde_json::json!(["foo"]));
}

#[test]
fn runtime_cli_validates_correspondence_result_payload() {
    let output = run_runtime_cli(&serde_json::json!({
        "action": "validate_correspondence_result",
        "raw_payload": {
            "correspondence": {
                "decision": "FAIL",
                "verdicts": [
                    {"node": "main_node", "verdict": "Fail", "comment": "Still mismatched."}
                ]
            },
            "overall": "REJECT",
            "summary": "Paper mismatch remains",
            "comments": "Need a better excerpt."
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse validate correspondence output");
    assert_eq!(json["status"], "validate_correspondence_result_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(true));
    assert_eq!(
        json["output"]["data"]["correspondence"]["decision"],
        serde_json::json!("FAIL")
    );
    assert_eq!(
        json["output"]["data"]["overall"],
        serde_json::json!("REJECT")
    );
    // Verdicts are echoed verbatim (substantiveness-shaped per-node accountability).
    assert_eq!(
        json["output"]["data"]["correspondence"]["verdicts"][0]["node"],
        serde_json::json!("main_node")
    );
    assert_eq!(
        json["output"]["data"]["correspondence"]["verdicts"][0]["verdict"],
        serde_json::json!("Fail")
    );
}

#[test]
fn runtime_cli_rejects_correspondence_result_with_legacy_issues_field() {
    // Regression: the corr-node lane now requires `verdicts[]`; the legacy
    // `issues[]` shape (used by paper-faithfulness target lane) must be
    // rejected so a stale prompt fragment doesn't sneak silent-passes
    // through.
    let output = run_runtime_cli(&serde_json::json!({
        "action": "validate_correspondence_result",
        "raw_payload": {
            "correspondence": {
                "decision": "FAIL",
                "issues": [
                    {"node": "main_node", "description": "Stale shape, should reject."}
                ]
            },
            "overall": "REJECT",
            "summary": "Stale schema",
            "comments": ""
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse validate correspondence output");
    // The validator still returns ok=false (a structural failure), not a
    // process error.
    assert_eq!(json["status"], "validate_correspondence_result_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(false));
    let errors = json["output"]["errors"]
        .as_array()
        .expect("errors must be a list");
    // The legacy `issues[]` field is silently dropped by serde (verdicts
    // defaults to []), so the validator sees decision=FAIL with no Fail
    // verdicts and rejects with the consistency-check message.
    let any_decision_error = errors.iter().any(|e| {
        e.as_str()
            .is_some_and(|s| s.contains("PASS when no verdict is Fail"))
    });
    assert!(
        any_decision_error,
        "expected lane-decision-consistency error, got: {errors:?}"
    );
}

#[test]
fn runtime_cli_validates_paper_faithfulness_result_payload() {
    let output = run_runtime_cli(&serde_json::json!({
        "action": "validate_paper_faithfulness_result",
        "raw_payload": {
            "paper_faithfulness": {
                "decision": "FAIL",
                "issues": [
                    {"node": "main_result", "description": "Still mismatched."}
                ]
            },
            "overall": "REJECT",
            "summary": "Paper mismatch remains",
            "comments": "Need a better excerpt."
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value = serde_json::from_slice(&output.stdout).expect("parse validate paper output");
    assert_eq!(json["status"], "validate_paper_faithfulness_result_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(true));
    assert_eq!(
        json["output"]["data"]["paper_faithfulness"]["decision"],
        serde_json::json!("FAIL")
    );
    assert_eq!(
        json["output"]["data"]["overall"],
        serde_json::json!("REJECT")
    );
}

#[test]
fn runtime_cli_rejects_soundness_result_for_wrong_node_name() {
    let output = run_runtime_cli(&serde_json::json!({
        "action": "validate_soundness_result",
        "node_name": "foo",
        "raw_payload": {
            "node": "bar",
            "soundness": {
                "decision": "SOUND",
                "explanation": "fine"
            },
            "overall": "APPROVE",
            "summary": "ok",
            "comments": ""
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse validate soundness output");
    assert_eq!(json["status"], "validate_soundness_result_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(false));
    assert!(json["output"]["errors"]
        .as_array()
        .is_some_and(|errs| errs.iter().any(|err| err
            .as_str()
            .is_some_and(|msg| msg.contains("node must equal foo")))));
}
#[test]
fn runtime_cli_sync_tablet_support_combines_python_support_and_root_sync() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    let argv_log = repo.join("sync-argv.json");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("write preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("write preamble tex");
    fs::write(
        tablet_dir.join("B.lean"),
        "import Tablet.Preamble\n\ndef B : Nat := 0\n",
    )
    .expect("write B lean");
    fs::write(
        tablet_dir.join("B.tex"),
        "\\begin{definition}B\\end{definition}\n",
    )
    .expect("write B tex");
    let script = script_dir.join("check.py");
    fs::write(
        &script,
        format!(
            "#!/usr/bin/env python3\nimport json,sys\nfrom pathlib import Path\nPath({:?}).write_text(json.dumps(sys.argv[1:]), encoding='utf-8')\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({{'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {{cmd}}')\n",
            argv_log.display().to_string()
        ),
    )
    .expect("write check script");
    let mut perms = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("script perms");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "sync_tablet_support",
        "repo_path": repo,
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse sync_tablet_support output");
    assert_eq!(json["status"], "sync_tablet_support_ok");
    assert_eq!(
        fs::read_to_string(repo.join("Tablet.lean")).expect("read Tablet.lean"),
        "-- Auto-generated by .trellis. Do not edit.\nimport Tablet.B\nimport Tablet.Preamble\n"
    );
    assert_eq!(
        json["output"]["support"]["index_md_path"],
        serde_json::json!("Tablet/INDEX.md")
    );
    assert_eq!(
        json["output"]["root"]["node_names"],
        serde_json::json!(["B", "Preamble"])
    );
    let argv: Value = serde_json::from_str(&fs::read_to_string(&argv_log).expect("read argv log"))
        .expect("parse argv log");
    assert_eq!(argv[0], serde_json::json!("sync-tablet-support"));
    assert_eq!(argv[1], serde_json::json!(repo.display().to_string()));
    assert_eq!(argv[2], serde_json::json!("--render-json"));
    // Render payload is piped via stdin (`-` sentinel) so ARG_MAX cannot bite
    // when the rendered tablet INDEX/README JSON grows past ~100 KB.
    assert_eq!(argv[3], serde_json::json!("-"));
}
#[test]
fn runtime_cli_check_worker_result_turns_validation_execution_failure_into_invalid_output() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("write preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("write preamble tex");
    fs::write(
        tablet_dir.join("Gnp.lean"),
        "import Tablet.Preamble\n\nnoncomputable def Gnp : Nat := by\n  sorry\n",
    )
    .expect("write bad Gnp lean");
    fs::write(
        tablet_dir.join("Gnp.tex"),
        "\\begin{definition}Bad node.\\end{definition}\n",
    )
    .expect("write bad Gnp tex");
    let script = script_dir.join("check.py");
    fs::write(
        &script,
        "#!/usr/bin/env python3\nimport json,sys\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    json.dump({'returncode': 1, 'stdout': '[Gnp]\\nTablet/Gnp.lean:4:2: error: synthetic compile failure', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {cmd}')\n",
    )
    .expect("write check script");
    let mut perms = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("script perms");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "check_trellis_worker_result",
        "repo_path": repo,
        "acceptance_context": {
            "request": {
                "id": 9,
                "cycle": 2,
                "kind": "worker",
                "current_node_kinds": {
                    "Preamble": "preamble"
                }
            },
            "validation_kind": "theorem_global",
            "worker_acceptance": {
                "validation_execution_plan": [
                    {"kind": "scoped_tablet", "allowed_nodes_mode": "AllPresent", "explicit_nodes": []}
                ],
                "forbid_tablet_changes_when_stuck": true
            },
            "active_node": "",
            "held_target": "",
            "authorized_nodes": ["Preamble"],
            "configured_targets": [],
            "current_present_nodes": ["Preamble"],
            "current_proof_nodes": [],
            "current_deps": {"Preamble": []},
            "current_semantic_deps": {"Preamble": []},
            "current_target_claims": {"Preamble": []},
            "repo_path": repo,
            "before_snapshot": {},
            "baseline_errors": [],
            "imports_before": [],
            "expected_active_hash": "",
            "baseline_declaration_hashes": {},
            "baseline_correspondence_hashes": {}
        },
        "raw_payload": {
            "outcome": "valid",
            "summary": "Attempted worker result.",
            "comments": "",
            "semantic_dep_updates": {"Gnp": []},
            "target_claim_updates": {"Gnp": []},
            "difficulty_updates": {}
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse check_trellis_worker_result output");
    assert_eq!(json["status"], "check_trellis_worker_result_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(false));
    assert_eq!(
        json["output"]["final_outcome"],
        serde_json::json!("invalid")
    );
    assert_eq!(
        json["output"]["response"]["outcome"],
        serde_json::json!("Invalid")
    );
    assert!(json["output"]["validation_errors"]
        .as_array()
        .is_some_and(|items| !items.is_empty()));
}

#[test]
fn runtime_cli_check_worker_result_scoped_tablet_reports_new_node_errors() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("write preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("write preamble tex");
    fs::write(
        tablet_dir.join("Gnp.lean"),
        "-- [TABLET NODE: Gnp]\ndef Gnp : Nat := by\n  exact Real.log 3\n",
    )
    .expect("write bad Gnp lean");
    fs::write(
        tablet_dir.join("Gnp.tex"),
        "\\begin{definition}Bad node.\\end{definition}\n",
    )
    .expect("write bad Gnp tex");
    let script = script_dir.join("check.py");
    fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json
import sys

cmd = sys.argv[1]
if cmd == "lean-compile-node":
    node = sys.argv[2]
    if node == "Gnp":
        json.dump(
            {
                "returncode": 1,
                "stdout": "",
                "stderr": "Tablet/Gnp.lean:3:8: error: unknown constant Real.log\n",
                "timed_out": False,
                "spawn_error": "",
            },
            sys.stdout,
        )
        sys.exit(0)
    json.dump(
        {
            "returncode": 0,
            "stdout": "node build ok",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "print-axioms":
    json.dump(
        {
            "returncode": 0,
            "stdout": "does not depend on any axioms",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
raise SystemExit(f"unexpected command: {cmd}")
"#,
    )
    .expect("write check script");
    let mut perms = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("script perms");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "check_trellis_worker_result",
        "repo_path": repo,
        "acceptance_context": {
            "request": {
                "id": 9,
                "cycle": 2,
                "kind": "worker",
                "current_node_kinds": {
                    "Preamble": "preamble"
                }
            },
            "validation_kind": "theorem_global",
            "worker_acceptance": {
                "validation_execution_plan": [
                    {"kind": "scoped_tablet", "allowed_nodes_mode": "AllPresent", "explicit_nodes": []}
                ],
                "forbid_tablet_changes_when_stuck": true
            },
            "active_node": "",
            "held_target": "",
            "authorized_nodes": ["Preamble"],
            "configured_targets": [],
            "current_present_nodes": ["Preamble"],
            "current_proof_nodes": [],
            "current_deps": {"Preamble": []},
            "current_semantic_deps": {"Preamble": []},
            "current_target_claims": {"Preamble": []},
            "repo_path": repo,
            "before_snapshot": {},
            "baseline_errors": [],
            "imports_before": [],
            "expected_active_hash": "",
            "baseline_declaration_hashes": {},
            "baseline_correspondence_hashes": {}
        },
        "raw_payload": {
            "outcome": "valid",
            "summary": "Attempted worker result.",
            "comments": "",
            "semantic_dep_updates": {"Gnp": []},
            "target_claim_updates": {"Gnp": []},
            "difficulty_updates": {}
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse check_trellis_worker_result output");
    assert_eq!(json["status"], "check_trellis_worker_result_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(false));
    assert_eq!(
        json["output"]["final_outcome"],
        serde_json::json!("invalid")
    );
    let step_errors = json["output"]["validation_step_results"][0]["errors"]
        .as_array()
        .expect("scoped tablet errors");
    assert!(step_errors.iter().any(|item| {
        item.as_str()
            .is_some_and(|text| text.contains("Gnp: Compilation failed"))
    }));
}

#[test]
fn runtime_cli_check_worker_result_hydrates_valid_outputs() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("write preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("write preamble tex");
    fs::write(
        tablet_dir.join("A.lean"),
        "import Tablet.Preamble\n\n-- [TABLET NODE: A]\ndef A : Nat :=\n-- BODY\n  0\n",
    )
    .expect("write A lean");
    fs::write(
        tablet_dir.join("A.tex"),
        "\\begin{definition}A\\end{definition}\n",
    )
    .expect("write A tex");
    let script = script_dir.join("check.py");
    let command_log = repo.join("worker_hydrate_commands.log");
    fs::write(
        &script,
        format!(
            "#!/usr/bin/env python3\nimport json,sys\nfrom pathlib import Path\ncmd = sys.argv[1]\nwith Path({:?}).open('a', encoding='utf-8') as handle:\n    handle.write(cmd + '\\n')\nif cmd == 'sync-tablet-support':\n    json.dump({{'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}}, sys.stdout)\n    sys.exit(0)\nif cmd == 'prepare-compiled-support':\n    json.dump({{'returncode': 0, 'stdout': 'prepared', 'stderr': '', 'timed_out': False, 'spawn_error': ''}}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    json.dump({{'returncode': 0, 'stdout': 'materialized', 'stderr': '', 'timed_out': False, 'spawn_error': ''}}, sys.stdout)\n    sys.exit(0)\nif cmd == 'lean-semantic-payloads':\n    json.dump({{'A': {{'ok': True, 'payload': 'root|A||const|Tablet.A|def|type=(const Nat)', 'error': ''}}}}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {{cmd}}')\n",
            command_log.display().to_string()
        ),
    )
    .expect("write check script");
    let mut perms = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("script perms");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "check_trellis_worker_result",
        "repo_path": repo,
        "acceptance_context": {
            "request": {
                "id": 13,
                "cycle": 7,
                "kind": "worker",
                "current_node_kinds": {
                    "Preamble": "preamble",
                    "A": "definition"
                }
            },
            "validation_kind": "theorem_global",
            "worker_acceptance": {
                "validation_execution_plan": [],
                "forbid_tablet_changes_when_stuck": true
            },
            "active_node": "",
            "held_target": "",
            "authorized_nodes": [],
            "configured_targets": ["thm:conn"],
            "current_present_nodes": ["Preamble", "A"],
            "current_proof_nodes": [],
            "current_deps": {"Preamble": [], "A": ["Preamble"]},
            "current_semantic_deps": {"Preamble": [], "A": ["Preamble"]},
            "current_target_claims": {"Preamble": [], "A": ["thm:conn"]},
            "current_paper_approved_fingerprints": {},
            "repo_path": repo,
            "before_snapshot": {},
            "baseline_errors": [],
            "imports_before": [],
            "expected_active_hash": "",
            "baseline_declaration_hashes": {},
            "baseline_correspondence_hashes": {}
        },
        "raw_payload": {
            "outcome": "valid",
            "summary": "Valid worker output.",
            "comments": "",
            "semantic_dep_updates": {},
            "target_claim_updates": {},
            "difficulty_updates": {}
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse check_trellis_worker_result output");
    assert_eq!(json["status"], "check_trellis_worker_result_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(true));
    assert_eq!(json["output"]["final_outcome"], serde_json::json!("valid"));
    assert_eq!(
        json["output"]["response"]["outcome"],
        serde_json::json!("Valid")
    );
    assert_eq!(
        json["output"]["response"]["summary"],
        serde_json::json!("Valid worker output.")
    );
    assert_eq!(
        json["output"]["response"]["comments"],
        serde_json::json!("")
    );
    assert!(
        json["output"]["response"]["snapshot"]["paper_current_fingerprints"]["thm:conn"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    let commands = fs::read_to_string(&command_log).expect("read command log");
    assert!(commands.contains("sync-tablet-support"));
    assert!(commands.contains("materialize-tablet-oleans"));
    assert!(!commands.contains("prepare-compiled-support"));
}

#[test]
fn runtime_cli_check_worker_result_fingerprints_new_helper_with_post_delta_kind() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("write preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("write preamble tex");
    fs::write(
        tablet_dir.join("Helper.lean"),
        "import Tablet.Preamble\n\n-- [TABLET NODE: Helper]\ntheorem Helper : True := by\n-- BODY\n  trivial\n",
    )
    .expect("write helper lean");
    fs::write(
        tablet_dir.join("Helper.tex"),
        "\\begin{helper}Helper claim.\\end{helper}\n\\begin{proof}Trivial.\\end{proof}\n",
    )
    .expect("write helper tex");
    let script = script_dir.join("check.py");
    fs::write(
        &script,
        "#!/usr/bin/env python3\nimport json,sys\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}, sys.stdout)\n    sys.exit(0)\nif cmd == 'prepare-compiled-support':\n    json.dump({'returncode': 0, 'stdout': 'prepared', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    json.dump({'returncode': 0, 'stdout': 'materialized', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nif cmd == 'lean-semantic-payloads':\n    json.dump({'Helper': {'ok': True, 'payload': 'root|Helper||const|Tablet.Helper|theorem|type=(const True)', 'error': ''}, 'Preamble': {'ok': False, 'payload': '', 'error': ''}}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {cmd}')\n",
    )
    .expect("write check script");
    let mut perms = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("script perms");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "check_trellis_worker_result",
        "repo_path": repo,
        "acceptance_context": {
            "request": {
                "id": 15,
                "cycle": 8,
                "kind": "worker",
                "current_node_kinds": {
                    "Preamble": "preamble"
                }
            },
            "validation_kind": "theorem_global",
            "worker_acceptance": {
                "validation_execution_plan": [],
                "forbid_tablet_changes_when_stuck": true
            },
            "active_node": "",
            "held_target": "",
            "authorized_nodes": [],
            "configured_targets": [],
            "current_present_nodes": ["Preamble"],
            "current_proof_nodes": [],
            "current_deps": {"Preamble": []},
            "current_semantic_deps": {"Preamble": []},
            "current_target_claims": {"Preamble": []},
            "current_paper_approved_fingerprints": {},
            "repo_path": repo,
            "before_snapshot": {},
            "baseline_errors": [],
            "imports_before": [],
            "expected_active_hash": "",
            "baseline_declaration_hashes": {},
            "baseline_correspondence_hashes": {}
        },
        "raw_payload": {
            "outcome": "valid",
            "summary": "Introduced helper.",
            "comments": "",
            "semantic_dep_updates": {},
            "target_claim_updates": {"Helper": []},
            "difficulty_updates": {}
        }
    }));
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse check_trellis_worker_result output");
    assert_eq!(json["status"], "check_trellis_worker_result_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(true));
    assert_eq!(
        json["output"]["response"]["node_kind_updates"]["Helper"],
        serde_json::json!({"Set": "Proof"})
    );
    assert_eq!(
        json["output"]["response"]["proof_node_updates"]["Helper"],
        serde_json::json!({"Set": true})
    );
    let fingerprint_raw = json["output"]["response"]["snapshot"]
        ["substantiveness_current_fingerprints"]["Helper"]
        .as_str()
        .expect("helper substantiveness fingerprint");
    let fingerprint: Value =
        serde_json::from_str(fingerprint_raw).expect("parse substantiveness fingerprint");
    assert_eq!(fingerprint["node_kind"], serde_json::json!("proof"));
}

#[test]
fn runtime_cli_check_worker_result_surfaces_hydration_failure_as_invalid() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("write preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("write preamble tex");
    let script = script_dir.join("check.py");
    fs::write(
        &script,
        "#!/usr/bin/env python3\nimport json,sys\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    json.dump({'returncode': 1, 'stdout': '', 'stderr': 'broken materialization', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {cmd}')\n",
    )
    .expect("write check script");
    let mut perms = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("script perms");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "check_trellis_worker_result",
        "repo_path": repo,
        "acceptance_context": {
            "request": {
                "id": 14,
                "cycle": 7,
                "kind": "worker",
                "current_node_kinds": {
                    "Preamble": "preamble"
                }
            },
            "validation_kind": "theorem_global",
            "worker_acceptance": {
                "validation_execution_plan": [],
                "forbid_tablet_changes_when_stuck": true
            },
            "active_node": "",
            "held_target": "",
            "authorized_nodes": [],
            "configured_targets": [],
            "current_present_nodes": ["Preamble"],
            "current_proof_nodes": [],
            "current_deps": {"Preamble": []},
            "current_semantic_deps": {"Preamble": []},
            "current_target_claims": {"Preamble": []},
            "current_paper_approved_fingerprints": {},
            "repo_path": repo,
            "before_snapshot": {},
            "baseline_errors": [],
            "imports_before": [],
            "expected_active_hash": "",
            "baseline_declaration_hashes": {},
            "baseline_correspondence_hashes": {}
        },
        "raw_payload": {
            "outcome": "valid",
            "summary": "Valid worker output.",
            "comments": "",
            "semantic_dep_updates": {},
            "target_claim_updates": {},
            "difficulty_updates": {}
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse check_trellis_worker_result output");
    assert_eq!(json["status"], "check_trellis_worker_result_ok");
    assert_eq!(json["output"]["ok"], serde_json::json!(false));
    assert_eq!(
        json["output"]["final_outcome"],
        serde_json::json!("invalid")
    );
    assert_eq!(
        json["output"]["response"]["outcome"],
        serde_json::json!("Invalid")
    );
    assert!(json["output"]["validation_errors"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item
            .as_str()
            .is_some_and(|msg| msg.contains("materialize-tablet-oleans failed")))));
    assert!(
        json["output"]["response"]["deterministic_rejection_reasons"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item
                .as_str()
                .is_some_and(|msg| msg.contains("materialize-tablet-oleans failed"))))
    );
}

#[test]
fn runtime_cli_hydrate_worker_response_syncs_worker_support_and_observes_live_nodes() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("write preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("write preamble tex");
    fs::write(
        tablet_dir.join("A.lean"),
        "import Tablet.Preamble\n\ndef A : Nat := 0\n",
    )
    .expect("write A lean");
    fs::write(
        tablet_dir.join("A.tex"),
        "\\begin{definition}A\\end{definition}\n",
    )
    .expect("write A tex");
    let script = script_dir.join("check.py");
    let command_log = repo.join("hydrate_worker_commands.log");
    fs::write(
        &script,
        format!(
            "#!/usr/bin/env python3\nimport json,sys\nfrom pathlib import Path\ncmd = sys.argv[1]\nwith Path({:?}).open('a', encoding='utf-8') as handle:\n    handle.write(cmd + '\\n')\nif cmd == 'lean-semantic-payloads':\n    json.dump({{'A': {{'ok': True, 'payload': 'root|A||const|Tablet.A|def|type=(const Nat)', 'error': ''}}, 'Preamble': {{'ok': False, 'payload': '', 'error': ''}}}}, sys.stdout)\n    sys.exit(0)\nif cmd == 'sync-tablet-support':\n    json.dump({{'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    json.dump({{'returncode': 0, 'stdout': 'materialized', 'stderr': '', 'timed_out': False, 'spawn_error': ''}}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {{cmd}}')\n",
            command_log.display().to_string()
        ),
    )
    .expect("write check script");
    let mut perms = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("script perms");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "hydrate_worker_response",
        "input": {
            "repo_path": repo,
            "configured_targets": [],
            "current_target_claims": {},
            "approved_paper_fingerprints": {},
            "response": {
                "request_id": 12,
                "cycle": 7,
                "status": "Ok",
                "outcome": "Invalid",
                "snapshot": {
                    "present_nodes": ["A", "Preamble"],
                    "open_nodes": [],
                    "coverage": {},
                    "target_fingerprints": {},
                    "corr_current_fingerprints": {},
                    "paper_current_fingerprints": {},
                    "sound_current_fingerprints": {}
                },
                "proof_node_updates": {},
                "node_kind_updates": {},
                "dep_updates": {},
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {}
            }
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse hydrate_worker_response output");
    assert_eq!(json["status"], "hydrate_worker_response_ok");
    assert!(
        json["output"]["response"]["snapshot"]["corr_current_fingerprints"]["A"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    let commands = fs::read_to_string(&command_log).expect("read command log");
    assert!(commands.contains("sync-tablet-support"));
    assert!(commands.contains("materialize-tablet-oleans"));
    assert!(!commands.contains("prepare-compiled-support"));
}

#[test]
fn runtime_cli_hydrate_worker_response_uses_post_delta_node_kind_for_substantiveness() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("write preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("write preamble tex");
    fs::write(
        tablet_dir.join("Helper.lean"),
        "import Tablet.Preamble\n\ntheorem Helper : True := by\n  trivial\n",
    )
    .expect("write helper lean");
    fs::write(
        tablet_dir.join("Helper.tex"),
        "\\begin{helper}Helper claim.\\end{helper}\n\\begin{proof}Trivial.\\end{proof}\n",
    )
    .expect("write helper tex");
    let script = script_dir.join("check.py");
    fs::write(
        &script,
        "#!/usr/bin/env python3\nimport json,sys\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    json.dump({'returncode': 0, 'stdout': 'materialized', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nif cmd == 'lean-semantic-payloads':\n    json.dump({'Helper': {'ok': True, 'payload': 'root|Helper||const|Tablet.Helper|theorem|type=(const True)', 'error': ''}, 'Preamble': {'ok': False, 'payload': '', 'error': ''}}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {cmd}')\n",
    )
    .expect("write check script");
    let mut perms = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("script perms");

    let hydrate = |current_node_kinds: Value, node_kind_updates: Value| -> Value {
        let output = run_runtime_cli(&serde_json::json!({
            "action": "hydrate_worker_response",
            "input": {
                "repo_path": repo.display().to_string(),
                "configured_targets": [],
                "current_target_claims": {"Preamble": []},
                "approved_paper_fingerprints": {},
                "current_node_kinds": current_node_kinds,
                "response": {
                    "request_id": 16,
                    "cycle": 8,
                    "status": "Ok",
                    "outcome": "Valid",
                    "snapshot": {
                        "present_nodes": ["Helper", "Preamble"],
                        "open_nodes": [],
                        "coverage": {},
                        "target_fingerprints": {},
                        "corr_current_fingerprints": {},
                        "paper_current_fingerprints": {},
                        "sound_current_fingerprints": {}
                    },
                    "proof_node_updates": {"Helper": {"Set": true}},
                    "node_kind_updates": node_kind_updates,
                    "dep_updates": {"Helper": {"Set": ["Preamble"]}},
                    "semantic_dep_updates": {},
                    "target_claim_updates": {"Helper": {"Set": []}},
                    "difficulty_updates": {}
                }
            }
        }));
        assert_eq!(
            output.status.code(),
            Some(0),
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("parse hydrate_worker_response output")
    };

    let first = hydrate(
        serde_json::json!({"Preamble": "preamble"}),
        serde_json::json!({"Helper": {"Set": "Proof"}}),
    );
    assert_eq!(first["status"], "hydrate_worker_response_ok");
    let first_fp_raw = first["output"]["response"]["snapshot"]
        ["substantiveness_current_fingerprints"]["Helper"]
        .as_str()
        .expect("first helper substantiveness fingerprint")
        .to_string();
    let first_fp: Value =
        serde_json::from_str(&first_fp_raw).expect("parse first substantiveness fingerprint");
    assert_eq!(first_fp["node_kind"], serde_json::json!("proof"));

    let second = hydrate(
        serde_json::json!({"Preamble": "preamble", "Helper": "proof"}),
        serde_json::json!({}),
    );
    assert_eq!(second["status"], "hydrate_worker_response_ok");
    let second_fp_raw = second["output"]["response"]["snapshot"]
        ["substantiveness_current_fingerprints"]["Helper"]
        .as_str()
        .expect("second helper substantiveness fingerprint");
    assert_eq!(first_fp_raw, second_fp_raw);
}

#[test]
fn runtime_cli_prepare_worker_gate_syncs_support_before_observations() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("write preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("write preamble tex");
    fs::write(
        tablet_dir.join("A.lean"),
        "import Tablet.Preamble\n\ndef A : Nat := 0\n",
    )
    .expect("write A lean");
    fs::write(
        tablet_dir.join("A.tex"),
        "\\begin{definition}A\\end{definition}\n",
    )
    .expect("write A tex");
    let script = script_dir.join("check.py");
    fs::write(
        &script,
        "#!/usr/bin/env python3\nimport json,sys\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {cmd}')\n",
    )
    .expect("write check script");
    let mut perms = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("script perms");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "prepare_worker_gate",
        "repo_path": repo,
        "request": {
            "id": 1,
            "kind": "worker",
            "cycle": 1,
            "phase": "theorem_stating",
            "current_present_nodes": ["Preamble"],
            "current_node_kinds": {
                "Preamble": "preamble"
            },
            "worker_acceptance": {
                "enabled": true,
                "validation_kind": "theorem_global",
                "observation_plan": {
                    "capture_before_snapshot": true
                }
            }
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse prepare_worker_gate output");
    assert_eq!(json["status"], "prepare_worker_gate_ok");
    assert_eq!(
        json["output"]["request"]["worker_context"]["authorized_nodes"],
        serde_json::json!([])
    );
    assert_eq!(
        json["output"]["request"]["worker_contract"]["scope_contract"]["existing_node_scope_mode"],
        "all_present"
    );
    assert_eq!(
        json["output"]["request"]["worker_contract"]["scope_contract"]["new_nodes_allowed"],
        true
    );
    // A2: pending_targets is omitted when empty.
    assert!(
        json["output"]["request"]["worker_contract"]["scope_contract"]
            .get("pending_targets")
            .is_none(),
        "pending_targets should be omitted when blocked_targets is empty"
    );
    assert_eq!(
        json["output"]["request"]["worker_contract"]["stuck_contract"]["meaning"],
        "cannot_make_progress_on_pending_work_under_current_scope"
    );
    assert!(!repo.join("Tablet.lean").exists());
}

#[test]
fn runtime_cli_stuck_worker_does_not_sync_support_files() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("write preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("write preamble tex");
    let before_snapshot = serde_json::json!({
        "Preamble.lean": sha256_hex(&fs::read(tablet_dir.join("Preamble.lean")).expect("read preamble lean")),
        "Preamble.tex": sha256_hex(&fs::read(tablet_dir.join("Preamble.tex")).expect("read preamble tex")),
    });

    let output = run_runtime_cli(&serde_json::json!({
        "action": "check_trellis_worker_result",
        "repo_path": repo,
        "acceptance_context": {
            "request": {
                "id": 21,
                "cycle": 4,
                "kind": "worker",
                "current_node_kinds": {
                    "Preamble": "preamble"
                }
            },
            "validation_kind": "theorem_global",
            "worker_acceptance": {
                "validation_execution_plan": [],
                "forbid_tablet_changes_when_stuck": true
            },
            "active_node": "",
            "held_target": "",
            "authorized_nodes": [],
            "configured_targets": [],
            "current_present_nodes": ["Preamble"],
            "current_proof_nodes": [],
            "current_deps": {
                "Preamble": []
            },
            "current_semantic_deps": {
                "Preamble": []
            },
            "current_target_claims": {
                "Preamble": []
            },
            "repo_path": repo,
            "before_snapshot": before_snapshot,
            "baseline_errors": [],
            "imports_before": [],
            "expected_active_hash": "",
            "baseline_declaration_hashes": {},
            "baseline_correspondence_hashes": {}
        },
        "raw_payload": {
            "outcome": "stuck",
            "summary": "Need theorem decomposition first.",
            "comments": "",
            "semantic_dep_updates": {},
            "target_claim_updates": {},
            "difficulty_updates": {}
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse check_trellis_worker_result output");
    assert_eq!(json["status"], "check_trellis_worker_result_ok");
    assert_eq!(json["output"]["final_outcome"], serde_json::json!("stuck"));
    assert!(
        !repo.join("Tablet.lean").exists(),
        "stuck worker path should not sync support artifacts"
    );
}

#[test]
fn runtime_cli_init_show_and_step_start_cycle() {
    let tmp = project_tempdir();
    let root = tmp.path().join("runtime");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_support_script(&repo);
    let config_path = write_runtime_cli_config(&repo);

    let init_output = run_runtime_cli(&serde_json::json!({
        "action": "init",
        "root": root,
        "metadata": {
            "repo_path": repo,
            "config_path": config_path
        },
        "state": {
            "stage": "Start"
        }
    }));
    assert_eq!(init_output.status.code(), Some(0));

    let show_output = run_runtime_cli(&serde_json::json!({
        "action": "show",
        "root": root,
    }));
    assert_eq!(show_output.status.code(), Some(0));
    let show_json: Value = serde_json::from_slice(&show_output.stdout).expect("parse show output");
    assert_eq!(show_json["status"], "ok");
    assert_eq!(show_json["state"]["stage"], "Start");
    assert_eq!(show_json["event_count"], 0);
    assert_eq!(show_json["metadata"]["repo_path"], serde_json::json!(repo));

    let step_output = run_runtime_cli(&serde_json::json!({
        "action": "step",
        "root": root,
    }));
    assert_eq!(step_output.status.code(), Some(0));
    let step_json: Value = serde_json::from_slice(&step_output.stdout).expect("parse step output");
    assert_eq!(step_json["status"], "ok");
    assert_eq!(step_json["state"]["stage"], "Worker");
    assert_eq!(step_json["state"]["cycle"], 1);
    assert_eq!(step_json["event_count"], 1);
    assert_eq!(
        step_json["outcome"]["commands"][0]["command"],
        "issue_request"
    );
}

#[test]
fn runtime_cli_init_seeds_configured_targets_from_config() {
    let tmp = project_tempdir();
    let root = tmp.path().join("runtime");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_support_script(&repo);
    let config_path = repo.join("trellis.config.json");
    fs::write(
        &config_path,
        serde_json::json!({
            "repo_path": repo,
            "worker": {"provider": "codex", "model": "worker-a", "label": "worker-a"},
            "reviewer": {"provider": "codex", "model": "reviewer-a", "label": "reviewer-a"},
            "workflow": {
                "main_result_targets": [
                    {"start_line": 63, "end_line": 70, "tex_label": "thm:conn"}
                ]
            }
        })
        .to_string(),
    )
    .expect("write config");

    let init_output = run_runtime_cli(&serde_json::json!({
        "action": "init",
        "root": root,
        "metadata": {
            "repo_path": repo,
            "config_path": config_path
        },
        "state": {
            "stage": "Start"
        }
    }));
    assert_eq!(init_output.status.code(), Some(0));
    let init_json: Value = serde_json::from_slice(&init_output.stdout).expect("parse init output");
    assert_eq!(init_json["status"], "ok");
    assert_eq!(
        init_json["state"]["configured_targets"],
        serde_json::json!(["thm:conn"])
    );
    assert_eq!(
        init_json["state"]["approved_targets"]["configured_targets"],
        serde_json::json!([])
    );
    assert_eq!(
        init_json["state"]["live"]["coverage"],
        serde_json::json!({"thm:conn": []})
    );
    assert_eq!(
        init_json["state"]["approved_targets"]["coverage"],
        serde_json::json!({})
    );

    let step_output = run_runtime_cli(&serde_json::json!({
        "action": "step",
        "root": root,
    }));
    assert_eq!(step_output.status.code(), Some(0));
    let step_json: Value = serde_json::from_slice(&step_output.stdout).expect("parse step output");
    assert_eq!(step_json["status"], "ok");
    assert_eq!(step_json["state"]["stage"], "Worker");
    assert_eq!(step_json["state"]["cycle"], 1);
}

#[test]
fn runtime_cli_init_from_config_owns_repo_resolution_and_support_sync() {
    let tmp = project_tempdir();
    let workspace = tmp.path().join("workspace");
    let repo = workspace.join("repo");
    let root = workspace.join("runtime");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_support_script(&repo);
    let config_dir = workspace.join("config");
    fs::create_dir_all(&config_dir).expect("config dir");
    let config_path = config_dir.join("trellis.config.json");
    fs::write(
        &config_path,
        serde_json::json!({
            "repo_path": "../repo",
            "worker": {"provider": "codex", "model": "worker-a", "label": "worker-a"},
            "reviewer": {"provider": "codex", "model": "reviewer-a", "label": "reviewer-a"},
            "workflow": {
                "main_result_targets": [
                    {"start_line": 63, "end_line": 70, "tex_label": "thm:conn"}
                ]
            }
        })
        .to_string(),
    )
    .expect("write config");

    let init_output = run_runtime_cli(&serde_json::json!({
        "action": "init_from_config",
        "root": root,
        "config_path": config_path,
    }));
    assert_eq!(init_output.status.code(), Some(0));
    let init_json: Value = serde_json::from_slice(&init_output.stdout).expect("parse init output");
    assert_eq!(init_json["status"], "ok");
    assert_eq!(init_json["metadata"]["repo_path"], serde_json::json!(repo));
    assert_eq!(
        init_json["state"]["configured_targets"],
        serde_json::json!(["thm:conn"])
    );
    assert!(repo.join("Tablet.lean").is_file());
}

// K-2 regression: a config with a single agent per verifier panel produces
// a `protocol_state.json` with `verifier_lanes == ["v1"]` rather than the
// legacy 2-lane default. This is the kernel-CLI-side gate that used to
// crash live runs with "not enough configured paper-faithfulness agents
// for requested lanes" on the first verifier dispatch.
#[test]
fn runtime_cli_init_from_config_derives_single_lane_for_single_agent_panels() {
    let tmp = project_tempdir();
    let workspace = tmp.path().join("workspace");
    let repo = workspace.join("repo");
    let root = workspace.join("runtime");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_support_script(&repo);
    let config_dir = workspace.join("config");
    fs::create_dir_all(&config_dir).expect("config dir");
    let config_path = config_dir.join("trellis.config.json");
    fs::write(
        &config_path,
        serde_json::json!({
            "repo_path": "../repo",
            "policy_path": "trellis.policy.json",
            "worker": {"provider": "codex", "model": "worker-a", "label": "worker-a"},
            "reviewer": {"provider": "codex", "model": "reviewer-a", "label": "reviewer-a"},
            "verification": {
                "correspondence_agents": [
                    {"provider": "codex", "model": "gpt-5.5", "label": "codex-xhigh"}
                ],
                "soundness_agents": [
                    {"provider": "codex", "model": "gpt-5.5", "label": "codex-xhigh"}
                ]
            },
            "workflow": {}
        })
        .to_string(),
    )
    .expect("write config");
    fs::write(
        config_dir.join("trellis.policy.json"),
        serde_json::json!({
            "verification": {
                "correspondence_agent_selectors": ["codex-xhigh"],
                "soundness_agent_selectors": ["codex-xhigh"]
            }
        })
        .to_string(),
    )
    .expect("write policy");

    let init_output = run_runtime_cli(&serde_json::json!({
        "action": "init_from_config",
        "root": root,
        "config_path": config_path,
    }));
    assert_eq!(init_output.status.code(), Some(0));
    let init_json: Value = serde_json::from_slice(&init_output.stdout).expect("parse init output");
    assert_eq!(init_json["status"], "ok");
    assert_eq!(
        init_json["state"]["verifier_lanes"],
        serde_json::json!(["v1"])
    );
}

// Backwards compat: a config with 2 agents per panel keeps the legacy
// 2-lane behavior for existing operator setups (e.g. the connectivity_gnp
// templates).
#[test]
fn runtime_cli_init_from_config_keeps_two_lanes_for_two_agent_panels() {
    let tmp = project_tempdir();
    let workspace = tmp.path().join("workspace");
    let repo = workspace.join("repo");
    let root = workspace.join("runtime");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_support_script(&repo);
    let config_dir = workspace.join("config");
    fs::create_dir_all(&config_dir).expect("config dir");
    let config_path = config_dir.join("trellis.config.json");
    fs::write(
        &config_path,
        serde_json::json!({
            "repo_path": "../repo",
            "policy_path": "trellis.policy.json",
            "worker": {"provider": "codex", "model": "worker-a", "label": "worker-a"},
            "reviewer": {"provider": "codex", "model": "reviewer-a", "label": "reviewer-a"},
            "verification": {
                "correspondence_agents": [
                    {"provider": "gemini", "model": "gemini-pro", "label": "gemini-pro"},
                    {"provider": "claude", "model": "claude-opus", "label": "claude-high"}
                ],
                "soundness_agents": [
                    {"provider": "gemini", "model": "gemini-pro", "label": "gemini-pro"},
                    {"provider": "claude", "model": "claude-opus", "label": "claude-high"}
                ]
            },
            "workflow": {}
        })
        .to_string(),
    )
    .expect("write config");
    fs::write(
        config_dir.join("trellis.policy.json"),
        serde_json::json!({
            "verification": {
                "correspondence_agent_selectors": ["gemini-pro", "claude-high"],
                "soundness_agent_selectors": ["gemini-pro", "claude-high"]
            }
        })
        .to_string(),
    )
    .expect("write policy");

    let init_output = run_runtime_cli(&serde_json::json!({
        "action": "init_from_config",
        "root": root,
        "config_path": config_path,
    }));
    assert_eq!(init_output.status.code(), Some(0));
    let init_json: Value = serde_json::from_slice(&init_output.stdout).expect("parse init output");
    assert_eq!(init_json["status"], "ok");
    assert_eq!(
        init_json["state"]["verifier_lanes"],
        serde_json::json!(["v1", "v2"])
    );
}

#[test]
fn runtime_cli_init_seeds_empty_preamble_as_present_current_corr_pass() {
    let tmp = project_tempdir();
    let root = tmp.path().join("runtime");
    let repo = tmp.path().join("repo");
    let tablet = repo.join("Tablet");
    fs::create_dir_all(&tablet).expect("tablet dir");
    seed_support_script(&repo);
    fs::write(tablet.join("Preamble.lean"), "-- shared imports\n").expect("write preamble");
    let config_path = repo.join("trellis.config.json");
    fs::write(
        &config_path,
        serde_json::json!({
            "repo_path": repo,
            "worker": {"provider": "codex", "model": "worker-a", "label": "worker-a"},
            "reviewer": {"provider": "codex", "model": "reviewer-a", "label": "reviewer-a"},
            "workflow": {
                "main_result_targets": [
                    {"start_line": 63, "end_line": 70, "tex_label": "thm:conn"}
                ]
            }
        })
        .to_string(),
    )
    .expect("write config");

    let init_output = run_runtime_cli(&serde_json::json!({
        "action": "init",
        "root": root,
        "metadata": {
            "repo_path": repo,
            "config_path": config_path
        },
        "state": {
            "stage": "Start"
        }
    }));
    assert_eq!(init_output.status.code(), Some(0));
    let init_json: Value = serde_json::from_slice(&init_output.stdout).expect("parse init output");
    assert_eq!(init_json["status"], "ok");
    assert_eq!(
        init_json["state"]["live"]["present_nodes"],
        serde_json::json!(["Preamble"])
    );
    assert_eq!(
        init_json["state"]["corr_status"]["Preamble"],
        serde_json::json!("Pass")
    );
    assert_eq!(
        init_json["state"]["corr_approved_fingerprints"]["Preamble"],
        serde_json::json!("")
    );
    assert_eq!(
        init_json["state"]["live"]["corr_current_fingerprints"]["Preamble"],
        serde_json::json!("")
    );

    let step_output = run_runtime_cli(&serde_json::json!({
        "action": "step",
        "root": root,
    }));
    assert_eq!(step_output.status.code(), Some(0));
    let step_json: Value = serde_json::from_slice(&step_output.stdout).expect("parse step output");
    assert_eq!(step_json["status"], "ok");
    assert_eq!(step_json["state"]["stage"], "Worker");

    let request_output = run_runtime_cli(&serde_json::json!({
        "action": "current_request",
        "root": root,
    }));
    assert_eq!(request_output.status.code(), Some(0));
    let request_json: Value =
        serde_json::from_slice(&request_output.stdout).expect("parse current_request output");
    assert_eq!(request_json["status"], "current_request_ok");
    assert_eq!(
        request_json["request"]["current_present_nodes"],
        serde_json::json!(["Preamble"])
    );
}

#[test]
fn runtime_cli_init_treats_preamble_without_structured_items_as_vacuously_passed() {
    let tmp = project_tempdir();
    let root = tmp.path().join("runtime");
    let repo = tmp.path().join("repo");
    let tablet = repo.join("Tablet");
    fs::create_dir_all(&tablet).expect("tablet dir");
    fs::write(tablet.join("Preamble.lean"), "-- shared imports\n").expect("write preamble");
    fs::write(
        tablet.join("Preamble.tex"),
        "This file has prose only.\n% and comments\n",
    )
    .expect("write preamble tex");

    let init_output = run_runtime_cli(&serde_json::json!({
        "action": "init",
        "root": root,
        "metadata": {
            "repo_path": repo,
        },
        "state": {
            "stage": "Start"
        }
    }));
    assert_eq!(init_output.status.code(), Some(0));
    let init_json: Value = serde_json::from_slice(&init_output.stdout).expect("parse init output");
    assert_eq!(init_json["status"], "ok");
    assert_eq!(init_json["state"]["corr_status"]["Preamble"], "Pass");
    assert_eq!(
        init_json["state"]["live"]["corr_current_fingerprints"]["Preamble"],
        ""
    );
}

#[test]
fn runtime_cli_init_creates_and_seeds_empty_preamble_for_paper_only_repo() {
    let tmp = project_tempdir();
    let root = tmp.path().join("runtime");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_support_script(&repo);
    let config_path = repo.join("trellis.config.json");
    fs::write(
        &config_path,
        serde_json::json!({
            "repo_path": repo,
            "worker": {"provider": "codex", "model": "worker-a", "label": "worker-a"},
            "reviewer": {"provider": "codex", "model": "reviewer-a", "label": "reviewer-a"},
            "workflow": {
                "main_result_targets": [
                    {"start_line": 63, "end_line": 70, "tex_label": "thm:conn"}
                ]
            }
        })
        .to_string(),
    )
    .expect("write config");

    let init_output = run_runtime_cli(&serde_json::json!({
        "action": "init",
        "root": root,
        "metadata": {
            "repo_path": repo,
            "config_path": config_path
        },
        "state": {
            "stage": "Start"
        }
    }));
    assert_eq!(init_output.status.code(), Some(0));
    let init_json: Value = serde_json::from_slice(&init_output.stdout).expect("parse init output");
    assert_eq!(init_json["status"], "ok");
    assert_eq!(
        init_json["state"]["live"]["present_nodes"],
        serde_json::json!(["Preamble"])
    );
    assert_eq!(
        init_json["state"]["corr_status"]["Preamble"],
        serde_json::json!("Pass")
    );
    assert!(repo.join("Tablet/Preamble.lean").is_file());
    assert!(repo.join("Tablet/Preamble.tex").is_file());

    let step_output = run_runtime_cli(&serde_json::json!({
        "action": "step",
        "root": root,
    }));
    assert_eq!(step_output.status.code(), Some(0));
    let step_json: Value = serde_json::from_slice(&step_output.stdout).expect("parse step output");
    assert_eq!(step_json["status"], "ok");
    assert_eq!(
        step_json["state"]["in_flight_request"]["worker_context"]["authorized_nodes"],
        serde_json::json!(["Preamble"])
    );
}
#[test]
fn runtime_cli_current_request_resolves_verifier_lane_bindings_from_config_and_policy() {
    let tmp = project_tempdir();
    let root = tmp.path().join("runtime");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_support_script(&repo);
    let config_path = repo.join("trellis.config.json");
    fs::write(
        &config_path,
        serde_json::json!({
            "repo_path": repo,
            "policy_path": "trellis.policy.json",
            "workflow": {},
            "verification": {
                "correspondence_agents": [
                    {"provider": "claude", "model": "corr-a", "label": "claude-a"},
                    {"provider": "gemini", "model": "corr-b", "label": "gemini-b"}
                ],
                "soundness_agents": [
                    {"provider": "claude", "model": "snd-a", "label": "claude-a"},
                    {"provider": "gemini", "model": "snd-b", "label": "gemini-b"}
                ]
            }
        })
        .to_string(),
    )
    .expect("write config");
    fs::write(
        repo.join("trellis.policy.json"),
        serde_json::json!({
            "verification": {
                "correspondence_agent_selectors": ["gemini-b", "claude-a"]
            }
        })
        .to_string(),
    )
    .expect("write policy");

    let init_output = run_runtime_cli(&serde_json::json!({
        "action": "init",
        "root": root,
        "metadata": {
            "repo_path": repo,
            "config_path": config_path
        },
        "state": {
            "stage": "VerifyCorr",
            "cycle": 3,
            "request_seq": 2,
            "in_flight_request": {
                "id": 2,
                "kind": "Corr",
                "cycle": 3,
                "phase": "TheoremStating",
                "mode": "Global",
                "verify_lanes": ["v2", "v1"],
                "verify_nodes": ["n1"],
                "verify_targets": ["t1"],
                "corr_verify_nodes": ["n1"],
                "corr_verify_targets": ["t1"],
                "current_present_nodes": ["Preamble", "n1"],
                "current_proof_nodes": ["n1"],
                "current_node_kinds": {"Preamble": "Preamble", "n1": "Proof"},
                "current_deps": {"Preamble": [], "n1": ["Preamble"]},
                "current_semantic_deps": {"Preamble": [], "n1": ["Preamble"]},
                "current_target_claims": {"Preamble": [], "n1": ["t1"]},
                "fresh_context": true,
                "invalid_attempt": false,
                "human_input_outstanding": false,
                "gate_kind": "None"
            }
        }
    }));
    assert_eq!(init_output.status.code(), Some(0));

    let output = run_runtime_cli(&serde_json::json!({
        "action": "current_request",
        "root": root,
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value = serde_json::from_slice(&output.stdout).expect("parse current_request output");
    assert_eq!(
        json["request"]["corr_verify_lane_bindings"],
        serde_json::json!([
            {
                "lane_id": "v1",
                "provider": "gemini",
                "model": "corr-b",
                "effort": null,
                "extra_args": [],
                "fallback_models": [],
                "label": "gemini-b"
            },
            {
                "lane_id": "v2",
                "provider": "claude",
                "model": "corr-a",
                "effort": null,
                "extra_args": [],
                "fallback_models": [],
                "label": "claude-a"
            }
        ])
    );
    assert_eq!(
        json["request"]["corr_contract"]["issue_reporting_policy"],
        serde_json::json!("explicit_per_node_verdicts")
    );
    assert_eq!(
        json["request"]["corr_contract"]["prompt_fragments"],
        serde_json::json!([
            "common/TRELLIS_FORMALIZATION_SCHEME_verifier.md",
            "verifier/common/00_intro.md",
            "verifier/correspondence/05_frontier.md",
            "verifier/correspondence/07_scratchpad.md",
            "shared/10_repository_root.md",
            "verifier/common/10_lane_id.md",
            "verifier/common/15_previous_findings.md",
            "shared/20_read_files.md",
            "shared/25_filespec.md",
            "shared/30_project_invariants.md",
            "verifier/correspondence/20_frontier.md",
            "verifier/correspondence/30_contract.md",
            "verifier/correspondence/40_rubric.md",
            "verifier/correspondence/50_authority.md",
            "shared/90_artifact_delivery.md",
            "canonical/CORRESPONDENCE.md",
            "shared/91_structured_request_pointer.md"
        ])
    );
    assert_eq!(
        json["request"]["corr_contract"]["artifact_prompt_view"]["json_check_command_template"],
        serde_json::json!([
            "python3",
            "{{check_script_path}}",
            "correspondence-result",
            "{{raw_output_path}}"
        ])
    );
    assert_eq!(
        json["request"]["corr_contract"]["artifact_prompt_view"]
            ["acceptance_check_command_template"],
        serde_json::Value::Null,
        "Trim 13: corr verifier has no acceptance checker; field is null \
         (the bridge-side null-drop helper strips it from the rendered prompt)"
    );
    assert_eq!(
        json["request"]["sound_verify_lane_bindings"],
        serde_json::json!([])
    );
}

#[test]
fn runtime_cli_executes_theorem_validation_plan_via_repo_check_script() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("dep.lean"),
        "theorem dep : True := by trivial\n",
    )
    .expect("dep lean");
    fs::write(
        tablet_dir.join("dep.tex"),
        "\\begin{lemma}dep\\end{lemma}\n",
    )
    .expect("dep tex");
    fs::write(
        tablet_dir.join("main_node.lean"),
        "import Tablet.dep\ntheorem main_node : True := by trivial\n",
    )
    .expect("main lean");
    fs::write(
        tablet_dir.join("main_node.tex"),
        "\\begin{theorem}main\\end{theorem}\n",
    )
    .expect("main tex");
    let script = script_dir.join("check.py");
    fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json
import sys

cmd = sys.argv[1]
if cmd == "lean-compile-node":
    json.dump(
        {
            "returncode": 0,
            "stdout": "scoped node build ok",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "print-axioms":
    json.dump(
        {
            "returncode": 0,
            "stdout": f"{sys.argv[2]} does not depend on any axioms",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "lean-build-tablet":
    json.dump(
        {
            "returncode": 0,
            "stdout": "scoped build ok",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
raise SystemExit(f"unexpected command: {cmd}")
"#,
    )
    .expect("write check script");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "execute_worker_validation_plan",
        "input": {
            "repo_path": repo,
            "active_node": "main_node",
            "before_snapshot": {"main_node.lean": "abc", "dep.lean": "xyz"},
            "baseline_errors": [],
            "authorized_nodes": ["main_node"],
            "current_present_nodes": ["dep", "main_node"],
            "validation_execution_plan": [
                {
                    "kind": "theorem_target_edit_scope",
                    "target": "main_node",
                    "initial_scope": ["main_node"]
                },
                {
                    "kind": "scoped_tablet",
                    "allowed_nodes_mode": "previous_or_explicit",
                    "explicit_nodes": ["main_node"]
                }
            ]
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse execute_worker_validation_plan");
    assert_eq!(json["status"], "execute_worker_validation_plan_ok");
    assert_eq!(
        json["output"]["step_results"][0]["kind"],
        serde_json::json!("theorem_target_edit_scope")
    );
    assert_eq!(
        json["output"]["step_results"][0]["allowed_nodes"],
        serde_json::json!(["dep", "main_node"])
    );
    assert_eq!(
        json["output"]["step_results"][1]["kind"],
        serde_json::json!("scoped_tablet")
    );
    assert_eq!(
        json["output"]["step_results"][1]["allowed_nodes"],
        serde_json::json!(["dep", "main_node"])
    );
    assert_eq!(
        json["output"]["step_results"][1]["build_output"],
        serde_json::json!("")
    );
}

#[test]
fn runtime_cli_executes_proof_and_cleanup_validation_plan_via_repo_check_script() {
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("preamble lean");
    fs::write(
        tablet_dir.join("main_node.lean"),
        "import Tablet.Preamble\ntheorem main_node : True := by trivial\n",
    )
    .expect("main lean");
    fs::write(
        tablet_dir.join("main_node.tex"),
        "\\begin{theorem}main\\end{theorem}\n",
    )
    .expect("main tex");
    let preamble_hash =
        sha256_hex(&fs::read(tablet_dir.join("Preamble.lean")).expect("read preamble"));
    let main_tex_hash =
        sha256_hex(&fs::read(tablet_dir.join("main_node.tex")).expect("read main tex"));
    let main_decl_hash = sha256_hex(b"theorem main_node : True");
    let script = script_dir.join("check.py");
    fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json
import sys

cmd = sys.argv[1]
if cmd == "lean-compile-node":
    json.dump(
        {
            "returncode": 0,
            "stdout": "delta build ok",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "print-axioms":
    json.dump(
        {
            "returncode": 0,
            "stdout": f"{sys.argv[2]} does not depend on any axioms",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
if cmd == "lean-build-tablet":
    json.dump(
        {
            "returncode": 0,
            "stdout": "cleanup build ok",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        sys.stdout,
    )
    sys.exit(0)
raise SystemExit(f"unexpected command: {cmd}")
"#,
    )
    .expect("write check script");

    let output = run_runtime_cli(&serde_json::json!({
        "action": "execute_worker_validation_plan",
        "input": {
            "repo_path": repo,
            "active_node": "main_node",
            "before_snapshot": {
                "main_node.lean": "abc",
                "main_node.tex": main_tex_hash,
                "Preamble.lean": preamble_hash
            },
            "baseline_errors": [],
            "imports_before": ["Tablet.Preamble"],
            "expected_active_hash": main_decl_hash.clone(),
            "baseline_declaration_hashes": {"main_node": main_decl_hash},
            "baseline_correspondence_hashes": {},
            "authorized_nodes": ["helper", "main_node"],
            "current_present_nodes": ["Preamble", "helper", "main_node"],
            "validation_execution_plan": [
                {
                    "kind": "proof_easy_scope",
                    "active_node": "main_node"
                },
                {
                    "kind": "proof_worker_delta",
                    "active_node": "main_node",
                    "mode": "local",
                    "authorized_nodes": []
                },
                {
                    "kind": "cleanup_preserving"
                }
            ]
        }
    }));
    assert_eq!(output.status.code(), Some(0));
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse execute_worker_validation_plan");
    assert_eq!(json["status"], "execute_worker_validation_plan_ok");
    assert_eq!(
        json["output"]["step_results"][0]["kind"],
        serde_json::json!("proof_easy_scope")
    );
    assert_eq!(
        json["output"]["step_results"][0]["ok"],
        serde_json::json!(false)
    );
    assert!(
        json["output"]["step_results"][0]["detail"]
            .as_str()
            .unwrap_or("")
            .contains("Legacy proof_easy_scope validation steps are retired"),
        "expected retired proof_easy_scope diagnostic, got {}",
        json["output"]["step_results"][0]["detail"]
    );
    assert_eq!(
        json["output"]["step_results"][1]["kind"],
        serde_json::json!("proof_worker_delta")
    );
    assert_eq!(
        json["output"]["step_results"][1]["allowed_nodes"],
        serde_json::json!(["helper", "main_node"])
    );
    assert_eq!(
        json["output"]["step_results"][2]["kind"],
        serde_json::json!("cleanup_preserving")
    );
    assert_eq!(
        json["output"]["step_results"][2]["build_output"],
        serde_json::json!("")
    );
}

#[test]
fn runtime_cli_rejects_missing_response_for_inflight_request() {
    let tmp = project_tempdir();
    let root = tmp.path().join("runtime");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_support_script(&repo);
    let config_path = write_runtime_cli_config(&repo);

    let _ = run_runtime_cli(&serde_json::json!({
        "action": "init",
        "root": root,
        "metadata": {
            "repo_path": repo,
            "config_path": config_path
        },
        "state": {
            "stage": "Start"
        }
    }));
    let _ = run_runtime_cli(&serde_json::json!({
        "action": "step",
        "root": root,
    }));

    let output = run_runtime_cli(&serde_json::json!({
        "action": "step",
        "root": root,
    }));
    assert_eq!(output.status.code(), Some(1));
    let json: Value = serde_json::from_slice(&output.stdout).expect("parse error output");
    assert_eq!(json["status"], "error");
    assert!(json["message"]
        .as_str()
        .is_some_and(|msg| msg.contains("missing response")));
}
#[test]
fn runtime_cli_run_executes_multiple_steps_via_bridge() {
    let tmp = project_tempdir();
    let root = tmp.path().join("runtime");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_support_script(&repo);
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
    .expect("write config");
    // The bridge returns Stuck for every worker, which now triggers a
    // worktree restore (the broadened predicate covers Stuck). The
    // restore calls `git reset --hard HEAD`, so the repo must be a git
    // worktree for this test to succeed.
    let git_commands: &[&[&str]] = &[
        &["init"],
        &["config", "user.name", "trellis-test"],
        &["config", "user.email", "trellis-test@example.com"],
        &["add", "-A"],
        &["commit", "-m", "init"],
    ];
    for args in git_commands {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(*args)
            .status()
            .expect("git command");
        assert!(status.success(), "git setup failed");
    }
    let bridge_log = tmp.path().join("bridge-log.jsonl");
    let bridge = tmp.path().join("bridge-run.py");
    fs::write(
        &bridge,
        format!(
            "#!/usr/bin/env python3\nimport json,sys\nfrom pathlib import Path\npayload=json.load(sys.stdin)\nlog=Path({:?})\nwith log.open('a', encoding='utf-8') as fh:\n    fh.write(json.dumps(payload)+'\\n')\nrequest=payload['request']\nif request['kind']=='worker':\n    json.dump({{'kind':'worker','request_id':request['id'],'cycle':request['cycle'],'status':'Ok','outcome':'Stuck','snapshot':{{'present_nodes':['Preamble'],'open_nodes':[],'coverage':{{}},'target_fingerprints':{{'Preamble':''}},'corr_current_fingerprints':{{'Preamble':''}},'paper_current_fingerprints':{{}},'sound_current_fingerprints':{{}}}},'proof_node_updates':{{}},'dep_updates':{{}},'semantic_dep_updates':{{}},'target_claim_updates':{{}},'difficulty_updates':{{}}}}, sys.stdout)\nelse:\n    json.dump({{'kind':'review','request_id':request['id'],'cycle':request['cycle'],'status':'Ok','decision':'NeedInput','task_blockers':[],'next_active':None,'reset':'None','next_mode':'Global','difficulty_updates':{{}},'clear_human_input':False}}, sys.stdout)\n",
            bridge_log.display().to_string()
        ),
    )
    .expect("write bridge script");
    let mut perms = fs::metadata(&bridge).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&bridge, perms).expect("chmod bridge script");
    let checker_socket = tmp.path().join("checker.sock");
    fs::write(&checker_socket, "").expect("write dummy checker socket path");

    let init_output = run_runtime_cli(&serde_json::json!({
        "action": "init",
        "root": root,
        "metadata": {
            "repo_path": repo,
            "config_path": config_path
        },
        "state": {
            "stage": "Start"
        }
    }));
    assert_eq!(init_output.status.code(), Some(0));

    let output = run_runtime_cli_with_env(
        &serde_json::json!({
            "action": "run",
            "root": root,
            "max_steps": 3
        }),
        &[
            ("TRELLIS_RUNTIME_BRIDGE_CMD", bridge.as_path()),
            ("TRELLIS_CHECKER_SOCKET", checker_socket.as_path()),
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let json: Value = serde_json::from_slice(&output.stdout).expect("parse run output");
    assert_eq!(json["status"], "ok");
    assert_eq!(json["steps_executed"], 3);
    assert_eq!(json["state"]["stage"], "Reviewer");
    let log_lines = fs::read_to_string(&bridge_log).expect("bridge log");
    let entries: Vec<Value> = log_lines
        .lines()
        .map(|line| serde_json::from_str(line).expect("bridge log json"))
        .collect();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["request"]["kind"], "worker");
    assert_eq!(entries[1]["request"]["kind"], "worker");
}

#[test]
fn runtime_cli_runs_checkpoint_hook_on_commit_command() {
    let tmp = project_tempdir();
    let root = tmp.path().join("runtime");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_support_script(&repo);
    let config_path = write_runtime_cli_config(&repo);
    let hook_output = tmp.path().join("hook-output.json");
    let hook = tmp.path().join("checkpoint-hook.sh");
    fs::write(&hook, "#!/bin/sh\ncat > \"$HOOK_OUT\"\n").expect("write hook");
    let mut perms = fs::metadata(&hook).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&hook, perms).expect("chmod hook");

    let _ = run_runtime_cli(&serde_json::json!({
        "action": "init",
        "root": root,
        "metadata": {
            "repo_path": repo,
            "config_path": config_path
        },
        "state": {
            "stage": "Reviewer",
            "cycle": 4,
            "request_seq": 1,
            "sound_assessment_schema_version": 1,
            "configured_targets": ["t"],
            "in_flight_request": { "id": 1, "kind": "Review", "cycle": 4 },
            "proof_nodes": ["a"],
            "committed_proof_nodes": ["a"],
            "target_claims": { "a": ["t"] },
            "committed_target_claims": { "a": ["t"] },
            "live": {
                "present_nodes": ["a", "b"],
                "open_nodes": ["a", "b"],
                "coverage": { "t": ["a"] },
                "target_fingerprints": { "a": "ta" },
                "paper_current_fingerprints": { "t": "a=ta" },
                "corr_current_fingerprints": { "a": "ca", "b": "cb" },
                "sound_current_fingerprints": { "a": "sa" },
                "substantiveness_current_fingerprints": { "a": "sa-a", "b": "sa-b" }
            },
            "committed": {
                "present_nodes": ["a", "b"],
                "open_nodes": ["a", "b"],
                "coverage": { "t": ["a"] },
                "target_fingerprints": { "a": "ta" },
                "paper_current_fingerprints": { "t": "a=ta" },
                "corr_current_fingerprints": { "a": "ca", "b": "cb" },
                "sound_current_fingerprints": { "a": "sa" },
                "substantiveness_current_fingerprints": { "a": "sa-a", "b": "sa-b" }
            },
            "corr_status": { "a": "Pass", "b": "Pass" },
            "corr_approved_fingerprints": { "a": "ca", "b": "cb" },
            "paper_status": { "t": "Pass" },
            "paper_approved_fingerprints": { "t": "a=ta" },
            "sound_status": { "a": "Pass" },
            "sound_approved_fingerprints": { "a": "sa" },
            "substantiveness_status": { "a": "Pass", "b": "Pass" },
            "substantiveness_approved_fingerprints": { "a": "sa-a", "b": "sa-b" }
        }
    }));

    let output = run_runtime_cli_with_env(
        &serde_json::json!({
            "action": "step",
            "root": root,
            "response": {
                "kind": "review",
                "request_id": 1,
                "cycle": 4,
                "status": "Ok",
                "decision": "Continue",
                "task_blockers": [],
                "next_active": "a",
                "reset": "None",
                "next_mode": "Global",
                "clear_human_input": false
            }
        }),
        &[
            ("TRELLIS_RUNTIME_CHECKPOINT_HOOK", hook.as_path()),
            ("HOOK_OUT", hook_output.as_path()),
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let payload: Value = serde_json::from_slice(&fs::read(&hook_output).expect("hook output"))
        .expect("parse hook output");
    assert_eq!(payload["checkpoint"]["cycle"], 4);
    assert_eq!(payload["commands"][0]["command"], "commit_checkpoint");
    assert_eq!(payload["metadata"]["repo_path"], serde_json::json!(repo));
}

#[test]
fn runtime_cli_restore_active_worker_base_normalizes_strict_modes_to_group_writable() {
    // 2026-04-28 fix regression test. When a worker burst writes Tablet
    // files via tools that default to mode 0o600 (e.g. Python's
    // `tempfile.mkstemp` followed by atomic rename), the snapshot capture
    // (`copy_dir_recursive` in `capture_active_worker_base_for_request`)
    // used to preserve those modes, and the rollback path
    // (`restore_repo_worktree_to_active_worker_base`) restored them as
    // 0o600. The next worker burst — running as the burst user, which is
    // in the supervisor's group — could not read or
    // modify those files because mode 0o600 grants no group access. The
    // failure mode: after a worker's transport_failure rolls back via
    // this exact path, the next worker burst hits a transient
    // write-permission error on a file that an earlier worker created with
    // a restrictive 0o600 mode.
    //
    // The fix normalizes the destination mode to 0o664 (group-writable)
    // inside `copy_dir_recursive`, so any kernel-driven copy into the
    // worker repo (or the snapshot dir, which feeds the rollback) leaves
    // group members able to read/write the resulting files.
    let tmp = project_tempdir();
    let root = tmp.path().join("runtime");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_support_script(&repo);
    let config_path = write_runtime_cli_config(&repo);

    // Repo contents: a Tablet/ with one node file at the strict 0o600 mode
    // that triggers the bug. After capture-into-snapshot then
    // restore-back-to-repo, this file's mode must end up 0o664.
    fs::create_dir_all(repo.join("Tablet")).expect("tablet dir");
    let strict_lean = repo.join("Tablet/StrictNode.lean");
    fs::write(
        &strict_lean,
        "import Tablet.Preamble\n-- [TABLET NODE: StrictNode]\n",
    )
    .expect("write strict lean");
    {
        let mut perms = fs::metadata(&strict_lean).expect("metadata").permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&strict_lean, perms).expect("chmod strict");
    }
    fs::write(repo.join("Tablet/Preamble.lean"), "").expect("write preamble lean");
    fs::write(repo.join("Tablet/Preamble.tex"), "").expect("write preamble tex");

    // The rollback's first phase (`restore_repo_worktree_to_head`) runs
    // `git reset --hard HEAD`, so the repo must be a git repo with at
    // least one commit. Initialize one and commit the test files so
    // `HEAD` is meaningful.
    let git_init = Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(&repo)
        .output()
        .expect("git init");
    assert!(
        git_init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&git_init.stderr)
    );
    let _ = Command::new("git")
        .args(["config", "user.email", "test@test.invalid"])
        .current_dir(&repo)
        .output()
        .expect("git config email");
    let _ = Command::new("git")
        .args(["config", "user.name", "test"])
        .current_dir(&repo)
        .output()
        .expect("git config name");
    let git_add = Command::new("git")
        .args(["add", "."])
        .current_dir(&repo)
        .output()
        .expect("git add");
    assert!(
        git_add.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&git_add.stderr)
    );
    let git_commit = Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(&repo)
        .output()
        .expect("git commit");
    assert!(
        git_commit.status.success(),
        "git commit failed: stdout={} stderr={}",
        String::from_utf8_lossy(&git_commit.stdout),
        String::from_utf8_lossy(&git_commit.stderr)
    );

    // Initialize the runtime with an in-flight Worker request so
    // `restore_active_worker_base_for_inflight` will proceed (it no-ops
    // when the in-flight kind is not Worker).
    let init_output = run_runtime_cli(&serde_json::json!({
        "action": "init",
        "root": root,
        "metadata": {
            "repo_path": repo,
            "config_path": config_path
        },
        "state": {
            "stage": "Worker",
            "cycle": 1,
            "request_seq": 1,
            "in_flight_request": {
                "id": 1,
                "kind": "Worker",
                "cycle": 1,
                "phase": "TheoremStating",
                "mode": "Global"
            }
        }
    }));
    assert_eq!(
        init_output.status.code(),
        Some(0),
        "init failed: stdout={} stderr={}",
        String::from_utf8_lossy(&init_output.stdout),
        String::from_utf8_lossy(&init_output.stderr),
    );

    // Manually populate the active_worker_base snapshot from the repo
    // (the natural code path goes through `step` which captures via
    // `apply_request_execution_hints_to_state`; bypassing that here keeps
    // the test focused on the rollback path itself, exercising the
    // production `RestoreActiveWorkerBase` CLI action which is the actual
    // surface the Python bridge calls in production). The capture is just
    // a directory copy, so we mirror it directly: snapshot mode follows
    // source mode so the strict 0o600 propagates into the snapshot,
    // matching what the live system observed.
    let snapshot_tablet = root.join("active_worker_base/Tablet");
    fs::create_dir_all(&snapshot_tablet).expect("snapshot tablet dir");
    {
        let snapshot_strict = snapshot_tablet.join("StrictNode.lean");
        fs::copy(&strict_lean, &snapshot_strict).expect("copy strict to snapshot");
        let mut perms = fs::metadata(&snapshot_strict)
            .expect("snapshot metadata")
            .permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&snapshot_strict, perms).expect("chmod snapshot strict");
        // Sanity: snapshot file has the strict mode that triggers the bug.
        let snapshot_mode = fs::metadata(&snapshot_strict)
            .expect("snapshot metadata 2")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            snapshot_mode, 0o600,
            "test setup: snapshot file should be at the strict mode"
        );
    }
    fs::copy(
        repo.join("Tablet/Preamble.lean"),
        snapshot_tablet.join("Preamble.lean"),
    )
    .expect("copy preamble lean to snapshot");
    fs::copy(
        repo.join("Tablet/Preamble.tex"),
        snapshot_tablet.join("Preamble.tex"),
    )
    .expect("copy preamble tex to snapshot");

    // Simulate worker_86's deletion: the Tablet/ file is gone before the
    // rollback runs.
    fs::remove_file(&strict_lean).expect("simulate worker delete");
    assert!(!strict_lean.exists(), "test setup: strict file deleted");

    // Drive the production rollback path via the same CLI action the
    // Python bridge calls (`_restore_active_worker_base_via_kernel`).
    let restore_output = run_runtime_cli(&serde_json::json!({
        "action": "restore_active_worker_base",
        "root": root
    }));
    assert_eq!(
        restore_output.status.code(),
        Some(0),
        "restore failed: stdout={} stderr={}",
        String::from_utf8_lossy(&restore_output.stdout),
        String::from_utf8_lossy(&restore_output.stderr),
    );
    let restore_json: Value =
        serde_json::from_slice(&restore_output.stdout).expect("parse restore output");
    assert_eq!(restore_json["status"], "restore_active_worker_base_ok");
    assert_eq!(
        restore_json["restored"], true,
        "rollback should have actually run (in-flight Worker + snapshot present)"
    );

    // The deleted file must be back in the worker repo.
    assert!(
        strict_lean.exists(),
        "rollback should have restored Tablet/StrictNode.lean to the worker repo"
    );

    // Bug fix assertion: the restored file's mode must be group-writable
    // (0o664), not the strict 0o600 that the snapshot held. Without the
    // fix this would still be 0o600 (group has no read/write), which on
    // the live system means the next worker burst — running as
    // the burst user, in the supervisor's group — cannot
    // open the file for read or write.
    let restored_mode = fs::metadata(&strict_lean)
        .expect("restored metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        restored_mode, 0o664,
        "restored Tablet file should be 0o664 (group-writable); was {restored_mode:o}"
    );

    // Sanity: group-write is set, group-read is set. These are the bits
    // the next worker (group member) actually needs to read/edit.
    assert!(
        restored_mode & 0o060 == 0o060,
        "restored mode {restored_mode:o} is missing group-read or group-write"
    );

    // Directory normalization: the rollback path also recreates Tablet/
    // (`fs::remove_dir_all` then `copy_dir_recursive`). Confirm the dir
    // ends up group-writable too — necessary for the worker to be able
    // to add or delete sibling node files.
    let tablet_dir_mode = fs::metadata(repo.join("Tablet"))
        .expect("tablet dir metadata")
        .permissions()
        .mode()
        & 0o7777;
    assert!(
        tablet_dir_mode & 0o020 == 0o020,
        "Tablet/ dir mode {tablet_dir_mode:o} is missing group-write"
    );
}

// ---------------------------------------------------------------------------
// Tier 1 reviewer-payload contract violations
//
// Each test submits a `normalize_review` request with a deliberately invalid
// raw_payload and asserts the kernel rejects via `review_response_legal`,
// surfacing exit code 1 and the canonical "review response is not legal for
// the request" error. These are end-to-end regressions for the
// reviewer-decision contract rules; the unit tests in model.rs cover the
// rules in isolation, but these tests prove the rules are reachable through
// the actual CLI surface that the bridge invokes in production.
// ---------------------------------------------------------------------------

fn assert_review_rejected(output: &std::process::Output) {
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit code 1; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse normalize_review error output");
    assert_eq!(
        json["status"], "error",
        "expected status=error, got json={json}"
    );
    // Two distinct rejection paths surface invalid reviewer payloads:
    //   1. Pre-validation parse layer (unknown blocker ids, malformed
    //      decisions) — emits its own descriptive error.
    //   2. `review_response_legal` — emits the canonical
    //      "review response is not legal for the request".
    // Either is a successful rejection.
    let message = json["message"].as_str().unwrap_or("");
    let parse_rejection = message.contains("unknown blocker")
        || message.contains("invalid")
        || message.contains("not allowed")
        || message.contains("authorized_node_ids");
    let legality_rejection = message.contains("review response is not legal for the request");
    assert!(
        parse_rejection || legality_rejection,
        "expected reviewer-payload rejection in message, got {message:?}"
    );
}

#[test]
fn runtime_cli_prepare_worker_gate_proof_restructure_authorizes_cross_node_edits() {
    // When validation_kind=proof_restructure (the kind selected when the
    // reviewer chose Restructure mode, regardless of active-node difficulty
    // — see kernel/src/model.rs current_worker_validation_kind), the worker
    // contract surface MUST advertise scope_mode="authorized_existing_nodes"
    // so the worker is allowed to edit nodes beyond the active one. This
    // documents the downstream effect of the easy-node-restructure kernel
    // fix: an Easy active node in Restructure mode must NOT receive the
    // proof_easy single-file scope.
    let tmp = project_tempdir();
    let repo = tmp.path().join("repo");
    let tablet_dir = repo.join("Tablet");
    let script_dir = repo.join(".trellis/scripts");
    fs::create_dir_all(&tablet_dir).expect("tablet dir");
    fs::create_dir_all(&script_dir).expect("script dir");
    fs::write(
        tablet_dir.join("Preamble.lean"),
        "import Mathlib.Data.Nat.Basic\n",
    )
    .expect("write preamble lean");
    fs::write(tablet_dir.join("Preamble.tex"), "").expect("write preamble tex");
    fs::write(
        tablet_dir.join("ActiveProof.lean"),
        "import Tablet.Preamble\n\ntheorem ActiveProof : True := trivial\n",
    )
    .expect("write active proof lean");
    fs::write(
        tablet_dir.join("ActiveProof.tex"),
        "\\begin{theorem}ActiveProof\\end{theorem}\n",
    )
    .expect("write active proof tex");
    fs::write(
        tablet_dir.join("OtherNode.lean"),
        "import Tablet.Preamble\n\ntheorem OtherNode : True := trivial\n",
    )
    .expect("write other node lean");
    fs::write(
        tablet_dir.join("OtherNode.tex"),
        "\\begin{theorem}OtherNode\\end{theorem}\n",
    )
    .expect("write other node tex");
    seed_support_script(&repo);

    let output = run_runtime_cli(&serde_json::json!({
        "action": "prepare_worker_gate",
        "repo_path": repo,
        "request": {
            "id": 30,
            "kind": "worker",
            "cycle": 7,
            "phase": "proof_formalization",
            "active_node": "ActiveProof",
            "current_present_nodes": ["Preamble", "ActiveProof", "OtherNode"],
            "current_node_kinds": {
                "Preamble": "preamble",
                "ActiveProof": "proof",
                "OtherNode": "proof"
            },
            "worker_acceptance": {
                "enabled": true,
                "validation_kind": "proof_restructure",
                "authorized_nodes": ["ActiveProof", "OtherNode"],
                "observation_plan": {}
            }
        }
    }));
    assert_eq!(
        output.status.code(),
        Some(0),
        "expected exit 0; stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse prepare_worker_gate output");
    assert_eq!(json["status"], "prepare_worker_gate_ok");
    // ProofRestructure scope: the worker is allowed to edit any node in
    // its `authorized_nodes` set, not just the active node.
    assert_eq!(
        json["output"]["request"]["worker_contract"]["scope_contract"]["existing_node_scope_mode"],
        "authorized_existing_nodes"
    );
    // Helper-creation IS allowed under Restructure (it's the explicit
    // authorization for "active proof burden + nearby support edits").
    assert_eq!(
        json["output"]["request"]["worker_contract"]["scope_contract"]["new_nodes_allowed"],
        true
    );
}

#[test]
fn runtime_cli_normalize_review_rejects_local_mode_with_task_blockers() {
    // Regression for: reviewer chose Local mode + non-empty task_blocker_ids.
    // Local mode authorizes only the active node's proof body; task_blockers
    // require Restructure or CoarseRestructure scope. The kernel must reject
    // so the runtime reissues the review request.
    let output = run_runtime_cli(&serde_json::json!({
        "action": "normalize_review",
        "input": {
            "request": {
                "id": 21,
                "kind": "review",
                "cycle": 7,
                "phase": "proof_formalization",
                "mode": "local",
                "active_node": "ActiveProof",
                "blockers": [{
                    "kind": "NodeCorr",
                    "object": {"otype": "node", "node": "OtherNode"},
                    "fingerprint": "fp-other"
                }],
                "allowed_decisions": ["continue"],
                "allowed_next_modes": ["local", "restructure", "coarse_restructure"],
                "kernel_hinted_next_active_nodes": ["ActiveProof", "OtherNode"],
                "targeted_next_active_nodes": [],
                "allow_targeted_without_next_active": false,
                "allowed_resets": ["none"],
                "allowed_difficulty_update_nodes": [],
                "current_present_nodes": ["ActiveProof", "OtherNode"],
                "human_input_outstanding": false,
                "gate_kind": "none"
            },
            "raw_payload": {
                "decision": "continue",
                // BUG: task_blocker_ids non-empty under Local mode — illegal.
                "task_blocker_ids": ["nodecorr:node:OtherNode:fp-other"],
                "reset_blocker_ids": [],
                "next_active": "ActiveProof",
                "next_mode": "local",
                "reset": "none",
                "difficulty_updates": {},
                "allow_new_obligations": true,
                "must_close_active": false,
                "clear_human_input": false
            }
        }
    }));
    assert_review_rejected(&output);
}

#[test]
fn runtime_cli_normalize_review_rejects_continue_restructure_without_authorized_nodes() {
    // Regression for the Continue/Restructure authorized-nodes rule: in
    // ProofFormalization Continue mode, next_mode=Restructure (or
    // CoarseRestructure) with reset=None requires non-empty
    // authorized_node_ids. A reviewer that leaves the envelope empty must
    // be rejected. (The payload below also omits a live blocker from all
    // action buckets, which is now allowed — omitted blockers stay live
    // — but the authorized-nodes rule still fires.)
    let output = run_runtime_cli(&serde_json::json!({
        "action": "normalize_review",
        "input": {
            "request": {
                "id": 22,
                "kind": "review",
                "cycle": 8,
                "phase": "proof_formalization",
                "mode": "restructure",
                "active_node": "ActiveProof",
                "blockers": [{
                    "kind": "NodeCorr",
                    "object": {"otype": "node", "node": "OtherNode"},
                    "fingerprint": "fp-other"
                }],
                "allowed_decisions": ["continue"],
                "allowed_next_modes": ["local", "restructure", "coarse_restructure"],
                "kernel_hinted_next_active_nodes": ["ActiveProof", "OtherNode"],
                "targeted_next_active_nodes": [],
                "allow_targeted_without_next_active": false,
                "allowed_resets": ["none"],
                "allowed_difficulty_update_nodes": [],
                "current_present_nodes": ["ActiveProof", "OtherNode"],
                "human_input_outstanding": false,
                "gate_kind": "none"
            },
            "raw_payload": {
                "decision": "continue",
                // BUG: outstanding blocker omitted from all three buckets.
                "task_blocker_ids": [],
                "reset_blocker_ids": [],
                "next_active": "ActiveProof",
                "next_mode": "restructure",
                "reset": "none",
                "difficulty_updates": {},
                "allow_new_obligations": true,
                "must_close_active": false,
                "clear_human_input": false
            }
        }
    }));
    assert_review_rejected(&output);
}

#[test]
fn runtime_cli_normalize_review_rejects_fabricated_task_blocker() {
    // Regression for: reviewer's task_blockers must be a subset of the
    // request's outstanding blockers — a reviewer cannot invent a blocker
    // id that wasn't surfaced by the request.
    let output = run_runtime_cli(&serde_json::json!({
        "action": "normalize_review",
        "input": {
            "request": {
                "id": 24,
                "kind": "review",
                "cycle": 10,
                "phase": "proof_formalization",
                "mode": "restructure",
                "active_node": "ActiveProof",
                // Outstanding: just one real blocker.
                "blockers": [{
                    "kind": "NodeCorr",
                    "object": {"otype": "node", "node": "RealNode"},
                    "fingerprint": "fp-real"
                }],
                "allowed_decisions": ["continue"],
                "allowed_next_modes": ["restructure"],
                "kernel_hinted_next_active_nodes": ["ActiveProof", "RealNode"],
                "targeted_next_active_nodes": [],
                "allow_targeted_without_next_active": false,
                "allowed_resets": ["none"],
                "allowed_difficulty_update_nodes": [],
                "current_present_nodes": ["ActiveProof", "RealNode"],
                "human_input_outstanding": false,
                "gate_kind": "none"
            },
            "raw_payload": {
                "decision": "continue",
                // BUG: task_blocker_ids references a blocker id that the
                // request did not surface.
                "task_blocker_ids": ["nodecorr:node:GhostNode:fp-ghost"],
                "reset_blocker_ids": [],
                "next_active": "ActiveProof",
                "next_mode": "restructure",
                "reset": "none",
                "difficulty_updates": {},
                "allow_new_obligations": true,
                "must_close_active": false,
                "clear_human_input": false
            }
        }
    }));
    assert_review_rejected(&output);
}

#[test]
fn runtime_cli_normalize_review_reports_task_blocker_outside_worker_scope() {
    let output = run_runtime_cli(&serde_json::json!({
        "action": "normalize_review",
        "input": {
            "request": {
                "id": 27,
                "kind": "review",
                "cycle": 13,
                "phase": "proof_formalization",
                "mode": "restructure",
                "active_node": "ActiveProof",
                "blockers": [{
                    "kind": "NodeCorr",
                    "object": {"otype": "node", "node": "OtherNode"},
                    "fingerprint": "fp-other"
                }],
                "allowed_decisions": ["continue"],
                "allowed_next_modes": ["restructure", "coarse_restructure"],
                "kernel_hinted_next_active_nodes": ["ActiveProof", "OtherNode"],
                "targeted_next_active_nodes": [],
                "allow_targeted_without_next_active": false,
                "allowed_resets": ["none"],
                "allowed_difficulty_update_nodes": [],
                "current_present_nodes": ["ActiveProof", "OtherNode"],
                "current_deps": {},
                "current_target_claims": {},
                "human_input_outstanding": false,
                "gate_kind": "none"
            },
            "raw_payload": {
                "decision": "continue",
                "task_blocker_ids": ["nodecorr:node:OtherNode:fp-other"],
                "reset_blocker_ids": [],
                "next_active": "ActiveProof",
                "next_mode": "restructure",
                "reset": "none",
                "difficulty_updates": {},
                "allow_new_obligations": true,
                "must_close_active": false,
                "clear_human_input": false,
                "authorized_node_ids": ["ActiveProof"]
            }
        }
    }));
    assert_review_rejected(&output);
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("parse normalize_review error output");
    let message = json["message"].as_str().unwrap_or("");
    assert!(
        message.contains("outside the proposed worker scope") && message.contains("OtherNode"),
        "expected clear worker-scope rejection, got {message:?}"
    );
}

#[test]
fn runtime_cli_normalize_review_rejects_need_input_with_blocker_actions() {
    // Regression for: NeedInput is a "halt the run for human input"
    // decision; it must NOT carry any populated blocker action bucket
    // (task / override / reset) or otherwise adjudicate mode / active node.
    let output = run_runtime_cli(&serde_json::json!({
        "action": "normalize_review",
        "input": {
            "request": {
                "id": 25,
                "kind": "review",
                "cycle": 11,
                "phase": "proof_formalization",
                "mode": "local",
                "active_node": "ActiveProof",
                "blockers": [{
                    "kind": "NodeCorr",
                    "object": {"otype": "node", "node": "OtherNode"},
                    "fingerprint": "fp-other"
                }],
                "allowed_decisions": ["continue", "need_input"],
                "allowed_next_modes": ["local", "restructure"],
                "kernel_hinted_next_active_nodes": ["ActiveProof", "OtherNode"],
                "targeted_next_active_nodes": [],
                "allow_targeted_without_next_active": false,
                "allowed_resets": ["none"],
                "allowed_difficulty_update_nodes": [],
                "human_input_outstanding": false,
                "gate_kind": "none"
            },
            "raw_payload": {
                "decision": "need_input",
                // BUG: NeedInput must have empty task_blockers; the comment
                // field is the only place to put the human-readable ask.
                "task_blocker_ids": ["nodecorr:node:OtherNode:fp-other"],
                "reset_blocker_ids": [],
                "next_active": "",
                "next_mode": "local",
                "reset": "none",
                "difficulty_updates": {},
                "allow_new_obligations": true,
                "must_close_active": false,
                "clear_human_input": false
            }
        }
    }));
    assert_review_rejected(&output);
}

#[test]
fn runtime_cli_normalize_review_rejects_advance_phase_with_outstanding_blockers() {
    // Regression for: AdvancePhase decision is only legal when there are
    // no outstanding blockers AND the human-input gate is settled. Catches
    // a reviewer that tries to skip past unresolved correspondence/sound
    // failures.
    let output = run_runtime_cli(&serde_json::json!({
        "action": "normalize_review",
        "input": {
            "request": {
                "id": 26,
                "kind": "review",
                "cycle": 12,
                "phase": "theorem_stating",
                "mode": "global",
                "active_node": null,
                "blockers": [{
                    "kind": "PaperFaithfulness",
                    "object": {"otype": "target", "target": "main_result"},
                    "fingerprint": "fp-main"
                }],
                "allowed_decisions": ["continue", "advance_phase"],
                "allowed_next_modes": ["global", "targeted"],
                "kernel_hinted_next_active_nodes": [],
                "targeted_next_active_nodes": [],
                "allow_targeted_without_next_active": true,
                "allowed_resets": ["none"],
                "allowed_difficulty_update_nodes": [],
                "human_input_outstanding": false,
                "gate_kind": "none"
            },
            "raw_payload": {
                "decision": "advance_phase",
                // BUG: outstanding blocker present but reviewer chose
                // advance_phase without addressing it.
                "task_blocker_ids": [],
                "reset_blocker_ids": [],
                "next_active": "",
                "next_mode": "global",
                "reset": "none",
                "difficulty_updates": {},
                "allow_new_obligations": true,
                "must_close_active": false,
                "clear_human_input": false
            }
        }
    }));
    assert_review_rejected(&output);
}

#[test]
fn runtime_cli_normalize_review_rejects_reset_with_nonempty_blocker_actions() {
    // Regression for: reset (LastClean / LastCommit) is a whole-state
    // rollback — the reviewer must NOT also adjudicate individual blockers.
    // task / override / reset blocker action buckets must all be empty when
    // a reset is requested.
    let output = run_runtime_cli(&serde_json::json!({
        "action": "normalize_review",
        "input": {
            "request": {
                "id": 23,
                "kind": "review",
                "cycle": 9,
                "phase": "proof_formalization",
                "mode": "local",
                "active_node": "ActiveProof",
                "blockers": [{
                    "kind": "NodeCorr",
                    "object": {"otype": "node", "node": "OtherNode"},
                    "fingerprint": "fp-other"
                }],
                "allowed_decisions": ["continue"],
                "allowed_next_modes": ["local", "restructure"],
                "kernel_hinted_next_active_nodes": ["ActiveProof", "OtherNode"],
                "targeted_next_active_nodes": [],
                "allow_targeted_without_next_active": false,
                "allowed_resets": ["none", "last_commit"],
                "allowed_difficulty_update_nodes": [],
                "human_input_outstanding": false,
                "gate_kind": "none"
            },
            "raw_payload": {
                "decision": "continue",
                // BUG: reset is a whole-state rollback; combining it with a
                // populated task_blocker is contradictory.
                "task_blocker_ids": ["nodecorr:node:OtherNode:fp-other"],
                "reset_blocker_ids": [],
                "next_active": "ActiveProof",
                "next_mode": "local",
                "reset": "last_commit",
                "difficulty_updates": {},
                "allow_new_obligations": true,
                "must_close_active": false,
                "clear_human_input": false
            }
        }
    }));
    assert_review_rejected(&output);
}

/// E2E coverage for the symmetric proof-phase StartCycle dispatch
/// (`0f37014`). Exercises the real `trellis_runtime_cli` binary:
///
/// 1. `init` seeds a runtime with phase=ProofFormalization, an active
///    node "a" with substantiveness Unknown (no status entry, no
///    approved fingerprint), and all OTHER lanes Pass.
/// 2. `step` (no response) drives StartCycle. Pre-`0f37014` this
///    would unconditionally route to RequestKind::Worker; under the
///    symmetric-StartCycle change it must route to
///    RequestKind::Paper carrying the substantiveness frontier.
///
/// This pins the symmetry end-to-end through the real CLI surface,
/// so a future kernel-only refactor that breaks the wiring at
/// engine.rs:506-508 cannot land silently.
#[test]
fn runtime_cli_proof_start_with_substantiveness_unknown_dispatches_paper() {
    let tmp = project_tempdir();
    let root = tmp.path().join("runtime");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_support_script(&repo);
    // Custom config: must include a verifier agent pool for the
    // substantiveness lane. Without one, the bridge verifier binding
    // step in step() rejects the request with "not enough configured
    // substantiveness agents for requested lanes".
    let config_path = repo.join("trellis.config.json");
    fs::write(
        &config_path,
        serde_json::json!({
            "repo_path": repo,
            "worker": {"provider": "codex", "model": "worker-a", "label": "worker-a"},
            "reviewer": {"provider": "codex", "model": "reviewer-a", "label": "reviewer-a"},
            "verification": {
                "correspondence_agents": [
                    {"provider": "codex", "model": "v1", "label": "v1"}
                ],
                "soundness_agents": [
                    {"provider": "codex", "model": "v1", "label": "v1"}
                ],
                "substantiveness_agents": [
                    {"provider": "codex", "model": "v1", "label": "v1"}
                ]
            },
            "workflow": {}
        })
        .to_string(),
    )
    .expect("write proof-phase substantiveness config");

    let init_output = run_runtime_cli(&serde_json::json!({
        "action": "init",
        "root": root,
        "metadata": {
            "repo_path": repo,
            "config_path": config_path
        },
        "state": {
            "stage": "Start",
            "phase": "proof_formalization",
            "cycle": 4,
            "request_seq": 1,
            "sound_assessment_schema_version": 1,
            "configured_targets": ["t"],
            "active_node": "a",
            "proof_nodes": ["a"],
            "committed_proof_nodes": ["a"],
            "target_claims": { "a": ["t"] },
            "committed_target_claims": { "a": ["t"] },
            "live": {
                "present_nodes": ["a"],
                "open_nodes": ["a"],
                "coverage": { "t": ["a"] },
                "target_fingerprints": { "a": "ta" },
                "paper_current_fingerprints": { "t": "a=ta" },
                "corr_current_fingerprints": { "a": "ca" },
                "sound_current_fingerprints": { "a": "sa" }
            },
            "committed": {
                "present_nodes": ["a"],
                "open_nodes": ["a"],
                "coverage": { "t": ["a"] },
                "target_fingerprints": { "a": "ta" },
                "paper_current_fingerprints": { "t": "a=ta" },
                "corr_current_fingerprints": { "a": "ca" },
                "sound_current_fingerprints": { "a": "sa" }
            },
            "corr_status": { "a": "Pass" },
            "corr_approved_fingerprints": { "a": "ca" },
            "paper_status": { "t": "Pass" },
            "paper_approved_fingerprints": { "t": "a=ta" },
            "sound_status": { "a": "Pass" },
            "sound_approved_fingerprints": { "a": "sa" }
        }
    }));
    assert_eq!(
        init_output.status.code(),
        Some(0),
        "init failed; stderr={}",
        String::from_utf8_lossy(&init_output.stderr),
    );

    let step_output = run_runtime_cli(&serde_json::json!({
        "action": "step",
        "root": root,
    }));
    assert_eq!(
        step_output.status.code(),
        Some(0),
        "step failed; stderr={}",
        String::from_utf8_lossy(&step_output.stderr),
    );
    let step_json: Value = serde_json::from_slice(&step_output.stdout).expect("parse step output");
    assert_eq!(step_json["status"], "ok");
    // Symmetric StartCycle: substantiveness frontier non-empty in proof
    // phase routes to Paper / VerifyPaper, not Worker.
    assert_eq!(
        step_json["state"]["stage"], "VerifyPaper",
        "proof-phase StartCycle with substantiveness Unknown must \
         route to VerifyPaper (got: {:?})",
        step_json["state"]["stage"],
    );
    let cmd = &step_json["outcome"]["commands"][0];
    assert_eq!(cmd["command"], "issue_request");
    assert_eq!(
        cmd["request"]["kind"], "Paper",
        "expected Paper kind from proof_start_request_kind; got {:?}",
        cmd["request"]["kind"],
    );
    let sub_nodes = &cmd["request"]["substantiveness_verify_nodes"];
    assert!(
        sub_nodes.as_array().map(|a| !a.is_empty()).unwrap_or(false),
        "expected non-empty substantiveness_verify_nodes; got {:?}",
        sub_nodes,
    );
    assert_eq!(sub_nodes[0], "a");
}
