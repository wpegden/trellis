"""Kernel-backed checker wrapper for trellis.

This module is the active import surface for checker-facing code. It keeps
Python limited to loading JSON, invoking atomic observation helpers, and
forwarding requests to the Rust kernel for every acceptance-relevant decision.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path
from typing import Any, Dict, Mapping, Optional, Sequence

from trellis.atomic_actions.cli import main as atomic_actions_main
from trellis.project_paths import project_checker_dir, project_runtime_src_dir, project_state_dir_for_repo
from trellis.runtime.kernel_cli import KernelCliError, run_kernel_cli
from trellis.runtime_snapshot import materialize_project_runtime


def _load_json_artifact(path: Path) -> tuple[Optional[Any], list[str]]:
    try:
        return json.loads(path.read_text(encoding="utf-8")), []
    except FileNotFoundError:
        return None, [f"{path} not found"]
    except (json.JSONDecodeError, TypeError) as exc:
        return None, [f"{path} is not valid JSON: {exc}"]
    except OSError as exc:
        return None, [f"Could not read {path}: {exc}"]


def _print_json_validation_result(result: Dict[str, Any], *, path: Path) -> int:
    if result.get("ok", False):
        print(f"OK: {path}")
        for warning in result.get("warnings", []):
            print(f"WARNING: {warning}")
        return 0
    for err in result.get("errors", []):
        print(f"FAIL: {err}")
    return 1


def _raw_only_full_check_warning(
    *, path: Path, context_json: Optional[Path], repo: Optional[Path]
) -> str:
    """Warning emitted on a successful `--raw-only` worker-result check.

    `--raw-only` validates only the JSON envelope (the kernel's
    `validate_trellis_worker_result` action — `summary`/`outcome`/
    `comments`/etc.). It does NOT touch any Tablet/*.lean files, so it
    cannot catch structural-invariant violations (declaration-name
    mismatch, extra top-level lean declarations, .tex shape errors)
    that the kernel's commit-side acceptance gate enforces.

    A worker that submits `outcome=valid` based only on a `--raw-only`
    OK has effectively bypassed the structural part of the gate and
    will get reverted by the kernel at commit time.

    Suggest the full-check command (same args minus `--raw-only`) so
    the worker can self-validate before submitting.
    """
    # Match the worker contract's `acceptance_check_command` argument order
    # (`--repo` before `--context-json`) so the rendered string is identical to
    # what `request_contracts.rs:1665-1681` already gives the worker — no
    # reformulation surprises.
    script = sys.argv[0] if sys.argv and sys.argv[0] else "<scripts>/check.py"
    parts = ["python3", script, "trellis-worker-result", str(path)]
    if repo is not None:
        parts.extend(["--repo", str(repo)])
    if context_json is not None:
        parts.extend(["--context-json", str(context_json)])
    full_cmd = " ".join(parts)
    return (
        "--raw-only does not check Tablet/*.lean shape. "
        "Before reporting outcome=valid, also run the contract's "
        "acceptance_check_command:\n"
        f"    {full_cmd}"
    )


def _kernel_response(
    payload: Mapping[str, Any],
    *,
    expected_status: str,
) -> Dict[str, Any]:
    try:
        response = run_kernel_cli(dict(payload))
    except KernelCliError as exc:
        return {"ok": False, "errors": [f"kernel CLI failed: {exc}"], "data": None}
    if response.get("status") != expected_status:
        return {
            "ok": False,
            "errors": [f"unexpected kernel response status: {response.get('status')!r}"],
            "data": None,
        }
    output = response.get("output")
    if not isinstance(output, dict):
        return {"ok": False, "errors": ["kernel response is missing output"], "data": None}
    return {"ok": True, "errors": [], "data": output}


def _unwrap_artifact_validation_result(result: Dict[str, Any]) -> Dict[str, Any]:
    if not result.get("ok", False):
        return result
    outer = result.get("data")
    if not isinstance(outer, Mapping):
        return {"ok": False, "errors": ["artifact validation output is missing data"], "data": None}
    inner_ok = bool(outer.get("ok", False))
    inner_errors = outer.get("errors", [])
    if not inner_ok:
        if isinstance(inner_errors, list):
            return {"ok": False, "errors": [str(err) for err in inner_errors], "data": None}
        return {"ok": False, "errors": ["artifact validation failed"], "data": None}
    return {"ok": True, "errors": [], "data": outer.get("data")}

def build_trellis_worker_acceptance_context(
    repo: Path,
    request: Mapping[str, Any],
    *,
    collect_observations: bool = True,
    paper_source_path: Optional[Path] = None,
) -> Dict[str, Any]:
    payload: Dict[str, Any] = {
        "action": "prepare_worker_gate",
        "repo_path": str(repo),
        "request": dict(request),
        "collect_observations": collect_observations,
    }
    if paper_source_path is not None:
        payload["paper_source_path"] = str(paper_source_path)
    return _kernel_response(
        payload,
        expected_status="prepare_worker_gate_ok",
    )


def normalize_trellis_worker_result_data(
    data: Any,
    *,
    repo: Path,
    acceptance_context: Mapping[str, Any],
) -> Dict[str, Any]:
    if not isinstance(data, dict):
        return {"ok": False, "errors": ["result must be a JSON object"], "data": None}
    result = _kernel_response(
        {
            "action": "check_trellis_worker_result",
            "repo_path": str(repo),
            "acceptance_context": dict(acceptance_context),
            "raw_payload": dict(data),
        },
        expected_status="check_trellis_worker_result_ok",
    )
    if result["ok"] and isinstance(result["data"], dict):
        return dict(result["data"])
    return {"ok": False, "errors": list(result["errors"]), "data": None}


def _worker_request_id(acceptance_context: Mapping[str, Any]) -> int:
    request = acceptance_context.get("request")
    if not isinstance(request, Mapping):
        return 0
    try:
        return int(request.get("id", 0) or 0)
    except Exception:
        return 0


def _worker_checker_trace_path(repo: Path, request_id: int) -> Path:
    return project_checker_dir(project_state_dir_for_repo(repo)) / f"worker_request_{request_id}.json"


def record_worker_checker_trace(
    repo: Path,
    *,
    acceptance_context: Mapping[str, Any],
    result: Mapping[str, Any],
    source: str,
) -> Optional[Path]:
    request_id = _worker_request_id(acceptance_context)
    if request_id <= 0:
        return None
    trace_dir = project_checker_dir(project_state_dir_for_repo(repo))
    trace_dir.mkdir(parents=True, exist_ok=True)
    path = _worker_checker_trace_path(repo, request_id)
    payload = {
        "request_id": request_id,
        "cycle": (
            acceptance_context.get("request", {}).get("cycle", 0)
            if isinstance(acceptance_context.get("request"), Mapping)
            else 0
        ),
        "source": str(source or "").strip(),
        "repo_path": str(repo.resolve()),
        "result": dict(result),
    }
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    return path


def load_worker_checker_trace(repo: Path, *, request_id: int) -> Optional[Dict[str, Any]]:
    if request_id <= 0:
        return None
    path = _worker_checker_trace_path(repo, request_id)
    if not path.is_file():
        return None
    data, errors = _load_json_artifact(path)
    if errors or not isinstance(data, dict):
        return None
    return dict(data)


def normalize_trellis_reviewer_result_data(
    data: Any,
    *,
    review_request: Mapping[str, Any],
) -> Dict[str, Any]:
    if not isinstance(data, dict):
        return {"ok": False, "errors": ["result must be a JSON object"], "data": None}
    result = _kernel_response(
        {
            "action": "check_trellis_reviewer_result",
            "review_request": dict(review_request),
            "raw_payload": dict(data),
        },
        expected_status="check_trellis_reviewer_result_ok",
    )
    if result["ok"] and isinstance(result["data"], dict):
        return dict(result["data"])
    return {"ok": False, "errors": list(result["errors"]), "data": None}


def normalize_trellis_audit_result_data(
    data: Any,
    *,
    audit_request: Mapping[str, Any],
) -> Dict[str, Any]:
    """Cleanup-v2 (audit Finding 1): one-shot validate + normalize for the
    audit-burst artifact. Parallels `normalize_trellis_reviewer_result_data`.
    """
    if not isinstance(data, dict):
        return {"ok": False, "errors": ["result must be a JSON object"], "data": None}
    result = _kernel_response(
        {
            "action": "check_trellis_audit_result",
            "audit_request": dict(audit_request),
            "raw_payload": dict(data),
        },
        expected_status="check_trellis_audit_result_ok",
    )
    if result["ok"] and isinstance(result["data"], dict):
        return dict(result["data"])
    return {"ok": False, "errors": list(result["errors"]), "data": None}


def normalize_trellis_stuck_math_audit_result_data(
    data: Any,
    *,
    audit_request: Mapping[str, Any],
    repo: Optional[Path] = None,
) -> Dict[str, Any]:
    if not isinstance(data, dict):
        return {"ok": False, "errors": ["result must be a JSON object"], "data": None}
    result = _kernel_response(
        {
            "action": "check_trellis_stuck_math_audit_result",
            "audit_request": dict(audit_request),
            "raw_payload": dict(data),
        },
        expected_status="check_trellis_stuck_math_audit_result_ok",
    )
    if result["ok"] and isinstance(result["data"], dict):
        output = dict(result["data"])
        if repo is not None:
            probe_errors = _stuck_math_audit_probe_path_errors(
                output.get("data"),
                audit_request=audit_request,
                repo=repo,
            )
            if probe_errors:
                return {"ok": False, "errors": probe_errors, "data": None}
        return output
    return {"ok": False, "errors": list(result["errors"]), "data": None}


def _stuck_math_audit_probe_path_errors(
    data: Any,
    *,
    audit_request: Mapping[str, Any],
    repo: Path,
) -> list[str]:
    if not isinstance(data, Mapping):
        return ["normalized stuck math audit output is missing data"]
    probe_paths = data.get("probe_paths", [])
    if not isinstance(probe_paths, list):
        return ["probe_paths must be a list"]
    request_id = str(audit_request.get("id", "unknown") or "unknown")
    cycle = str(audit_request.get("cycle", "unknown") or "unknown")
    scratch = (
        repo
        / ".trellis"
        / "stuck-math-audit"
        / f"cycle-{cycle}-request-{request_id}"
    ).resolve()
    errors: list[str] = []
    for idx, raw in enumerate(probe_paths):
        if not isinstance(raw, str):
            errors.append(f"probe_paths[{idx}] must be a string")
            continue
        rel = raw.strip()
        if not rel:
            errors.append(f"probe_paths[{idx}] must be non-empty")
            continue
        if rel.startswith("/") or ".." in Path(rel).parts:
            errors.append(f"probe_paths[{idx}] must be a relative path inside the current scratch directory")
            continue
        path = (repo / rel).resolve()
        try:
            path.relative_to(scratch)
        except ValueError:
            errors.append(
                f"probe_paths[{idx}] must be inside {scratch.relative_to(repo)}"
            )
            continue
        if path.is_symlink():
            errors.append(f"probe_paths[{idx}] must not be a symlink")
            continue
        if not path.is_file():
            errors.append(f"probe_paths[{idx}] must name an existing regular file")
            continue
        try:
            if path.stat().st_nlink > 1:
                errors.append(f"probe_paths[{idx}] must not be a hardlink")
        except OSError as exc:
            errors.append(f"probe_paths[{idx}] could not be stat'ed: {exc}")
    return errors


def _validate_node_result(repo: Path, node_name: str, *, expected_hash: str = "") -> Dict[str, Any]:
    return _kernel_response(
        {
            "action": "check_node",
            "repo_path": str(repo),
            "node_name": node_name,
            "expected_hash": expected_hash,
        },
        expected_status="check_node_ok",
    )


def _validate_tablet_result(repo: Path) -> Dict[str, Any]:
    return _kernel_response(
        {
            "action": "check_tablet",
            "repo_path": str(repo),
        },
        expected_status="check_tablet_ok",
    )


def check_node(repo: Path, node_name: str, *, expected_hash: str = "") -> Dict[str, Any]:
    result = _validate_node_result(repo, node_name, expected_hash=expected_hash)
    if result["ok"] and isinstance(result["data"], dict):
        return dict(result["data"])
    return {"ok": False, "errors": list(result["errors"]), "warnings": []}


def check_tablet(repo: Path) -> Dict[str, Any]:
    result = _validate_tablet_result(repo)
    if result["ok"] and isinstance(result["data"], dict):
        return dict(result["data"])
    return {"ok": False, "errors": list(result["errors"]), "warnings": []}


def check_tablet_scoped(
    repo: Path,
    *,
    baseline_errors: Sequence[str],
    allowed_nodes: Sequence[str],
) -> Dict[str, Any]:
    result = _kernel_response(
        {
            "action": "check_tablet_scoped",
            "repo_path": str(repo),
            "baseline_errors": list(baseline_errors),
            "allowed_nodes": list(allowed_nodes),
        },
        expected_status="check_tablet_scoped_ok",
    )
    if result["ok"] and isinstance(result["data"], dict):
        return dict(result["data"])
    return {"ok": False, "errors": list(result["errors"]), "warnings": []}


def validate_json_artifact(
    kind: str,
    path: Path,
    *,
    phase: Optional[str] = None,
    context_json: Optional[Path] = None,
    node_name: Optional[str] = None,
    repo: Optional[Path] = None,
    raw_only: bool = False,
    invalid_attempt: bool = False,
    allow_targeted_without_next_active: bool = False,
    allowed_decisions: Optional[Sequence[str]] = None,
    allowed_next_modes: Optional[Sequence[str]] = None,
    allowed_resets: Optional[Sequence[str]] = None,
    allowed_difficulty_update_nodes: Optional[Sequence[str]] = None,
) -> Dict[str, Any]:
    data, load_errors = _load_json_artifact(path)
    if load_errors:
        return {"ok": False, "errors": load_errors, "data": None}
    assert data is not None

    if kind == "trellis-worker-result":
        if raw_only:
            if context_json is None:
                return {"ok": False, "errors": ["context_json is required for raw-only worker validation"], "data": None}
            context_data, context_errors = _load_json_artifact(context_json)
            if context_errors:
                return {"ok": False, "errors": context_errors, "data": None}
            if not isinstance(context_data, Mapping):
                return {"ok": False, "errors": ["worker acceptance context JSON must be a JSON object"], "data": None}
            result = _kernel_response(
                {
                    "action": "validate_trellis_worker_result",
                    "raw_payload": data,
                    "acceptance_context": dict(context_data),
                },
                expected_status="validate_trellis_worker_result_ok",
            )
            if result.get("ok"):
                result.setdefault("warnings", []).append(
                    _raw_only_full_check_warning(
                        path=path, context_json=context_json, repo=repo,
                    )
                )
            return result
        if context_json is None:
            return _kernel_response(
                {"action": "validate_trellis_worker_result", "raw_payload": data},
                expected_status="validate_trellis_worker_result_ok",
            )
        context_data, context_errors = _load_json_artifact(context_json)
        if context_errors:
            return {"ok": False, "errors": context_errors, "data": None}
        if not isinstance(context_data, Mapping):
            return {"ok": False, "errors": ["worker acceptance context JSON must be a JSON object"], "data": None}
        result = normalize_trellis_worker_result_data(
            data,
            repo=repo or Path("."),
            acceptance_context=context_data,
        )
        record_worker_checker_trace(
            repo or Path("."),
            acceptance_context=context_data,
            result=result,
            source="script",
        )
        return result

    if kind == "trellis-reviewer-result":
        if context_json is None:
            return _kernel_response(
                {"action": "validate_trellis_reviewer_result", "raw_payload": data},
                expected_status="validate_trellis_reviewer_result_ok",
            )
        context_data, context_errors = _load_json_artifact(context_json)
        if context_errors:
            return {"ok": False, "errors": context_errors, "data": None}
        if not isinstance(context_data, Mapping):
            return {"ok": False, "errors": ["review context JSON must be a JSON object"], "data": None}
        return normalize_trellis_reviewer_result_data(data, review_request=context_data)

    if kind == "trellis-audit-result":
        # Cleanup-v2 (audit Finding 1): validate + normalize an audit-burst
        # artifact. Raw-only path returns shape validation only; full path
        # also normalizes against the originating Audit request context.
        if context_json is None:
            return _kernel_response(
                {"action": "validate_trellis_audit_result", "raw_payload": data},
                expected_status="validate_trellis_audit_result_ok",
            )
        context_data, context_errors = _load_json_artifact(context_json)
        if context_errors:
            return {"ok": False, "errors": context_errors, "data": None}
        if not isinstance(context_data, Mapping):
            return {"ok": False, "errors": ["audit context JSON must be a JSON object"], "data": None}
        return normalize_trellis_audit_result_data(data, audit_request=context_data)

    if kind == "trellis-stuck-math-audit-result":
        if context_json is None:
            return _kernel_response(
                {"action": "validate_trellis_stuck_math_audit_result", "raw_payload": data},
                expected_status="validate_trellis_stuck_math_audit_result_ok",
            )
        context_data, context_errors = _load_json_artifact(context_json)
        if context_errors:
            return {"ok": False, "errors": context_errors, "data": None}
        if not isinstance(context_data, Mapping):
            return {"ok": False, "errors": ["stuck math audit context JSON must be a JSON object"], "data": None}
        return normalize_trellis_stuck_math_audit_result_data(
            data,
            audit_request=context_data,
            repo=repo,
        )

    if kind == "correspondence-result":
        return _unwrap_artifact_validation_result(
            _kernel_response(
                {"action": "validate_correspondence_result", "raw_payload": data},
                expected_status="validate_correspondence_result_ok",
            )
        )

    if kind == "paper-faithfulness-result":
        return _unwrap_artifact_validation_result(
            _kernel_response(
                {"action": "validate_paper_faithfulness_result", "raw_payload": data},
                expected_status="validate_paper_faithfulness_result_ok",
            )
        )

    if kind == "deviation-authorization-result":
        return _unwrap_artifact_validation_result(
            _kernel_response(
                {"action": "validate_deviation_authorization_result", "raw_payload": data},
                expected_status="validate_deviation_authorization_result_ok",
            )
        )

    if kind == "substantiveness-result":
        return _unwrap_artifact_validation_result(
            _kernel_response(
                {"action": "validate_substantiveness_result", "raw_payload": data},
                expected_status="validate_substantiveness_result_ok",
            )
        )

    if kind == "soundness-result":
        if node_name is None:
            return {"ok": False, "errors": ["node_name is required for soundness-result"], "data": None}
        return _unwrap_artifact_validation_result(
            _kernel_response(
                {
                    "action": "validate_soundness_result",
                    "raw_payload": data,
                    "node_name": node_name,
                },
                expected_status="validate_soundness_result_ok",
            )
        )

    return {"ok": False, "errors": [f"unsupported artifact kind: {kind}"], "data": None}


def _node_main(argv: Sequence[str]) -> int:
    parser = argparse.ArgumentParser(prog="node")
    parser.add_argument("node_name")
    parser.add_argument("repo_path", nargs="?", default=".")
    args = parser.parse_args(list(argv))
    repo = Path(args.repo_path).resolve()
    result = _validate_node_result(repo, args.node_name)
    if result.get("ok", False):
        print(f"OK: {repo / 'Tablet' / f'{args.node_name}.lean'}")
        return 0
    for err in result.get("errors", []):
        print(f"FAIL: {err}")
    return 1


def _tablet_main(argv: Sequence[str]) -> int:
    parser = argparse.ArgumentParser(prog="tablet")
    parser.add_argument("repo_path", nargs="?", default=".")
    args = parser.parse_args(list(argv))
    result = _validate_tablet_result(Path(args.repo_path).resolve())
    if result.get("ok", False):
        print(f"OK: {Path(args.repo_path).resolve() / 'Tablet'}")
        return 0
    for err in result.get("errors", []):
        print(f"FAIL: {err}")
    return 1


def _artifact_main(kind: str, argv: Sequence[str]) -> int:
    parser = argparse.ArgumentParser(prog=kind)
    parser.add_argument("path")
    parser.add_argument("--phase")
    parser.add_argument("--repo", default=".")
    parser.add_argument("--node")
    parser.add_argument("--context-json")
    parser.add_argument("--raw-only", action="store_true")
    if kind == "trellis-reviewer-result":
        parser.add_argument("--allow-targeted-without-next-active", action="store_true")
        parser.add_argument("--allowed-decision", action="append", default=[])
        parser.add_argument("--allowed-next-mode", action="append", default=[])
        parser.add_argument("--allowed-reset", action="append", default=[])
        parser.add_argument("--allowed-difficulty-update-node", action="append", default=[])
    args = parser.parse_args(list(argv))
    result = validate_json_artifact(
        kind,
        Path(args.path),
        phase=args.phase,
        context_json=Path(args.context_json).resolve() if args.context_json else None,
        node_name=args.node,
        repo=Path(args.repo).resolve(),
        raw_only=args.raw_only,
        allow_targeted_without_next_active=getattr(args, "allow_targeted_without_next_active", False),
        allowed_decisions=getattr(args, "allowed_decision", None),
        allowed_next_modes=getattr(args, "allowed_next_mode", None),
        allowed_resets=getattr(args, "allowed_reset", None),
        allowed_difficulty_update_nodes=getattr(args, "allowed_difficulty_update_node", None),
    )
    return _print_json_validation_result(result, path=Path(args.path))


def _sync_supervisor_workspace_main(argv: Sequence[str]) -> int:
    parser = argparse.ArgumentParser(prog="sync-supervisor-workspace")
    parser.add_argument("repo_path", nargs="?", default=".")
    args = parser.parse_args(list(argv))
    from trellis.supervisor_workspace import sync_supervisor_workspace

    payload = sync_supervisor_workspace(Path(args.repo_path).resolve())
    print(json.dumps(payload))
    return 0


def generate_check_node_sh(
    repo_path: Path,
    state_dir: Path,
) -> str:
    return """#!/bin/bash
# Wrapper for the shared deterministic checker.
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
state_dir="$(cd "$script_dir/.." && pwd)"
repo_dir="$(cd "$state_dir/.." && pwd)"
exec python3 "$script_dir/check.py" node "$@" "$repo_dir"
"""


def generate_check_tablet_sh(
    repo_path: Path,
    state_dir: Path,
) -> str:
    return """#!/bin/bash
# Wrapper for the shared deterministic checker.
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
state_dir="$(cd "$script_dir/.." && pwd)"
repo_dir="$(cd "$state_dir/.." && pwd)"
exec python3 "$script_dir/check.py" tablet "$repo_dir"
"""


def write_scripts(
    repo_path: Path,
    state_dir: Path,
) -> None:
    materialize_project_runtime(repo_path, state_dir)
    scripts_dir = state_dir / "scripts"
    scripts_dir.mkdir(parents=True, exist_ok=True)
    gid: Optional[int] = None
    try:
        # Use the operator's own primary group so a group-member burst (if a
        # second uid is reintroduced) can read/exec these scripts. Under the
        # current single-uid sandbox this is a self-chown no-op.
        gid = os.getgid()
        os.chown(str(scripts_dir), -1, gid)
        os.chmod(str(scripts_dir), 0o2755)
    except (OSError, PermissionError):
        gid = None

    check_dst = scripts_dir / "check.py"
    check_dst.write_text(
        "#!/usr/bin/env python3\n"
        "from __future__ import annotations\n\n"
        "import os\n"
        "import sys\n\n"
        "from pathlib import Path\n\n"
        "_state_root = Path(__file__).resolve().parent.parent\n"
        "_src_root = str((_state_root / 'runtime' / 'src').resolve())\n"
        "if _src_root not in sys.path:\n"
        "    sys.path.insert(0, _src_root)\n\n"
        "# Pin the kernel binary to THIS repo's materialized runtime bin, anchored\n"
        "# to check.py's own location. Inside the worker sandbox the repo-local bin\n"
        "# is the ONLY valid kernel: an inherited TRELLIS_TRELLIS_KERNEL_CMD comes\n"
        "# from the supervisor's host view (e.g. a `kernel/target/debug` path under\n"
        "# the checkout the supervisor was launched from) which is NOT mounted here,\n"
        "# so trusting it yields FileNotFoundError. Override it unconditionally when\n"
        "# the materialized bin is present; this also covers the case where\n"
        "# `from trellis` resolves source_root to an escaped checkout.\n"
        "_kernel_bin = _state_root / 'runtime' / 'bin' / 'trellis_runtime_cli'\n"
        "if _kernel_bin.is_file():\n"
        "    os.environ['TRELLIS_TRELLIS_KERNEL_CMD'] = str(_kernel_bin)\n\n"
        "from trellis.agent_check import main\n\n"
        "if __name__ == '__main__':\n"
        "    raise SystemExit(main())\n",
        encoding="utf-8",
    )
    check_dst.chmod(0o755)
    if gid is not None:
        try:
            os.chown(str(check_dst), -1, gid)
        except PermissionError:
            pass

    check_node_path = scripts_dir / "check_node.sh"
    check_node_path.write_text(
        generate_check_node_sh(
            repo_path,
            state_dir,
        ),
        encoding="utf-8",
    )
    check_node_path.chmod(0o755)
    if gid is not None:
        try:
            os.chown(str(check_node_path), -1, gid)
        except PermissionError:
            pass

    check_tablet_path = scripts_dir / "check_tablet.sh"
    check_tablet_path.write_text(
        generate_check_tablet_sh(
            repo_path,
            state_dir,
        ),
        encoding="utf-8",
    )
    check_tablet_path.chmod(0o755)
    if gid is not None:
        try:
            os.chown(str(check_tablet_path), -1, gid)
        except PermissionError:
            pass


def main(argv: Optional[Sequence[str]] = None) -> int:
    raw_args = list(argv if argv is not None else sys.argv[1:])
    if not raw_args:
        print("FAIL: command is required")
        return 2

    command = raw_args[0]
    rest = raw_args[1:]
    if command in {
        "lean-compile-node",
        "lean-build-tablet",
        "prepare-compiled-support",
        "materialize-tablet-oleans",
        "print-axioms",
        "local-closure-axioms",
        "lean-semantic-payloads",
        "sync-tablet-support",
    }:
        return atomic_actions_main(raw_args)
    if command == "sync-supervisor-workspace":
        return _sync_supervisor_workspace_main(rest)
    if command == "node":
        return _node_main(rest)
    if command == "tablet":
        return _tablet_main(rest)
    if command in {
        "trellis-worker-result",
        "trellis-reviewer-result",
        "trellis-audit-result",
        "trellis-stuck-math-audit-result",
        "paper-faithfulness-result",
        "deviation-authorization-result",
        "substantiveness-result",
        "correspondence-result",
        "soundness-result",
    }:
        return _artifact_main(command, rest)

    print(f"FAIL: unknown command: {command}")
    return 2


__all__ = [
    "build_trellis_worker_acceptance_context",
    "check_node",
    "check_tablet",
    "check_tablet_scoped",
    "generate_check_node_sh",
    "generate_check_tablet_sh",
    "main",
    "normalize_trellis_audit_result_data",
    "normalize_trellis_stuck_math_audit_result_data",
    "normalize_trellis_reviewer_result_data",
    "normalize_trellis_worker_result_data",
    "load_worker_checker_trace",
    "record_worker_checker_trace",
    "validate_json_artifact",
    "write_scripts",
]
