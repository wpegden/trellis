"""Focused tests for the active kernel-backed checker wrapper."""

from __future__ import annotations

import json
import re
import tempfile
from pathlib import Path
from unittest.mock import patch

import trellis.atomic_actions.observations as atomic_observations
import trellis.checking as checking
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
    # The dispatch script is part of the olean source-closure content hash
    # (olean-staleness fix); without it ``tablet_source_closure_hash`` is
    # None (fail-closed) and no olean is ever reported current.
    (repo / ".trellis" / "scripts").mkdir(parents=True)
    (repo / ".trellis" / "scripts" / "check.py").write_text(
        "#!/usr/bin/env python3\n", encoding="utf-8"
    )
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

    def _fake_run(_repo: Path, args: list[str], *, timeout_secs: float, bwrap_role=None, metrics=None) -> dict[str, object]:
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
        ["env", "lean", "--run", str(fake_script), "alpha", "alpha"],
        ["env", "lean", "--run", str(fake_script), "beta", "beta"],
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


def _stub_kernel_replay_ok(
    _repo: Path,
    node_names,
    *,
    timeout_secs: float,
    bwrap_role=None,
) -> dict[str, object]:
    """Successful replay prerequisite for tests focused on build batching."""
    return {
        "checked_nodes": list(node_names),
        "returncode": 0,
        "stdout": "",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }


def test_materialize_tablet_oleans_invokes_single_batched_lake_build() -> None:
    """Materialization issues one batched ``lake build``, not a per-node loop.

    Independent kernel replay is a separate required command and is mocked
    here so this test remains focused on build batching and target order.
    """
    repo = _tmp_repo()
    tablet = repo / "Tablet"
    (tablet / "Preamble.lean").write_text("import Mathlib.Data.Nat.Basic\n", encoding="utf-8")
    (tablet / "A.lean").write_text("import Tablet.Preamble\n", encoding="utf-8")
    (tablet / "B.lean").write_text("import Tablet.A\n", encoding="utf-8")

    seen_args: list[list[str]] = []

    def _fake_run(repo_path: Path, args: list[str], *, timeout_secs: float, bwrap_role=None, metrics=None) -> dict[str, object]:
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

    with patch.object(
        atomic_observations, "_run_lake_command", side_effect=_fake_run
    ), patch.object(
        atomic_observations,
        "_kernel_replay_tablet_modules",
        side_effect=_stub_kernel_replay_ok,
    ):
        # bwrap_role="lake_compiler" routes the direct-lake batched-build
        # path under test (with bwrap_role=None the checker socket is
        # mandatory and the call would raise).
        payload = atomic_observations.materialize_tablet_oleans(
            repo, ["B"], bwrap_role="lake_compiler"
        )

    assert sorted(payload["materialized_nodes"]) == ["A", "B", "Preamble"]
    # Exactly one build invocation, with all three targets.
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

    def _fake_run(repo_path: Path, args: list[str], *, timeout_secs: float, bwrap_role=None, metrics=None) -> dict[str, object]:
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

    with patch.object(
        atomic_observations, "_run_lake_command", side_effect=_fake_run
    ), patch.object(
        atomic_observations,
        "_kernel_replay_tablet_modules",
        side_effect=_stub_kernel_replay_ok,
    ):
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

    def _fake_run(repo_path: Path, args: list[str], *, timeout_secs: float, bwrap_role=None, metrics=None) -> dict[str, object]:
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

    with patch.object(
        atomic_observations, "_run_lake_command", side_effect=_fake_run
    ), patch.object(
        atomic_observations,
        "_kernel_replay_tablet_modules",
        side_effect=_stub_kernel_replay_ok,
    ):
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


def test_write_scripts_installs_incremental_check_wrapper() -> None:
    repo = _tmp_repo()
    state_dir = repo / ".trellis"
    with patch("trellis.checking.materialize_project_runtime"):
        write_scripts(repo, state_dir)

    module = state_dir / "scripts" / "incremental_check.py"
    wrapper = state_dir / "scripts" / "incremental-check"
    assert module.exists() and wrapper.exists()
    # The wrapper forwards to the provisioned module and pins the repo root.
    wrapper_text = wrapper.read_text(encoding="utf-8")
    assert "incremental_check.py" in wrapper_text
    assert "--repo" in wrapper_text
    # The provisioned module must be self-contained (no `from trellis` import)
    # so it runs standalone inside the worker sandbox.
    module_text = module.read_text(encoding="utf-8")
    assert "from trellis" not in module_text
    assert "import trellis" not in module_text
    # Both are executable.
    assert module.stat().st_mode & 0o111
    assert wrapper.stat().st_mode & 0o111


def _write_repo_config(repo: Path, *, target: str, warm: bool) -> None:
    """Write a minimal trellis.config.json selecting the backend + warm flag."""
    cfg: dict = {"workflow": {"default_target": target}}
    if warm:
        cfg["isabelle_warm_session"] = {"enabled": True}
    (repo / "trellis.config.json").write_text(json.dumps(cfg), encoding="utf-8")


def test_write_scripts_lean_incremental_check_is_lean_body_byte_for_byte() -> None:
    """Phase 3 backend gate — the Lean path is UNCHANGED. A Lean repo (the
    default; no config) gets the Lean `incremental_check.py` body + a wrapper that
    execs it. This pins the Lean/flag-OFF path so the Isabelle gate cannot perturb
    it."""
    from trellis.checking import generate_incremental_check_sh

    repo = _tmp_repo()  # no trellis.config.json -> default_target "lean"
    state_dir = repo / ".trellis"
    with patch("trellis.checking.materialize_project_runtime"):
        write_scripts(repo, state_dir)

    scripts = state_dir / "scripts"
    # The Lean module is provisioned (NOT the isabelle one).
    assert (scripts / "incremental_check.py").exists()
    assert not (scripts / "isabelle_incremental_check.py").exists()
    wrapper_text = (scripts / "incremental-check").read_text(encoding="utf-8")
    assert "incremental_check.py" in wrapper_text
    assert "isabelle_incremental_check.py" not in wrapper_text
    # The generator returns the exact Lean body (byte-for-byte the pre-Phase-3
    # text — the "Advisory warm-server Lean pre-check" comment is the marker).
    body = generate_incremental_check_sh(repo, state_dir)
    assert "Advisory warm-server Lean pre-check" in body
    assert "incremental_check.py" in body


def test_write_scripts_isabelle_warm_on_emits_isabelle_advisory_forwarder() -> None:
    """Phase 3 — an isabelle_hol run WITH the warm flag ON gets the WARM ISABELLE
    advisory: the wrapper execs isabelle_incremental_check.py (which drives the
    isabelle-warm-advisory socket op), and that standalone module is provisioned
    and self-contained."""
    from trellis.checking import generate_incremental_check_sh

    repo = _tmp_repo()
    _write_repo_config(repo, target="isabelle_hol", warm=True)
    state_dir = repo / ".trellis"
    with patch("trellis.checking.materialize_project_runtime"):
        write_scripts(repo, state_dir)

    scripts = state_dir / "scripts"
    module = scripts / "isabelle_incremental_check.py"
    assert module.exists()
    # The Lean module is NOT provisioned on this path.
    assert not (scripts / "incremental_check.py").exists()
    wrapper_text = (scripts / "incremental-check").read_text(encoding="utf-8")
    assert "isabelle_incremental_check.py" in wrapper_text
    assert "Advisory warm Isabelle pre-check" in wrapper_text
    # Standalone (no trellis import) so it runs inside the worker sandbox.
    module_text = module.read_text(encoding="utf-8")
    assert "from trellis" not in module_text and "import trellis" not in module_text
    # It drives the warm-advisory op over the socket (not the cert/gate op, and
    # NOT a spawned lean/lake/isabelle process — the warm session lives in the
    # supervisor checker, so the worker side only opens a socket).
    assert "isabelle_warm_advisory" in module_text
    assert "import subprocess" not in module_text
    assert "lake env lean" not in module_text
    assert module.stat().st_mode & 0o111
    # The generator selects the Isabelle body.
    body = generate_incremental_check_sh(repo, state_dir)
    assert "Advisory warm Isabelle pre-check" in body


def test_write_scripts_isabelle_warm_off_keeps_lean_body() -> None:
    """Phase 3 flag gate — an isabelle_hol run with the warm flag OFF leaves the
    Lean `incremental-check` body byte-for-byte (no warm prefix exists to advise
    against; the worker uses `isabelle build`). The backend swap is gated on the
    warm flag, not merely the backend."""
    from trellis.checking import generate_incremental_check_sh

    repo = _tmp_repo()
    _write_repo_config(repo, target="isabelle_hol", warm=False)
    state_dir = repo / ".trellis"
    with patch("trellis.checking.materialize_project_runtime"):
        write_scripts(repo, state_dir)

    scripts = state_dir / "scripts"
    assert (scripts / "incremental_check.py").exists()
    assert not (scripts / "isabelle_incremental_check.py").exists()
    body = generate_incremental_check_sh(repo, state_dir)
    assert "Advisory warm-server Lean pre-check" in body


def test_write_scripts_isabelle_emits_isa_query_discovery_helper() -> None:
    """An isabelle_hol run gets the `isa-query` discovery helper beside
    `incremental-check`. It must carry NO host-specific absolute path (the
    host-local `~/scratch/isa-console/isa_query.sh` it replaces was invisible
    inside the worker sandbox, whose HOME is a per-burst dir, never $HOME)."""
    repo = _tmp_repo()
    _write_repo_config(repo, target="isabelle_hol", warm=True)
    state_dir = repo / ".trellis"
    with patch("trellis.checking.materialize_project_runtime"):
        write_scripts(repo, state_dir)

    helper = state_dir / "scripts" / "isa-query"
    assert helper.exists()
    assert helper.stat().st_mode & 0o111
    text = helper.read_text(encoding="utf-8")

    # No baked host paths: everything resolves from BASH_SOURCE / env.
    assert not re.search(r"/home/[A-Za-z0-9_-]+", text)
    assert "isa-console" not in text
    assert 'TRELLIS_ISABELLE_BIN' in text
    assert 'script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"' in text

    # HEAP POLICY: forcing system heaps would point session OUTPUT at the
    # read-only system heap bind and fail with EROFS. Input lookup already
    # finds the prewarmed images there by default. Check the executable lines
    # only — the rationale is (deliberately) spelled out in the comments.
    code = "\n".join(
        line for line in text.splitlines() if not line.lstrip().startswith("#")
    )
    assert "system_heaps" not in code

    # Scratch lives under the worker-WRITABLE `.trellis/scratch`, not the
    # read-only scripts dir it is executed from.
    assert 'WORK="$state_dir/scratch/isa-query"' in text

    # The discovery commands the skill actually calls.
    for sub in ("find_theorems", "find_consts", "solve_direct", "sledgehammer", "thm_oracles"):
        assert sub in text

    # Regression pin: Isabelle skips executing a session whose source digest is
    # unchanged and exits 0, so a repeated identical query would print "(no
    # output captured)" and read as a dead tool. A per-invocation nonce in the
    # theory keeps the digest unique. Assert the nonce is both generated AND
    # interpolated into the theory (generating it alone would not fix anything).
    assert "NONCE=" in code
    assert "isa-query invocation nonce: $NONCE" in text

    # Regression pin: the lock must be taken BEFORE the shared theory/ROOT/result
    # files are written, or a concurrent query answers the wrong question.
    assert code.index("exec 9>") < code.index('cat > "$SESS_DIR/IsaQuery.thy"')

    # A caught ML exception is written as `ERROR: ...` while Isabelle still
    # reports success; the wrapper must turn that into a non-zero status.
    assert "^ERROR:" in code


def test_generated_isa_query_executes_and_reports_usage(tmp_path) -> None:
    """Execute the emitted helper rather than only grepping it: content
    assertions alone pass against an implementation that is entirely dead code
    (e.g. `exit 99` right after the shebang)."""
    import shutil
    import subprocess

    from trellis.checking import generate_isa_query_sh

    bash = shutil.which("bash")
    assert bash, "bash is required for this test"

    helper = tmp_path / "isa-query"
    helper.write_text(
        generate_isa_query_sh(tmp_path, tmp_path / ".trellis"), encoding="utf-8"
    )
    helper.chmod(0o755)

    # No args -> usage on stdout, exit 2. Catches dead-code implementations.
    no_args = subprocess.run(
        [bash, str(helper)], capture_output=True, text=True, timeout=60
    )
    assert no_args.returncode == 2, no_args.stderr
    assert "Usage:" in no_args.stdout
    assert "find_theorems" in no_args.stdout

    # A real command with no resolvable Isabelle -> the binary check fires,
    # proving arg parsing and the check are live code reached in order.
    # PATH points at an empty dir so the script cannot resolve `isabelle`;
    # bash itself is invoked by absolute path so the harness still works.
    (tmp_path / "empty-bin").mkdir()
    env = {"PATH": str(tmp_path / "empty-bin"), "HOME": str(tmp_path)}
    missing_isa = subprocess.run(
        [bash, str(helper), "find_theorems", "name: foo"],
        capture_output=True,
        text=True,
        timeout=60,
        env=env,
    )
    assert missing_isa.returncode == 2
    assert "no isabelle binary" in missing_isa.stderr


def test_write_scripts_lean_repo_omits_isa_query() -> None:
    """Backend gate — a Lean run's `.trellis/scripts/` is byte-identical to
    before: no `isa-query` is emitted."""
    repo = _tmp_repo()
    _write_repo_config(repo, target="lean", warm=False)
    state_dir = repo / ".trellis"
    with patch("trellis.checking.materialize_project_runtime"):
        write_scripts(repo, state_dir)

    assert not (state_dir / "scripts" / "isa-query").exists()


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


# --------------------------------------------------------------------------
# check.py -> checking.py::main forwarding of the Isabelle backend ops.
#
# The kernel sends Isabelle checker ops via .trellis/scripts/check.py <op>
# (tablet_support.rs::run_repo_command_json). check.py -> agent_check.main ->
# checking.py::main forwards a fixed set of ops to atomic_actions_main, which
# routes the 5 isabelle-* ops through the checker socket. These tests pin the
# (unchanged) Lean forward set and prove the isabelle-* ops now FORWARD (reach
# the socket dispatch in atomic_actions/cli.py) rather than hitting the
# "unknown command" branch in checking.py::main.
# --------------------------------------------------------------------------

_ISABELLE_FORWARDED_OPS = (
    "isabelle-check-node",
    "isabelle-thm-oracles",
    "isabelle-thm-deps",
    "isabelle-build-session",
    "isabelle-sync-session",
)


def test_check_main_lean_forward_set_is_unchanged() -> None:
    """The Lean (and sync-tablet-support) ops forwarded by checking.py::main to
    atomic_actions_main are exactly this set. Adding the Isabelle ops must not
    perturb the Lean forwarding behavior — this pins the Lean side byte-for-byte
    so a regression that drops/renames a Lean op fails loudly."""
    import trellis.checking as checking_mod

    forwarded: list[str] = []

    def _fake_atomic_main(argv):
        forwarded.append(argv[0])
        return 0

    lean_and_local_ops = [
        "lean-compile-node",
        "lean-build-tablet",
        "prepare-compiled-support",
        "materialize-tablet-oleans",
        "print-axioms",
        "local-closure-axioms",
        "lean-semantic-payloads",
        "sync-tablet-support",
    ]
    with patch.object(checking_mod, "atomic_actions_main", _fake_atomic_main):
        for op in lean_and_local_ops:
            assert checking_mod.main([op]) == 0
    assert forwarded == lean_and_local_ops


def test_check_main_forwards_isabelle_ops_to_atomic_actions(monkeypatch) -> None:
    """Each isabelle-* op reaches atomic_actions_main (the forward path), rather
    than the 'unknown command' branch. We intercept atomic_actions_main to prove
    the op is forwarded verbatim without spawning a real checker."""
    import trellis.checking as checking_mod

    for op in _ISABELLE_FORWARDED_OPS:
        seen: dict[str, object] = {}

        def _fake_atomic_main(argv, _seen=seen):
            _seen["argv"] = list(argv)
            return 0

        with patch.object(checking_mod, "atomic_actions_main", _fake_atomic_main):
            # node-name-taking ops need a positional arg to parse; build/sync do
            # not. The forward path is op-name dispatch, so this just feeds a
            # valid argv into atomic_actions_main via the real forward.
            argv = [op]
            if op in {"isabelle-check-node", "isabelle-thm-oracles", "isabelle-thm-deps"}:
                argv.append("Tablet.SomeNode")
            assert checking_mod.main(argv) == 0
        assert seen["argv"] == argv


def test_check_main_isabelle_sync_session_reaches_socket_layer(
    monkeypatch, capsys
) -> None:
    """With TRELLIS_CHECKER_SOCKET pointed at a NONEXISTENT path, the forwarded
    isabelle-sync-session op reaches the socket layer in atomic_actions/cli.py
    and fails with a connection error ('checker socket not found' /
    'supervisor_unavailable') — NOT 'unknown command'. The socket-not-found
    error is the proof of forwarding (no real server is spawned)."""
    missing_socket = Path(tempfile.mkdtemp()) / "nonexistent-checker.sock"
    assert not missing_socket.exists()
    monkeypatch.setenv("TRELLIS_CHECKER_SOCKET", str(missing_socket))

    exit_code = check_main(["isabelle-sync-session"])

    out = capsys.readouterr().out
    assert exit_code == 2
    assert "unknown command" not in out
    payload = json.loads(out)
    assert "checker socket not found" in payload["error"]
    assert "supervisor_unavailable" in payload["error"]


def test_check_main_isabelle_op_unset_socket_is_forwarded_server_only_error(
    monkeypatch, capsys
) -> None:
    """With TRELLIS_CHECKER_SOCKET UNSET, the forwarded isabelle op reaches the
    socket dispatch in atomic_actions/cli.py and reports the 'server-only op'
    error (it got far enough to consult the socket env), NOT 'unknown command'.
    This is the complementary proof that the op is forwarded, not rejected by
    checking.py::main."""
    monkeypatch.delenv("TRELLIS_CHECKER_SOCKET", raising=False)

    exit_code = check_main(["isabelle-build-session"])

    out = capsys.readouterr().out
    assert exit_code == 2
    assert "unknown command" not in out
    payload = json.loads(out)
    assert "server-only op" in payload["error"]


def test_check_main_still_rejects_genuinely_unknown_command(capsys) -> None:
    """A truly unknown command still hits the 'unknown command' branch — the
    forward-set widening did not turn checking.py::main into a pass-through."""
    exit_code = check_main(["definitely-not-a-real-op"])
    out = capsys.readouterr().out
    assert exit_code == 2
    assert "unknown command: definitely-not-a-real-op" in out
def test_normalize_stuck_math_audit_surfaces_kernel_memory_operation_errors() -> None:
    # Regression (stuck-math-audit 2998, cycle 677): the kernel checker
    # rejects an artifact whose memory_operations use short-form entry ids
    # by answering the `_ok` CLI status with the inner verdict
    # {ok: false, errors: [...], data: null}. The normalizer must surface
    # those errors verbatim instead of running the probe-path check on the
    # absent `data` and masking them behind
    # "normalized stuck math audit output is missing data".
    from trellis.checking import normalize_trellis_stuck_math_audit_result_data

    repo = _tmp_repo()
    kernel_errors = [
        "memory_operations[0].entry_id `pm-0086` does not name an existing process-memory entry"
    ]
    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "check_trellis_stuck_math_audit_result_ok",
            "output": {"ok": False, "errors": kernel_errors, "data": None, "response": None},
        },
    ):
        result = normalize_trellis_stuck_math_audit_result_data(
            {"report": "r", "tasks": [], "probe_paths": [], "memory_operations": []},
            audit_request={"id": 2998, "cycle": 677, "kind": "stuck_math_audit"},
            repo=repo,
        )

    assert not result["ok"]
    assert result["errors"] == kernel_errors
    assert all("missing data" not in err for err in result["errors"])


def test_normalize_stuck_math_audit_still_runs_probe_check_on_accepted_artifact() -> None:
    # Companion to the regression above: an accepted artifact (inner
    # ok: true) still goes through the probe-path check against the repo.
    from trellis.checking import normalize_trellis_stuck_math_audit_result_data

    repo = _tmp_repo()
    inner_data = {"report": "r", "tasks": [], "probe_paths": ["nowhere/probe.lean"]}
    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "check_trellis_stuck_math_audit_result_ok",
            "output": {"ok": True, "errors": [], "data": inner_data, "response": {"kind": "stuck_math_audit"}},
        },
    ):
        result = normalize_trellis_stuck_math_audit_result_data(
            dict(inner_data),
            audit_request={"id": 1, "cycle": 2, "kind": "stuck_math_audit"},
            repo=repo,
        )

    assert not result["ok"]
    assert any("probe_paths[0]" in err for err in result["errors"])


def test_validate_json_artifact_stuck_math_raw_path_unwraps_kernel_verdict() -> None:
    # The raw-only (no --context-json) stuck-math check must report the
    # kernel's inner shape-validation verdict; returning the CLI envelope
    # unexamined printed OK for every JSON object.
    repo = _tmp_repo()
    raw = repo / "stuck.raw.json"
    raw.write_text(json.dumps({"cone_clean_node": "", "probe_paths": [], "tasks": []}), encoding="utf-8")

    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "validate_trellis_stuck_math_audit_result_ok",
            "output": {"ok": False, "errors": ["report must be non-empty"], "data": None},
        },
    ):
        result = validate_json_artifact("trellis-stuck-math-audit-result", raw)
    assert not result["ok"]
    assert result["errors"] == ["report must be non-empty"]

    validated = {"report": "## Claim being audited\nx", "tasks": [], "probe_paths": []}
    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "validate_trellis_stuck_math_audit_result_ok",
            "output": {"ok": True, "errors": [], "data": validated},
        },
    ):
        result = validate_json_artifact("trellis-stuck-math-audit-result", raw)
    assert result["ok"]
    assert result["data"] == validated


def test_validate_json_artifact_worker_raw_path_unwraps_kernel_verdict() -> None:
    # Same masking as the stuck-math raw path: the no-context worker check
    # must report the kernel's inner shape-validation verdict instead of
    # returning the CLI envelope (which is ok for every JSON object).
    repo = _tmp_repo()
    raw = repo / "worker.raw.json"
    raw.write_text(json.dumps({"summary": ""}), encoding="utf-8")

    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "validate_trellis_worker_result_ok",
            "output": {"ok": False, "errors": ["outcome must be non-empty"], "data": None},
        },
    ):
        result = validate_json_artifact("trellis-worker-result", raw)
    assert not result["ok"]
    assert result["errors"] == ["outcome must be non-empty"]

    validated = {"outcome": "valid", "summary": "did the thing", "comments": ""}
    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "validate_trellis_worker_result_ok",
            "output": {"ok": True, "errors": [], "data": validated},
        },
    ):
        result = validate_json_artifact("trellis-worker-result", raw)
    assert result["ok"]
    assert result["data"] == validated


def test_validate_json_artifact_worker_raw_only_unwraps_kernel_verdict() -> None:
    # The --raw-only (context-aware) worker self-check had the same defect
    # AND appended the "also run the full check" warning to a payload the
    # kernel had rejected. A rejected payload must FAIL without the warning.
    repo = _tmp_repo()
    raw = repo / "worker.raw.json"
    context = repo / "worker.context.json"
    raw.write_text(json.dumps({"summary": ""}), encoding="utf-8")
    context.write_text(
        json.dumps({"worker_acceptance": {"validation_kind": "cleanup"}}),
        encoding="utf-8",
    )

    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "validate_trellis_worker_result_ok",
            "output": {
                "ok": False,
                "errors": ["outcome must be one of: invalid"],
                "data": None,
            },
        },
    ):
        result = validate_json_artifact(
            "trellis-worker-result", raw, context_json=context, raw_only=True
        )
    assert not result["ok"]
    assert result["errors"] == ["outcome must be one of: invalid"]
    assert "warnings" not in result

    validated = {"outcome": "invalid", "summary": "attempt failed cleanly", "comments": ""}
    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "validate_trellis_worker_result_ok",
            "output": {"ok": True, "errors": [], "data": validated},
        },
    ):
        result = validate_json_artifact(
            "trellis-worker-result", raw, context_json=context, raw_only=True
        )
    assert result["ok"]
    assert result["data"] == validated
    assert any("--raw-only does not check" in w for w in result.get("warnings", []))


def test_validate_json_artifact_reviewer_raw_path_unwraps_kernel_verdict() -> None:
    repo = _tmp_repo()
    raw = repo / "review.raw.json"
    raw.write_text(json.dumps({"decision": "continue"}), encoding="utf-8")

    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "validate_trellis_reviewer_result_ok",
            "output": {"ok": False, "errors": ["reason must be non-empty"], "data": None},
        },
    ):
        result = validate_json_artifact("trellis-reviewer-result", raw)
    assert not result["ok"]
    assert result["errors"] == ["reason must be non-empty"]

    validated = {"decision": "continue", "reason": "keep going", "comments": ""}
    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "validate_trellis_reviewer_result_ok",
            "output": {"ok": True, "errors": [], "data": validated},
        },
    ):
        result = validate_json_artifact("trellis-reviewer-result", raw)
    assert result["ok"]
    assert result["data"] == validated


def test_validate_json_artifact_audit_raw_path_unwraps_kernel_verdict() -> None:
    repo = _tmp_repo()
    raw = repo / "audit.raw.json"
    raw.write_text(json.dumps({"report": ""}), encoding="utf-8")

    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "validate_trellis_audit_result_ok",
            "output": {"ok": False, "errors": ["report must be non-empty"], "data": None},
        },
    ):
        result = validate_json_artifact("trellis-audit-result", raw)
    assert not result["ok"]
    assert result["errors"] == ["report must be non-empty"]

    validated = {"report": "## Findings\nnone", "tasks": []}
    with patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "validate_trellis_audit_result_ok",
            "output": {"ok": True, "errors": [], "data": validated},
        },
    ):
        result = validate_json_artifact("trellis-audit-result", raw)
    assert result["ok"]
    assert result["data"] == validated


def test_generated_isa_query_normalizes_unicode_to_symbol_notation(tmp_path) -> None:
    """A Unicode math character in a query reaches Isabelle as `\\<name>`.

    Isabelle's inner lexer reads symbol notation. A .thy FILE accepts either
    form, but this helper embeds the query in an SML string literal evaluated
    past that decode, so a raw Unicode character died with "Inner lexical
    error" — losing three library probes in one live burst.

    Executes the helper against a stub `isabelle` (a real build is minutes) and
    inspects the theory it writes before building.
    """
    import shutil
    import subprocess

    from trellis.checking import generate_isa_query_sh

    bash = shutil.which("bash")
    assert bash, "bash is required for this test"

    # A stub Isabelle: answers the ISABELLE_HOME probe, no-ops the build.
    home = tmp_path / "isabelle-home"
    (home / "etc").mkdir(parents=True)
    (home / "etc" / "symbols").write_text(
        "# stub table\n"
        "\\<and>                  code: 0x002227  group: logic\n"
        "\\<le>                   code: 0x002264  group: relation\n",
        encoding="utf-8",
    )
    stub = tmp_path / "isabelle"
    stub.write_text(
        "#!/bin/bash\n"
        'if [[ "$1" == "getenv" ]]; then printf "%s\\n" "' + str(home) + '"; exit 0; fi\n'
        "exit 0\n",
        encoding="utf-8",
    )
    stub.chmod(0o755)

    # The helper derives its state dir from its own location, so it must sit
    # where `write_scripts` puts it.
    helper = tmp_path / ".trellis" / "scripts" / "isa-query"
    helper.parent.mkdir(parents=True, exist_ok=True)
    helper.write_text(
        generate_isa_query_sh(tmp_path, tmp_path / ".trellis"), encoding="utf-8"
    )
    helper.chmod(0o755)

    env = {
        "PATH": "/usr/bin:/bin",
        "HOME": str(tmp_path),
        "TRELLIS_ISABELLE_BIN": str(stub),
    }
    subprocess.run(
        [bash, str(helper), "solve_direct", "x ∧ y ≤ z"],
        capture_output=True,
        text=True,
        timeout=120,
        env=env,
    )

    theory = tmp_path / ".trellis" / "scratch" / "isa-query" / "session" / "IsaQuery.thy"
    assert theory.exists(), "helper did not write its scratch theory"
    text = theory.read_text(encoding="utf-8")
    assert "\\<and>" in text and "\\<le>" in text, text
    assert "∧" not in text and "≤" not in text, (
        "raw Unicode reached the embedded query; Isabelle's inner lexer rejects it"
    )


def test_check_node_and_tablet_refuse_every_audit_family_role(monkeypatch, capsys) -> None:
    """The audit bwrap withholds the writable `Tablet/` and the checker socket
    that `check_node` / `check_tablet` need, so the entry points stop at the
    top and say what they are and where the verdicts live.

    The roles are every `burst_role` the kernel emits on a
    `stuck_math_audit_contract` (`kernel/src/request_contracts.rs`); all seven
    route through `_handle_stuck_math_audit` and get that same sandbox shape.
    """
    for role in (
        "stuck_math_audit",
        "need_input_auditor",
        "revision_planner",
        "initial_planner",
        "gap_research",
        "gap_plan_critic",
        "assumptions_lane",
    ):
        monkeypatch.setenv("TRELLIS_SANDBOX_ROLE", role)

        with patch("trellis.checking.run_kernel_cli") as kernel:
            assert check_main(["node", "SomeNode"]) == 1
            assert check_main(["tablet"]) == 1
        kernel.assert_not_called()

        out = capsys.readouterr().out
        assert "supervisor-owned deterministic acceptance check" in out
        assert "node_lane_verdicts" in out
        assert "`check.py node`" in out
        assert "`check.py tablet`" in out


def test_deterministic_check_denied_roles_match_the_kernel_contract() -> None:
    """The deny set is pinned to the `burst_role` literals in
    `kernel/src/request_contracts.rs`.
    """
    contracts = (
        Path(__file__).resolve().parents[1] / "kernel" / "src" / "request_contracts.rs"
    ).read_text(encoding="utf-8")
    emitted: set[str] = set()
    for line in contracts.splitlines():
        stripped = line.strip()
        if stripped.startswith('"burst_role":'):
            emitted.update(re.findall(r'"([a-z_]+)"', stripped.split(":", 1)[1]))

    assert emitted == set(checking._DETERMINISTIC_CHECK_DENIED_ROLES)


def test_check_node_and_tablet_pass_through_for_non_audit_roles(monkeypatch) -> None:
    """The refusal is keyed to the audit role alone: every other burst role
    (and an unwrapped host invocation) reaches the kernel unchanged.
    """
    repo = _tmp_repo()
    node_response = {
        "status": "check_node_ok",
        "output": {"ok": True, "errors": [], "warnings": []},
    }
    tablet_response = {
        "status": "check_tablet_ok",
        "output": {"ok": True, "errors": [], "warnings": []},
    }

    for role in ("worker", "lake_compiler", "grunt", "reviewer"):
        monkeypatch.setenv("TRELLIS_SANDBOX_ROLE", role)
        with patch(
            "trellis.checking.run_kernel_cli",
            side_effect=[node_response, tablet_response],
        ) as kernel:
            assert check_main(["node", "SomeNode", str(repo)]) == 0
            assert check_main(["tablet", str(repo)]) == 0
        assert kernel.call_count == 2

    monkeypatch.delenv("TRELLIS_SANDBOX_ROLE", raising=False)
    with patch(
        "trellis.checking.run_kernel_cli",
        side_effect=[node_response, tablet_response],
    ) as kernel:
        assert check_main(["node", "SomeNode", str(repo)]) == 0
        assert check_main(["tablet", str(repo)]) == 0
    assert kernel.call_count == 2
