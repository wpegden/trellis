"""Focused tests for the active kernel-backed checker wrapper."""

from __future__ import annotations

import json
import tempfile
from pathlib import Path
from unittest.mock import patch

import trellis.atomic_actions.observations as atomic_observations
import trellis.runtime_snapshot as runtime_snapshot
from trellis.agent_check import main as check_main
from trellis.atomic_actions.observations import LEAN_SUPPORT_TIMEOUT_SECS
from trellis.checking import (
    build_trellis_worker_acceptance_context,
    check_node,
    check_tablet,
    check_tablet_scoped,
    validate_json_artifact,
    write_scripts,
)
from trellis.runtime import kernel_cli
from trellis.runtime_snapshot import materialize_project_runtime


def _tmp_repo() -> Path:
    repo = Path(tempfile.mkdtemp())
    (repo / "Tablet").mkdir()
    return repo


def test_build_trellis_worker_acceptance_context_uses_prepare_worker_gate() -> None:
    repo = _tmp_repo()
    request = {"id": 9, "cycle": 4, "kind": "worker"}

    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "prepare_worker_gate_ok",
            "output": {
                "request": request,
                "validation_kind": "cleanup",
                "worker_acceptance": {
                    "validation_kind": "cleanup",
                    "authorized_nodes": [],
                    "validation_execution_plan": [{"kind": "cleanup_preserving"}],
                },
                "active_node": "",
                "held_target": "",
                "authorized_nodes": [],
                "configured_targets": [],
                "current_present_nodes": [],
                "current_proof_nodes": [],
                "current_deps": {},
                "current_semantic_deps": {},
                "current_target_claims": {},
                "repo_path": str(repo),
                "before_snapshot": {},
                "baseline_errors": [],
                "imports_before": [],
                "expected_active_hash": "",
                "baseline_declaration_hashes": {},
                "baseline_correspondence_hashes": {},
            },
        },
    ) as mock_kernel:
        result = build_trellis_worker_acceptance_context(repo, request)

    assert result["ok"]
    assert result["data"]["validation_kind"] == "cleanup"
    payload = mock_kernel.call_args.args[0]
    assert payload["action"] == "prepare_worker_gate"
    assert payload["repo_path"] == str(repo)
    assert payload["request"]["id"] == 9


def test_validate_json_artifact_uses_kernel_for_soundness() -> None:
    repo = _tmp_repo()
    raw = repo / "sound.raw.json"
    raw.write_text(
        json.dumps(
            {
                "node": "n1",
                "soundness": {"decision": "SOUND", "explanation": "ok"},
                "overall": "APPROVE",
                "summary": "ok",
                "comments": "",
            }
        ),
        encoding="utf-8",
    )

    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "validate_soundness_result_ok",
            "output": {
                "ok": True,
                "errors": [],
                "data": {
                    "node": "n1",
                    "soundness": {"decision": "SOUND", "explanation": "ok"},
                    "overall": "APPROVE",
                    "summary": "ok",
                    "comments": "",
                },
            },
        },
    ) as mock_kernel:
        result = validate_json_artifact("soundness-result", raw, node_name="n1")

    assert result["ok"]
    assert result["data"] == {
        "node": "n1",
        "soundness": {"decision": "SOUND", "explanation": "ok"},
        "overall": "APPROVE",
        "summary": "ok",
        "comments": "",
    }
    payload = mock_kernel.call_args.args[0]
    assert payload["action"] == "validate_soundness_result"
    assert payload["node_name"] == "n1"


def test_validate_json_artifact_correspondence_unwraps_kernel_validation_output() -> None:
    repo = _tmp_repo()
    raw = repo / "corr.raw.json"
    raw.write_text(
        json.dumps(
            {
                "correspondence": {"decision": "PASS", "verdicts": []},
                "paper_faithfulness": {"decision": "PASS", "issues": []},
                "overall": "APPROVE",
                "summary": "ok",
                "comments": "",
            }
        ),
        encoding="utf-8",
    )

    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "validate_correspondence_result_ok",
            "output": {
                "ok": True,
                "errors": [],
                "data": {
                    "correspondence": {"decision": "PASS", "verdicts": []},
                    "paper_faithfulness": {"decision": "PASS", "issues": []},
                    "overall": "APPROVE",
                    "summary": "ok",
                    "comments": "",
                },
            },
        },
    ):
        result = validate_json_artifact("correspondence-result", raw)

    assert result["ok"]
    assert result["data"] == {
        "correspondence": {"decision": "PASS", "verdicts": []},
        "paper_faithfulness": {"decision": "PASS", "issues": []},
        "overall": "APPROVE",
        "summary": "ok",
        "comments": "",
    }


def test_check_main_worker_result_uses_single_kernel_check_action() -> None:
    repo = _tmp_repo()
    raw = repo / "worker.raw.json"
    context = repo / "worker.context.json"
    raw.write_text(
        json.dumps(
            {
                "outcome": "valid",
                "summary": "Applied a focused theorem repair.",
                "comments": "",
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            }
        ),
        encoding="utf-8",
    )
    context.write_text(
        json.dumps(
            {
                "request": {"id": 12, "cycle": 7, "kind": "worker"},
                "worker_acceptance": {
                    "validation_execution_plan": [{"kind": "cleanup_preserving"}],
                    "forbid_tablet_changes_when_stuck": True,
                },
                "active_node": "",
                "authorized_nodes": [],
                "configured_targets": [],
                "current_present_nodes": [],
                "current_proof_nodes": [],
                "current_deps": {},
                "current_semantic_deps": {},
                "current_target_claims": {},
                "before_snapshot": {},
                "baseline_errors": [],
                "imports_before": [],
                "expected_active_hash": "",
                "baseline_declaration_hashes": {},
                "baseline_correspondence_hashes": {},
            }
        ),
        encoding="utf-8",
    )

    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "check_trellis_worker_result_ok",
            "output": {
                "ok": True,
                "errors": [],
                "data": json.loads(raw.read_text(encoding="utf-8")),
                "response": {"kind": "worker", "outcome": "Valid"},
                "validation_step_results": [],
                "contract_errors": [],
                "validation_errors": [],
                "final_outcome": "valid",
            },
        },
    ) as mock_kernel:
        exit_code = check_main(
            [
                "trellis-worker-result",
                str(raw),
                "--repo",
                str(repo),
                "--context-json",
                str(context),
            ]
        )

    assert exit_code == 0
    payload = mock_kernel.call_args.args[0]
    assert payload["action"] == "check_trellis_worker_result"
    assert payload["repo_path"] == str(repo)
    assert payload["acceptance_context"]["request"]["id"] == 12


def test_check_main_worker_result_raw_only_uses_context_aware_validation() -> None:
    repo = _tmp_repo()
    raw = repo / "worker.raw.json"
    context = repo / "worker.context.json"
    raw.write_text(
        json.dumps(
            {
                "outcome": "invalid",
                "summary": "cleanup attempt failed cleanly",
                "comments": "",
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            }
        ),
        encoding="utf-8",
    )
    context.write_text(
        json.dumps(
            {
                "worker_acceptance": {
                    "validation_kind": "cleanup",
                }
            }
        ),
        encoding="utf-8",
    )

    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "validate_trellis_worker_result_ok",
            "output": {
                "ok": True,
                "errors": [],
                "data": json.loads(raw.read_text(encoding="utf-8")),
            },
        },
    ) as mock_kernel:
        exit_code = check_main(
            [
                "trellis-worker-result",
                str(raw),
                "--context-json",
                str(context),
                "--raw-only",
            ]
        )

    assert exit_code == 0
    payload = mock_kernel.call_args.args[0]
    assert payload["action"] == "validate_trellis_worker_result"
    assert payload["acceptance_context"]["worker_acceptance"]["validation_kind"] == "cleanup"


def test_check_main_reviewer_result_uses_single_kernel_check_action() -> None:
    repo = _tmp_repo()
    raw = repo / "review.raw.json"
    context = repo / "review.context.json"
    raw.write_text(
        json.dumps(
            {
                "decision": "continue",
                "reason": "Keep working the current blocker.",
                "task_blocker_ids": [],
                "reset_blocker_ids": [],
                "next_active": "main",
                "next_mode": "targeted",
                "reset": "none",
                "difficulty_updates": {},
                "allow_new_obligations": True,
                "must_close_active": False,
                "clear_human_input": False,
            }
        ),
        encoding="utf-8",
    )
    context.write_text(
        json.dumps(
            {
                "id": 7,
                "kind": "review",
                "cycle": 12,
                "phase": "theorem_stating",
                "allowed_decisions": ["continue"],
                "allowed_next_modes": ["targeted"],
                "kernel_hinted_next_active_nodes": ["main"],
                "targeted_next_active_nodes": ["main"],
                "allow_targeted_without_next_active": False,
                "allowed_resets": ["none"],
                "allowed_reset_blockers": [],
                "allowed_difficulty_update_nodes": [],
                "blockers": [],
                "current_present_nodes": [],
                "current_proof_nodes": [],
                "current_node_kinds": {},
                "current_deps": {},
                "current_semantic_deps": {},
                "current_target_claims": {},
                "human_input_outstanding": False,
                "worker_context": {
                    "enabled": False,
                    "active_difficulty": "hard",
                    "active_easy_attempts": 0,
                    "worker_profile": "none",
                    "validation_kind": "none",
                    "authorized_nodes": [],
                },
                "worker_acceptance": {
                    "enabled": False,
                    "validation_kind": "none",
                    "authorized_nodes": [],
                    "validation_execution_plan": [],
                    "require_explicit_semantic_deps_for_new_nodes": True,
                    "require_explicit_semantic_deps_for_changed_direct_deps": True,
                    "require_explicit_target_claims_for_new_nodes": True,
                    "forbid_tablet_changes_when_stuck": True,
                    "observation_plan": {
                        "capture_before_snapshot": False,
                        "capture_scoped_tablet_baseline_errors": False,
                        "scoped_tablet_baseline_scope": "none",
                        "capture_imports_before": False,
                        "capture_expected_active_hash": False,
                        "capture_baseline_declaration_hashes": False,
                        "capture_baseline_correspondence_hashes": False,
                    },
                },
            }
        ),
        encoding="utf-8",
    )

    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "check_trellis_reviewer_result_ok",
            "output": {
                "ok": True,
                "errors": [],
                "data": json.loads(raw.read_text(encoding="utf-8")),
                "response": {"kind": "review", "decision": "Continue"},
            },
        },
    ) as mock_kernel:
        exit_code = check_main(
            [
                "trellis-reviewer-result",
                str(raw),
                "--context-json",
                str(context),
            ]
        )

    assert exit_code == 0
    payload = mock_kernel.call_args.args[0]
    assert payload["action"] == "check_trellis_reviewer_result"
    assert payload["review_request"]["id"] == 7
    assert payload["raw_payload"]["next_mode"] == "targeted"


def test_check_node_and_tablet_wrappers_use_kernel() -> None:
    repo = _tmp_repo()

    with patch(
        "trellis.checking.run_kernel_cli",
        side_effect=[
            {
                "status": "check_node_ok",
                "output": {
                    "ok": True,
                    "errors": [],
                    "warnings": [],
                    "compiles": True,
                    "sorry_free": True,
                    "keyword_clean": True,
                    "imports_valid": True,
                    "declaration_intact": True,
                    "marker_valid": True,
                    "declaration_name_matches": True,
                    "tex_format_valid": True,
                    "axioms_valid": True,
                    "audited_axioms": [],
                    "axiom_violations": [],
                    "import_violations": [],
                    "forbidden_hits": [],
                    "sorry_warnings": [],
                    "build_output": "",
                },
            },
            {
                "status": "check_tablet_ok",
                "output": {
                    "ok": True,
                    "errors": [],
                    "warnings": [],
                    "error_records": [],
                    "build_output": "",
                },
            },
        ],
    ):
        node_result = check_node(repo, "n1")
        tablet_result = check_tablet(repo)

    assert node_result["ok"]
    assert tablet_result["ok"]


def test_check_tablet_scoped_uses_kernel() -> None:
    repo = _tmp_repo()
    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "check_tablet_scoped_ok",
            "output": {
                "ok": True,
                "errors": [],
                "warnings": [],
                "all_errors": [],
                "error_records": [],
                "allowed_nodes": ["n1"],
                "build_output": "",
            },
        },
    ) as mock_kernel:
        result = check_tablet_scoped(repo, baseline_errors=[], allowed_nodes=["n1"])

    assert result["ok"]
    payload = mock_kernel.call_args.args[0]
    assert payload["action"] == "check_tablet_scoped"
    assert payload["allowed_nodes"] == ["n1"]


def test_observe_lean_semantic_payloads_returns_raw_node_map() -> None:
    repo = _tmp_repo()
    fake_script = repo / "fake_semantic_payloads.lean"
    fake_script.write_text("-- stub\n", encoding="utf-8")
    seen_args: list[list[str]] = []

    def _fake_run(_repo: Path, args: list[str], *, timeout_secs: float, bwrap_role=None) -> dict[str, object]:
        seen_args.append(args)
        node_name = args[-1]
        if node_name == "alpha":
            return {
                "returncode": 0,
                "stdout": "FP\talpha\tpayload-alpha\n",
                "stderr": "",
                "timed_out": False,
                "spawn_error": "",
            }
        return {
            "returncode": 0,
            "stdout": "ERR\tbeta\tmissing declaration\n",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    with patch.object(
        atomic_observations,
        "_run_lake_command",
        side_effect=_fake_run,
    ) as mock_run, patch.object(
        atomic_observations,
        "_lean_semantic_fingerprint_script_path",
        return_value=fake_script,
    ):
        result = atomic_observations.observe_lean_semantic_payloads(
            repo, ["alpha", "beta"], bwrap_role="lake_compiler"
        )

    assert result == {
        "alpha": {"ok": True, "payload": "payload-alpha", "error": ""},
        "beta": {"ok": False, "payload": "", "error": "missing declaration"},
    }
    assert mock_run.call_count == 2
    assert seen_args == [
        ["env", "lean", "--run", str(fake_script), "alpha"],
        ["env", "lean", "--run", str(fake_script), "beta"],
    ]


def test_check_main_routes_lean_semantic_payloads_command(capsys) -> None:
    repo = _tmp_repo()
    expected = {
        "alpha": {"ok": True, "payload": "payload-alpha", "error": ""},
        "beta": {"ok": False, "payload": "", "error": "missing declaration"},
    }
    with patch(
        "trellis.atomic_actions.cli.observe_lean_semantic_payloads",
        return_value=expected,
    ) as mock_observe:
        exit_code = check_main(
            [
                "lean-semantic-payloads",
                str(repo),
                "--node",
                "alpha",
                "--node",
                "beta",
            ]
        )

    assert exit_code == 0
    assert json.loads(capsys.readouterr().out) == expected
    args = mock_observe.call_args.args
    assert args[0] == repo.resolve()
    assert args[1] == ["alpha", "beta"]


def _write_stub_olean_for(repo: Path, node: str) -> None:
    """Drop a stub olean blob at the supervisor's expected path. The
    new batched ``materialize_tablet_oleans`` uses a stat-walk to detect
    materialized nodes after the lake call returns, so test stubs that
    fake out ``_run_lake_command`` must also leave oleans on disk for
    the post-call walk to surface them."""
    olean = atomic_observations._tablet_olean_path(repo, node)
    olean.parent.mkdir(parents=True, exist_ok=True)
    olean.write_bytes(b"stub-olean-" + node.encode())


def test_materialize_tablet_oleans_invokes_single_batched_lake_build() -> None:
    """The new batched materialize path issues exactly one ``lake build``
    invocation with every Tablet target, not a per-node loop. Targets
    must follow the dependency-order topology so lake's job graph
    schedules deps first."""
    repo = _tmp_repo()
    tablet = repo / "Tablet"
    (tablet / "Preamble.lean").write_text("import Mathlib.Data.Nat.Basic\n", encoding="utf-8")
    (tablet / "A.lean").write_text("import Tablet.Preamble\n", encoding="utf-8")
    (tablet / "B.lean").write_text("import Tablet.A\n", encoding="utf-8")

    seen_args: list[list[str]] = []

    def _fake_run(repo_path: Path, args: list[str], *, timeout_secs: float, bwrap_role=None) -> dict[str, object]:
        seen_args.append(args)
        # The new batched call expects oleans on disk for the stat-walk.
        for node in ("Preamble", "A", "B"):
            _write_stub_olean_for(repo, node)
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    with patch.object(atomic_observations, "_run_lake_command", side_effect=_fake_run):
        # bwrap_role="lake_compiler" routes the direct-lake batched-build
        # path under test (with bwrap_role=None the checker socket is
        # mandatory and the call would raise).
        payload = atomic_observations.materialize_tablet_oleans(
            repo, ["B"], bwrap_role="lake_compiler"
        )

    assert sorted(payload["materialized_nodes"]) == ["A", "B", "Preamble"]
    # Exactly one lake invocation, with build + all three targets.
    assert len(seen_args) == 1
    args = seen_args[0]
    assert args[0] == "build"
    targets = [a for a in args if a.startswith("Tablet.")]
    # Targets must be in dependency order so lake's job graph respects
    # the closure topology even before its own analysis kicks in.
    assert targets == ["Tablet.Preamble", "Tablet.A", "Tablet.B"]


def test_materialize_tablet_oleans_partial_success_reflected_in_materialized_nodes() -> None:
    """When lake exits non-zero (e.g. one node failed to compile), the
    stat-walk must still surface every olean that landed before the
    failure point. ``materialized_nodes`` reflects on-disk truth, not
    lake's exit code."""
    repo = _tmp_repo()
    tablet = repo / "Tablet"
    (tablet / "Preamble.lean").write_text("import Mathlib.Data.Nat.Basic\n", encoding="utf-8")
    (tablet / "SyntheticAsymptoticsProbe.lean").write_text(
        (
            "import Tablet.Preamble\n"
            "import Mathlib.Analysis.Asymptotics.IsLittleO\n\n"
            "theorem SyntheticAsymptoticsProbe : True := by\n"
            "  trivial\n"
        ),
        encoding="utf-8",
    )

    def _fake_run(repo_path: Path, args: list[str], *, timeout_secs: float, bwrap_role=None) -> dict[str, object]:
        # Simulate partial success: Preamble built, the probe failed.
        _write_stub_olean_for(repo, "Preamble")
        return {
            "returncode": 1,
            "stdout": "",
            "stderr": (
                "error: object file "
                "/tmp/repo/.lake/packages/mathlib/.lake/build/lib/lean/Mathlib/Analysis/Asymptotics/IsLittleO.olean "
                "of module Mathlib.Analysis.Asymptotics.IsLittleO does not exist"
            ),
            "timed_out": False,
            "spawn_error": "",
        }

    with patch.object(atomic_observations, "_run_lake_command", side_effect=_fake_run):
        payload = atomic_observations.materialize_tablet_oleans(
            repo, ["SyntheticAsymptoticsProbe"], bwrap_role="lake_compiler"
        )

    # rc surfaces lake's failure unchanged.
    assert payload["returncode"] == 1, payload
    # The stat-walk surfaces the olean that did land pre-failure.
    assert payload["materialized_nodes"] == ["Preamble"]


def test_compile_node_invokes_single_batched_lake_build() -> None:
    """compile_node delegates to materialize_tablet_oleans; the closure
    walk must materialize the full dependency chain via a single
    batched ``lake build`` invocation."""
    repo = _tmp_repo()
    tablet = repo / "Tablet"
    (tablet / "Preamble.lean").write_text("import Mathlib.Data.Nat.Basic\n", encoding="utf-8")
    (tablet / "A.lean").write_text("import Tablet.Preamble\n", encoding="utf-8")
    (tablet / "B.lean").write_text("import Tablet.A\n", encoding="utf-8")

    seen_args: list[list[str]] = []

    def _fake_run(repo_path: Path, args: list[str], *, timeout_secs: float, bwrap_role=None) -> dict[str, object]:
        seen_args.append(args)
        for node in ("Preamble", "A", "B"):
            _write_stub_olean_for(repo, node)
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    with patch.object(atomic_observations, "_run_lake_command", side_effect=_fake_run):
        payload = atomic_observations.compile_node(repo, "B", bwrap_role="lake_compiler")

    assert payload["node"] == "B"
    assert payload["requested_nodes"] == ["B"]
    assert sorted(payload["materialized_nodes"]) == ["A", "B", "Preamble"]
    assert len(seen_args) == 1
    args = seen_args[0]
    assert args[0] == "build"
    targets = [a for a in args if a.startswith("Tablet.")]
    assert targets == ["Tablet.Preamble", "Tablet.A", "Tablet.B"]


def test_check_main_routes_materialize_tablet_oleans_command(capsys) -> None:
    repo = _tmp_repo()
    expected = {
        "requested_nodes": ["B"],
        "materialized_nodes": ["Preamble", "A", "B"],
        "returncode": 0,
        "stdout": "",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }
    with patch(
        "trellis.atomic_actions.cli.materialize_tablet_oleans",
        return_value=expected,
    ) as mock_materialize:
        exit_code = check_main(
            [
                "materialize-tablet-oleans",
                str(repo),
                "--node",
                "B",
            ]
        )

    assert exit_code == 0
    assert json.loads(capsys.readouterr().out) == expected
    args = mock_materialize.call_args.args
    assert args[0] == repo.resolve()
    assert args[1] == ["B"]


def test_check_main_materialize_tablet_oleans_uses_long_default_timeout() -> None:
    repo = _tmp_repo()
    expected = {
        "requested_nodes": [],
        "materialized_nodes": [],
        "returncode": 0,
        "stdout": "",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }
    with patch(
        "trellis.atomic_actions.cli.materialize_tablet_oleans",
        return_value=expected,
    ) as mock_materialize:
        exit_code = check_main(
            [
                "materialize-tablet-oleans",
                str(repo),
            ]
        )

    assert exit_code == 0
    assert mock_materialize.call_args.kwargs["timeout_secs"] == LEAN_SUPPORT_TIMEOUT_SECS


def test_write_scripts_installs_agent_check_wrapper() -> None:
    repo = _tmp_repo()
    state_dir = repo / ".trellis"
    with patch("trellis.checking.materialize_project_runtime") as mock_materialize:
        write_scripts(repo, state_dir)

    mock_materialize.assert_called_once()
    script = state_dir / "scripts" / "check.py"
    assert script.exists()
    assert "trellis.agent_check" in script.read_text(encoding="utf-8")


def test_generated_check_py_pins_kernel_to_repo_local_bin_even_when_src_escapes(
    tmp_path,
) -> None:
    """The generated check.py must resolve the kernel to its OWN repo's runtime
    bin regardless of where `from trellis` resolves source_root. Reproduce the
    field failure (runtime/src symlinked to a developer checkout, so source_root
    escapes) and assert the kernel is still pinned to the repo-local bin."""
    import os
    import subprocess

    repo = tmp_path / "repo"
    state_dir = repo / ".trellis"
    (state_dir / "scripts").mkdir(parents=True)
    runtime_bin = state_dir / "runtime" / "bin" / "trellis_runtime_cli"
    runtime_bin.parent.mkdir(parents=True)
    runtime_bin.write_text("#!/bin/sh\n", encoding="utf-8")
    runtime_bin.chmod(0o755)

    # A "developer checkout" with a stub trellis package, and runtime/src as a
    # symlink to it -> check.py's `from trellis` resolves source_root there.
    checkout = tmp_path / "trellis-checkout"
    (checkout / "trellis").mkdir(parents=True)
    (checkout / "trellis" / "__init__.py").write_text("", encoding="utf-8")
    (checkout / "trellis" / "agent_check.py").write_text(
        "import os\n"
        "def main():\n"
        "    print('KERNEL_CMD=' + os.environ.get('TRELLIS_TRELLIS_KERNEL_CMD', ''))\n"
        "    return 0\n",
        encoding="utf-8",
    )
    (state_dir / "runtime" / "src").symlink_to(checkout)

    with patch("trellis.checking.materialize_project_runtime"):
        write_scripts(repo, state_dir)

    content = (state_dir / "scripts" / "check.py").read_text(encoding="utf-8")
    assert "TRELLIS_TRELLIS_KERNEL_CMD" in content and "runtime" in content

    proc = subprocess.run(
        ["python3", str(state_dir / "scripts" / "check.py")],
        capture_output=True,
        text=True,
        env={k: v for k, v in os.environ.items() if k != "TRELLIS_TRELLIS_KERNEL_CMD"},
    )
    assert f"KERNEL_CMD={runtime_bin}" in proc.stdout, proc.stdout + proc.stderr

    # An INHERITED value (the supervisor's host kernel path, launched from a
    # checkout) is NOT valid inside the worker sandbox — check.py must override
    # it with the repo-local bin, not defer to it.
    env = dict(os.environ)
    env["TRELLIS_TRELLIS_KERNEL_CMD"] = "/nonexistent/checkout/kernel/target/debug/trellis_runtime_cli"
    proc = subprocess.run(
        ["python3", str(state_dir / "scripts" / "check.py")],
        capture_output=True,
        text=True,
        env=env,
    )
    assert f"KERNEL_CMD={runtime_bin}" in proc.stdout, proc.stdout + proc.stderr


def test_materialize_project_runtime_vendors_kernel_source_and_active_binary(monkeypatch) -> None:
    repo = _tmp_repo()
    state_dir = repo / ".trellis"
    fake_kernel = repo / "fake-kernel-bin"
    fake_kernel.write_bytes(b"kernel-binary")
    fake_kernel.chmod(0o755)
    monkeypatch.setenv("TRELLIS_TRELLIS_KERNEL_CMD", str(fake_kernel))

    materialize_project_runtime(repo, state_dir)

    vendored_manifest = state_dir / "runtime" / "src" / "kernel" / "Cargo.toml"
    vendored_binary = state_dir / "runtime" / "bin" / "trellis_runtime_cli"
    assert vendored_manifest.exists()
    assert vendored_binary.read_bytes() == b"kernel-binary"
    assert vendored_binary.stat().st_mode & 0o111


def test_materialize_project_runtime_ignores_transient_source_scratch(monkeypatch) -> None:
    repo = _tmp_repo()
    state_dir = repo / ".trellis"

    source_root = Path(tempfile.mkdtemp())
    package_src = source_root / "trellis"
    kernel_src = source_root / "kernel"
    skills_src = source_root / "skills"
    package_src.mkdir()
    kernel_src.mkdir()
    skills_src.mkdir()

    (package_src / "__init__.py").write_text("", encoding="utf-8")
    (kernel_src / "Cargo.toml").write_text(
        "[package]\nname = 'stub'\nversion = '0.1.0'\n",
        encoding="utf-8",
    )
    (skills_src / "SKILL.md").write_text("# stub\n", encoding="utf-8")

    transient_dir = kernel_src / ".tmp-tests" / ".tmpI5U5tW"
    transient_dir.mkdir(parents=True)
    (transient_dir / "scratch.txt").write_text("scratch\n", encoding="utf-8")

    script_src = source_root / "lean_semantic_fingerprint.lean"
    script_src.write_text("-- stub\n", encoding="utf-8")

    monkeypatch.setattr(runtime_snapshot, "PACKAGE_SOURCE_DIR", package_src)
    monkeypatch.setattr(runtime_snapshot, "KERNEL_SOURCE_DIR", kernel_src)
    monkeypatch.setattr(runtime_snapshot, "SKILLS_SOURCE_DIR", skills_src)
    monkeypatch.setattr(runtime_snapshot, "SCRIPT_SOURCES", (script_src,))
    monkeypatch.delenv("TRELLIS_TRELLIS_KERNEL_CMD", raising=False)

    materialize_project_runtime(repo, state_dir)

    vendored_kernel = state_dir / "runtime" / "src" / "kernel"
    assert (vendored_kernel / "Cargo.toml").exists()
    assert not (vendored_kernel / ".tmp-tests").exists()


def test_materialize_project_runtime_ignores_editor_lockfiles(monkeypatch) -> None:
    repo = _tmp_repo()
    state_dir = repo / ".trellis"

    source_root = Path(tempfile.mkdtemp())
    package_src = source_root / "trellis"
    kernel_src = source_root / "kernel"
    skills_src = source_root / "skills"
    prompt_dir = package_src / "prompt_fragments" / "worker" / "proof_formalization"
    package_src.mkdir()
    kernel_src.mkdir()
    skills_src.mkdir()
    prompt_dir.mkdir(parents=True)

    (package_src / "__init__.py").write_text("", encoding="utf-8")
    (prompt_dir / "05_scope_local.md").write_text("real prompt\n", encoding="utf-8")
    (prompt_dir / ".#05_scope_local.md").write_text("editor lock\n", encoding="utf-8")
    (kernel_src / "Cargo.toml").write_text(
        "[package]\nname = 'stub'\nversion = '0.1.0'\n",
        encoding="utf-8",
    )
    (skills_src / "SKILL.md").write_text("# stub\n", encoding="utf-8")

    script_src = source_root / "lean_semantic_fingerprint.lean"
    script_src.write_text("-- stub\n", encoding="utf-8")

    monkeypatch.setattr(runtime_snapshot, "PACKAGE_SOURCE_DIR", package_src)
    monkeypatch.setattr(runtime_snapshot, "KERNEL_SOURCE_DIR", kernel_src)
    monkeypatch.setattr(runtime_snapshot, "SKILLS_SOURCE_DIR", skills_src)
    monkeypatch.setattr(runtime_snapshot, "SCRIPT_SOURCES", (script_src,))
    monkeypatch.delenv("TRELLIS_TRELLIS_KERNEL_CMD", raising=False)

    materialize_project_runtime(repo, state_dir)

    vendored_prompt_dir = (
        state_dir
        / "runtime"
        / "src"
        / "trellis"
        / "prompt_fragments"
        / "worker"
        / "proof_formalization"
    )
    assert (vendored_prompt_dir / "05_scope_local.md").read_text(encoding="utf-8") == "real prompt\n"
    assert not (vendored_prompt_dir / ".#05_scope_local.md").exists()


def test_materialize_project_runtime_vendors_filespec_and_backfills_repo_copy(monkeypatch) -> None:
    repo = _tmp_repo()
    state_dir = repo / ".trellis"

    source_root = Path(tempfile.mkdtemp())
    package_src = source_root / "trellis"
    kernel_src = source_root / "kernel"
    skills_src = source_root / "skills"
    filespec_src = source_root / "FILESPEC.md"
    package_src.mkdir()
    kernel_src.mkdir()
    skills_src.mkdir()

    (package_src / "__init__.py").write_text("", encoding="utf-8")
    (kernel_src / "Cargo.toml").write_text(
        "[package]\nname = 'stub'\nversion = '0.1.0'\n",
        encoding="utf-8",
    )
    (skills_src / "SKILL.md").write_text("# stub\n", encoding="utf-8")
    filespec_src.write_text("# filespec\n", encoding="utf-8")

    script_src = source_root / "lean_semantic_fingerprint.lean"
    script_src.write_text("-- stub\n", encoding="utf-8")

    monkeypatch.setattr(runtime_snapshot, "PACKAGE_SOURCE_DIR", package_src)
    monkeypatch.setattr(runtime_snapshot, "KERNEL_SOURCE_DIR", kernel_src)
    monkeypatch.setattr(runtime_snapshot, "SKILLS_SOURCE_DIR", skills_src)
    monkeypatch.setattr(runtime_snapshot, "DOC_SOURCES", (filespec_src,))
    monkeypatch.setattr(runtime_snapshot, "SCRIPT_SOURCES", (script_src,))
    monkeypatch.delenv("TRELLIS_TRELLIS_KERNEL_CMD", raising=False)

    materialize_project_runtime(repo, state_dir)

    assert (repo / "FILESPEC.md").read_text(encoding="utf-8") == "# filespec\n"
    assert (state_dir / "runtime" / "src" / "FILESPEC.md").read_text(encoding="utf-8") == "# filespec\n"


def test_kernel_cli_command_prefers_vendored_runtime_binary(monkeypatch) -> None:
    repo = _tmp_repo()
    runtime_dir = repo / ".trellis" / "runtime"
    src_root = runtime_dir / "src"
    module_path = src_root / "trellis" / "runtime" / "kernel_cli.py"
    module_path.parent.mkdir(parents=True, exist_ok=True)
    module_path.write_text("# stub\n", encoding="utf-8")
    vendored_binary = runtime_dir / "bin" / "trellis_runtime_cli"
    vendored_binary.parent.mkdir(parents=True, exist_ok=True)
    vendored_binary.write_text("binary\n", encoding="utf-8")
    monkeypatch.delenv("TRELLIS_TRELLIS_KERNEL_CMD", raising=False)
    monkeypatch.setattr(kernel_cli, "__file__", str(module_path))

    assert kernel_cli.kernel_cli_command() == [str(vendored_binary)]


def test_kernel_cli_command_falls_back_to_vendored_manifest(monkeypatch) -> None:
    repo = _tmp_repo()
    runtime_dir = repo / ".trellis" / "runtime"
    src_root = runtime_dir / "src"
    module_path = src_root / "trellis" / "runtime" / "kernel_cli.py"
    module_path.parent.mkdir(parents=True, exist_ok=True)
    module_path.write_text("# stub\n", encoding="utf-8")
    manifest = src_root / "kernel" / "Cargo.toml"
    manifest.parent.mkdir(parents=True, exist_ok=True)
    manifest.write_text("[package]\nname = 'stub'\nversion = '0.1.0'\n", encoding="utf-8")
    monkeypatch.delenv("TRELLIS_TRELLIS_KERNEL_CMD", raising=False)
    monkeypatch.setattr(kernel_cli, "__file__", str(module_path))
    monkeypatch.setattr("shutil.which", lambda name: "/usr/bin/cargo" if name == "cargo" else None)

    assert kernel_cli.kernel_cli_command() == [
        "/usr/bin/cargo",
        "run",
        "--quiet",
        "--manifest-path",
        str(manifest),
        "--bin",
        "trellis_runtime_cli",
    ]
