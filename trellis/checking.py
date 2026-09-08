"""Kernel-backed checker wrapper for trellis.

This module is the active import surface for checker-facing code. It keeps
Python limited to loading JSON, invoking atomic observation helpers, and
forwarding requests to the Rust kernel for every acceptance-relevant decision.
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import sys
from pathlib import Path
from typing import Any, Dict, Mapping, Optional, Sequence

from trellis.atomic_actions.cli import main as atomic_actions_main
from trellis.project_paths import project_checker_dir, project_runtime_src_dir, project_state_dir_for_repo
from trellis.runtime.kernel_cli import KernelCliError, run_kernel_cli
from trellis.runtime_snapshot import materialize_project_runtime


def _artifact_not_found_hint(path: Path, repo: Optional[Path]) -> Optional[str]:
    """The two-directories-share-a-basename collision: the runtime state dir
    ``<runtime-root>/<name>/staging/…`` is a natural mistyping of the bridge
    dir ``<repo>/.trellis/runtime/<name>/staging/…`` where the artifact-check
    files actually live. When the missing path has exactly ONE same-basename
    twin under the repo's bridge staging tree, state the correct path."""
    roots: list[Path] = []
    if repo is not None:
        roots.append(repo)
    script = Path(sys.argv[0]).resolve() if sys.argv and sys.argv[0] else None
    if (
        script is not None
        and script.parent.name == "scripts"
        and script.parent.parent.name == ".trellis"
    ):
        roots.append(script.parent.parent.parent)
    candidates: list[Path] = []
    for root in roots:
        runtime_root = root / ".trellis" / "runtime"
        if runtime_root.is_dir():
            candidates.extend(sorted(runtime_root.glob(f"*/staging/{path.name}")))
    unique = [
        candidate
        for candidate in dict.fromkeys(candidates)
        if candidate.is_file() and candidate != path
    ]
    if len(unique) != 1:
        return None
    return (
        f"{path} not found. The artifact-check files live under "
        f"<repo>/.trellis/runtime/<run>/staging/ (not under the runtime state "
        f"directory of the same basename); did you mean {unique[0]}?"
    )


def _load_json_artifact(
    path: Path, *, repo: Optional[Path] = None
) -> tuple[Optional[Any], list[str]]:
    try:
        return json.loads(path.read_text(encoding="utf-8")), []
    except FileNotFoundError:
        hint = _artifact_not_found_hint(path, repo)
        return None, [hint if hint else f"{path} not found"]
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
    goal_prose_path: Optional[Path] = None,
) -> Dict[str, Any]:
    payload: Dict[str, Any] = {
        "action": "prepare_worker_gate",
        "repo_path": str(repo),
        "request": dict(request),
        "collect_observations": collect_observations,
    }
    if paper_source_path is not None:
        payload["paper_source_path"] = str(paper_source_path)
    # GAP B (W10): the prose GOAL referent. Passed unconditionally when the
    # config resolves one; the kernel uses it only for prose-mode requests
    # (`request.is_pv`), so math fingerprints are untouched.
    if goal_prose_path is not None:
        payload["goal_prose_path"] = str(goal_prose_path)
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
    kernel_request: Dict[str, Any] = {
        "action": "check_trellis_stuck_math_audit_result",
        "audit_request": dict(audit_request),
        "raw_payload": dict(data),
    }
    # Process memory: the kernel checker validates `memory_operations`
    # entry existence/status against the repo worktree (payloads carrying
    # operations without a repo path are rejected — fail closed).
    if repo is not None:
        kernel_request["repo_path"] = str(repo)
    result = _kernel_response(
        kernel_request,
        expected_status="check_trellis_stuck_math_audit_result_ok",
    )
    if result["ok"] and isinstance(result["data"], dict):
        output = dict(result["data"])
        if not output.get("ok", False):
            # The kernel checker rejected the artifact (inner verdict
            # `ok: false` under the `_ok` CLI status — the normal encoding
            # for validation failures). Surface its errors verbatim.
            # Regression guard (stuck-math-audit 2998, cycle 677): falling
            # through to the probe-path check here masked actionable
            # `memory_operations` entry-id errors behind the opaque
            # "normalized stuck math audit output is missing data".
            errors = [str(err) for err in output.get("errors") or []]
            return {
                "ok": False,
                "errors": errors
                or ["stuck math audit checker rejected the artifact without detail"],
                "data": None,
            }
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
    # Citable region: any cycle's audit scratch dir. The current burst
    # writes its own `cycle-<cycle>-request-<id>` dir; prior cycles'
    # artifacts are read-only citable evidence (a witness recorded three
    # cycles ago is still the witness — forcing it into report prose can
    # make a false-target finding decay). The integrity checks
    # below (existing regular file, no symlink, no hardlink, inside the
    # audit scratch tree) apply to every cited path regardless of cycle.
    scratch_root = (repo / ".trellis" / "stuck-math-audit").resolve()
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
            errors.append(f"probe_paths[{idx}] must be a relative path inside an audit scratch directory")
            continue
        path = (repo / rel).resolve()
        try:
            inside = path.relative_to(scratch_root)
        except ValueError:
            errors.append(
                f"probe_paths[{idx}] must be inside {scratch_root.relative_to(repo)}"
            )
            continue
        if len(inside.parts) < 2 or not inside.parts[0].startswith("cycle-"):
            errors.append(
                f"probe_paths[{idx}] must be inside a cycle-* audit scratch directory "
                f"(the current burst's, or a prior cycle's as a read-only citation)"
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
    data, load_errors = _load_json_artifact(path, repo=repo)
    if load_errors:
        return {"ok": False, "errors": load_errors, "data": None}
    assert data is not None

    if kind == "trellis-worker-result":
        if raw_only:
            if context_json is None:
                return {"ok": False, "errors": ["context_json is required for raw-only worker validation"], "data": None}
            context_data, context_errors = _load_json_artifact(context_json, repo=repo)
            if context_errors:
                return {"ok": False, "errors": context_errors, "data": None}
            if not isinstance(context_data, Mapping):
                return {"ok": False, "errors": ["worker acceptance context JSON must be a JSON object"], "data": None}
            # Unwrap the kernel's inner {ok, errors, data} verdict; the raw
            # `_kernel_response` envelope reports ok for every JSON object,
            # hiding shape-validation failures from the raw-only self-check.
            result = _unwrap_artifact_validation_result(
                _kernel_response(
                    {
                        "action": "validate_trellis_worker_result",
                        "raw_payload": data,
                        "acceptance_context": dict(context_data),
                    },
                    expected_status="validate_trellis_worker_result_ok",
                )
            )
            if result.get("ok"):
                result.setdefault("warnings", []).append(
                    _raw_only_full_check_warning(
                        path=path, context_json=context_json, repo=repo,
                    )
                )
            return result
        if context_json is None:
            return _unwrap_artifact_validation_result(
                _kernel_response(
                    {"action": "validate_trellis_worker_result", "raw_payload": data},
                    expected_status="validate_trellis_worker_result_ok",
                )
            )
        context_data, context_errors = _load_json_artifact(context_json, repo=repo)
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
            return _unwrap_artifact_validation_result(
                _kernel_response(
                    {"action": "validate_trellis_reviewer_result", "raw_payload": data},
                    expected_status="validate_trellis_reviewer_result_ok",
                )
            )
        context_data, context_errors = _load_json_artifact(context_json, repo=repo)
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
            return _unwrap_artifact_validation_result(
                _kernel_response(
                    {"action": "validate_trellis_audit_result", "raw_payload": data},
                    expected_status="validate_trellis_audit_result_ok",
                )
            )
        context_data, context_errors = _load_json_artifact(context_json, repo=repo)
        if context_errors:
            return {"ok": False, "errors": context_errors, "data": None}
        if not isinstance(context_data, Mapping):
            return {"ok": False, "errors": ["audit context JSON must be a JSON object"], "data": None}
        return normalize_trellis_audit_result_data(data, audit_request=context_data)

    if kind == "trellis-stuck-math-audit-result":
        if context_json is None:
            # Unwrap the kernel's inner {ok, errors, data} verdict (same as
            # correspondence/paper-faithfulness below). Returning the raw
            # `_kernel_response` envelope reported OK for every JSON object,
            # hiding shape-validation failures from the raw-only self-check.
            return _unwrap_artifact_validation_result(
                _kernel_response(
                    {"action": "validate_trellis_stuck_math_audit_result", "raw_payload": data},
                    expected_status="validate_trellis_stuck_math_audit_result_ok",
                )
            )
        context_data, context_errors = _load_json_artifact(context_json, repo=repo)
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


# Burst roles whose sandbox withholds the writable `Tablet/` tree and the
# checker RPC socket that `check_node` / `check_tablet` need. The kernel runs
# the deterministic check for these roles; the entry point says so and stops.
#
# This is every `burst_role` the kernel emits on a `stuck_math_audit_contract`
# (`kernel/src/request_contracts.rs`): all seven route through
# `_handle_stuck_math_audit` in `trellis/runtime/bridge.py`, and
# `_repo_writable_paths` in `trellis/sandbox.py` grants a writable `Tablet/` to
# `worker` / `grunt` / `lake_compiler` alone.
_DETERMINISTIC_CHECK_DENIED_ROLES = frozenset(
    {
        "stuck_math_audit",
        "need_input_auditor",
        "revision_planner",
        "initial_planner",
        "gap_research",
        "gap_plan_critic",
        "assumptions_lane",
    }
)


def _deterministic_check_refusal(entry_point: str) -> Optional[str]:
    role = os.environ.get("TRELLIS_SANDBOX_ROLE", "").strip()
    if role not in _DETERMINISTIC_CHECK_DENIED_ROLES:
        return None
    return (
        f"`check.py {entry_point}` is the supervisor-owned deterministic acceptance check. "
        "The per-node Lean-closure, soundness, correspondence and substantiveness verdicts "
        "are recorded under `node_lane_verdicts` in this burst's request context JSON."
    )


def _node_main(argv: Sequence[str]) -> int:
    refusal = _deterministic_check_refusal("node")
    if refusal is not None:
        print(refusal)
        return 1
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
    refusal = _deterministic_check_refusal("tablet")
    if refusal is not None:
        print(refusal)
        return 1
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


def _repo_targets_isabelle(repo_path: Path) -> bool:
    """True iff this repo is an Isabelle-backend run (warm flag irrelevant).

    Gates the `isa-query` discovery-helper emitter. Unlike
    :func:`_repo_targets_isabelle_warm`, this does NOT consult the warm-session
    flag: `isa-query` drives its own throwaway `isabelle build`, so it is useful
    whether or not the warm checker session is up. A Lean run yields False, so
    the Lean `.trellis/scripts/` contents stay byte-identical.

    Any read failure is treated as "not isabelle" so the Lean path is the safe
    default (mirrors the warm variant).
    """
    try:
        from trellis.sandbox import repo_targets_isabelle

        return bool(repo_targets_isabelle(repo_path))
    except Exception:
        return False


def _repo_targets_isabelle_warm(repo_path: Path) -> bool:
    """True iff this repo is an Isabelle-backend run AND the warm flag is ON.

    Gates the Phase-3 backend swap of the worker ``incremental-check`` emitter:
    only an ``isabelle_hol`` run with the warm checker session enabled gets the
    Isabelle warm-advisory forwarder; a Lean run, OR an Isabelle run with the
    warm flag OFF, keeps the existing Lean ``incremental-check`` body
    byte-for-byte (the Lean advisory is inert on a ``.thy`` tablet, but an
    Isabelle run with no warm session has no warm prefix to advise against, so it
    leaves the Lean wrapper unchanged and the worker uses ``isabelle build``).

    Resolution mirrors the rest of the codebase: ``workflow.default_target ==
    "isabelle_hol"`` (via :func:`trellis.sandbox.repo_targets_isabelle`) +
    ``isabelle_warm_session.enabled`` in the repo's ``trellis.config.json`` (via
    :class:`trellis.checker.isabelle_warm_config.IsabelleWarmSessionConfig`).
    Any read failure is treated as "not isabelle/not warm" so the Lean path is
    the safe default.
    """
    try:
        from trellis.sandbox import repo_targets_isabelle
    except Exception:
        return False
    try:
        if not repo_targets_isabelle(repo_path):
            return False
    except Exception:
        return False
    try:
        from trellis.checker.isabelle_warm_config import IsabelleWarmSessionConfig

        cfg = IsabelleWarmSessionConfig.load(repo_path.resolve() / "trellis.config.json")
        return bool(cfg.enabled)
    except Exception:
        return False


def generate_incremental_check_sh(
    repo_path: Path,
    state_dir: Path,
) -> str:
    """Wrapper exposing the advisory warm-server pre-check as
    ``incremental-check Tablet.NodeName``. Parallels ``check_node.sh`` — it
    resolves the repo root from its own location and forwards the target.

    ``incremental-check`` is a fast advisory pre-check: green is necessary but
    not sufficient. The deterministic worker check remains the only sign-off.

    Backend-aware (Phase 3): on an Isabelle-backend run with the warm checker
    session ON, the wrapper drives the WARM ISABELLE advisory
    (``isabelle_incremental_check.py`` → the ``isabelle-warm-advisory`` socket
    op) instead of the Lean ``lean --server`` body. On a Lean run, OR an
    Isabelle run with the warm flag OFF, the Lean body is byte-for-byte
    unchanged.
    """
    if _repo_targets_isabelle_warm(repo_path):
        return generate_isabelle_incremental_check_sh(repo_path, state_dir)
    return """#!/bin/bash
# Advisory warm-server Lean pre-check (NOT a sign-off gate).
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
state_dir="$(cd "$script_dir/.." && pwd)"
repo_dir="$(cd "$state_dir/.." && pwd)"
exec python3 "$script_dir/incremental_check.py" "$@" --repo "$repo_dir"
"""


def generate_isabelle_incremental_check_sh(
    repo_path: Path,
    state_dir: Path,
) -> str:
    """Isabelle ``incremental-check`` wrapper — drives the WARM advisory op.

    Forwards ``Tablet.NodeName`` to the standalone
    ``isabelle_incremental_check.py``, which sends the ``isabelle-warm-advisory``
    op over ``TRELLIS_CHECKER_SOCKET`` to the supervisor's warm Isabelle session.
    The warm session re-elaborates ONLY the in-flight theory against the warm
    accepted prefix and returns pass/fail+errors WITHOUT the cert / cold
    cross-check (that is the deterministic node-check gate's job). Advisory only:
    green is necessary but not sufficient; failure-open to ``isabelle build``.
    """
    return """#!/bin/bash
# Advisory warm Isabelle pre-check (NOT a sign-off gate).
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
state_dir="$(cd "$script_dir/.." && pwd)"
repo_dir="$(cd "$state_dir/.." && pwd)"
exec python3 "$script_dir/isabelle_incremental_check.py" "$@" --repo "$repo_dir"
"""


def generate_isa_query_sh(
    repo_path: Path,
    state_dir: Path,
) -> str:
    """Worker-facing Isabelle library-DISCOVERY helper (``isa-query``).

    Runs one Isar *diagnostic* command (``find_theorems`` / ``find_consts`` /
    ``solve_direct`` / ``sledgehammer`` / ``thm_oracles``) in the tablet's warm
    base context and prints `.thy`-ready output. This is the discovery analogue
    of ``incremental-check``: that one answers "does my theory compile", this one
    answers "what fact should I use". Both are advisory; neither is a sign-off.

    Paths resolve from ``BASH_SOURCE`` (like ``incremental-check``), so nothing
    host-specific is baked in and the script works in-burst and on the host.

    HEAP POLICY (load-bearing, verified empirically): the query session is NEVER
    built with ``-o system_heaps=true``. The prewarmed ``Tablet_Base`` /
    ``HOL-Probability`` images live in the SYSTEM heaps
    (``$ISABELLE_HEAPS_SYSTEM`` = ``<ISABELLE_HOME>/heaps``), which the worker
    sandbox binds READ-ONLY. Isabelle's default lookup already searches the
    system heaps for session *input* while writing session *output* under
    ``$ISABELLE_HOME_USER`` — which ``etc/settings`` derives unconditionally from
    ``$HOME`` (the per-burst home, writable). Passing ``system_heaps=true`` would
    redirect output into the read-only dir and fail with EROFS.
    """
    return r"""#!/bin/bash
# Isabelle library-DISCOVERY helper (advisory; NOT a sign-off gate).
#
# Usage:
#   isa-query find_theorems '<query>'   e.g. 'name: card_lists_length_eq'
#                                       or  '"card (set _)"'
#   isa-query find_consts   '<query>'   e.g. '"_ list => nat"'
#   isa-query solve_direct  '<goal>'    e.g. '(n::nat) + 0 = n'
#   isa-query sledgehammer  '<goal>' ['<override>']
#                                       override default: provers = vampire z3, timeout = 30
#   isa-query thm_oracles   '<thm-name>'
#   isa-query raw_ml        '<ml-file>' (must File.append to $ISA_OUT)
#
# Options (env):
#   ISA_IMPORTS   imports for the scratch theory (default "HOL-Probability.Probability")
#   ISA_TIMEOUT   hard wall-clock timeout in seconds (default 180)
#   ISA_LIMIT     max results for find_theorems (default 20; find_consts is unlimited)
#   ISA_THREADS   Isabelle worker threads (default 2 — courteous to a co-resident run)
#
# WHY a throwaway `isabelle build` and not `isabelle console`: console is a raw
# Poly/ML REPL (no Isar loop) AND has no bash_process server, so `sledgehammer`
# fails with "Bad bash_process server address". A build session runs every
# command in a real theory context (tool ML visible + ATP server up) and we
# capture output via File.write.
#
# HEAP POLICY: never `-o system_heaps=true`. The prewarmed Tablet_Base /
# HOL-Probability images live in the SYSTEM heaps, which are bound READ-ONLY in
# the worker sandbox. Default lookup reads them for input while writing output
# under $ISABELLE_HOME_USER (derived from $HOME — writable per burst). Forcing
# system heaps would point OUTPUT at the read-only dir and fail.
#
# Queries accept either Isabelle symbol notation (\<Rightarrow>) or the Unicode
# character it renders as; both are normalized to symbol notation before parsing.
# Results print in symbol notation, i.e. copy/paste-ready for a .thy.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
state_dir="$(cd "$script_dir/.." && pwd)"
repo_dir="$(cd "$state_dir/.." && pwd)"

WORK="$state_dir/scratch/isa-query"
SESS_DIR="$WORK/session"
OUT="$SESS_DIR/result.txt"
IMPORTS="${ISA_IMPORTS:-HOL-Probability.Probability}"
TIMEOUT="${ISA_TIMEOUT:-180}"
LIMIT="${ISA_LIMIT:-20}"
THREADS="${ISA_THREADS:-2}"
LOCK_WAIT="${ISA_LOCK_WAIT:-$TIMEOUT}"

usage() {
  sed -n '3,18p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

cmd="${1:-}"
arg="${2:-}"
arg3="${3:-}"

if [[ -z "$cmd" || -z "$arg" ]]; then
  usage; exit 2
fi

# Resolved AFTER the usage check so `isa-query` with no args still explains
# itself on a host that has no isabelle on PATH.
ISA="${TRELLIS_ISABELLE_BIN:-isabelle}"
if ! command -v "$ISA" >/dev/null 2>&1; then
  echo "isa-query: no isabelle binary on PATH and TRELLIS_ISABELLE_BIN unset" >&2
  exit 2
fi

mkdir -p "$SESS_DIR"

# ML escaper for a string embedded inside an SML "..." literal. Backslashes and
# quotes first, then embedded newlines -> `\n`: SML string literals accept
# printable chars or escape gaps, NOT a bare newline, so a multi-line goal
# (`solve_direct $'x = x\nand y = y'`) would otherwise emit an unclosed literal
# and fail the build with a parse error that points nowhere near the cause.
# Isabelle decodes `\<name>` SYMBOLS anywhere in the theory source, INCLUDING
# inside SML string literals. So escaping that backslash to `\\` does not yield
# a literal backslash: the decoder turns `\\<Rightarrow>` into `\` + the ⇒
# character, and SML then rejects `\⇒` with "bad escape character in string".
# Symbol notation is exactly what the skill tells the worker to use (results
# print in it, copy/paste-ready), so it must pass through UNescaped. Protect
# `\<` first, escape every OTHER backslash for SML, then restore.
esc_ml() {
  printf '%s' "$1" \
    | sed -e 's/\\</\x01/g' \
    | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' \
    | sed -e 's/\x01/\\</g' \
    | sed -e ':a' -e 'N' -e '$!ba' -e 's/\n/\\n/g'
}

# Isabelle's inner lexer reads `\<and>`, not the raw UTF-8 `\<and>` character an
# agent naturally types. A .thy FILE accepts either (the loader decodes UTF-8
# before ML sees it), but this helper embeds the query in an SML string literal
# evaluated at run time, past that decode — so an unnormalized `x \<and> y`
# reached the parser as bytes it cannot lex and died with "Inner lexical error".
# Normalize through Isabelle's OWN `etc/symbols` table, so every symbol the
# helper renders in its results is one it can read back. Missing table or no
# python3 leaves the query untouched: symbol notation already works.
ISABELLE_HOME_DIR="$("$ISA" getenv -b ISABELLE_HOME 2>/dev/null || true)"
to_symbols() {
  local table="$ISABELLE_HOME_DIR/etc/symbols"
  if [[ -z "$ISABELLE_HOME_DIR" || ! -f "$table" ]] || ! command -v python3 >/dev/null 2>&1; then
    printf '%s' "$1"
    return 0
  fi
  ISA_SYMBOL_TABLE="$table" python3 -c '
import os, re, sys
table = {}
with open(os.environ["ISA_SYMBOL_TABLE"], encoding="utf-8") as handle:
    for line in handle:
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        code = re.search(r"code:\s*(0x[0-9a-fA-F]+)", line)
        if code:
            table[chr(int(code.group(1), 16))] = line.split()[0]
sys.stdout.write("".join(table.get(ch, ch) for ch in sys.argv[1]))
' "$1"
}

E_ARG="$(esc_ml "$(to_symbols "$arg")")"

case "$cmd" in
  find_theorems)
    ML_BODY="
  val st = Proof.init ctxt;
  val _ = w (Pretty.string_of (Find_Theorems.pretty_theorems st (SOME $LIMIT) true
              (Find_Theorems.read_query Position.none \"$E_ARG\")));"
    ;;
  find_consts)
    ML_BODY="
  val _ = w (Pretty.string_of (Find_Consts.pretty_consts ctxt
              (Find_Consts.read_query Position.none \"$E_ARG\")));"
    ;;
  solve_direct)
    ML_BODY="
  val st = Proof.theorem_cmd NONE (K I) [[(\"$E_ARG\", [])]] ctxt;
  val (ok, (nm, msgs)) = Solve_Direct.solve_direct st;
  val _ = w (\"solve_direct outcome=\" ^ nm ^ \" (solved=\" ^ Bool.toString ok ^ \")\");
  val _ = List.app w msgs;"
    ;;
  sledgehammer)
    OVR="${arg3:-provers = vampire z3, timeout = 30}"
    PAIRS="$(printf '%s' "$OVR" | awk -F, '{
        out="";
        for(i=1;i<=NF;i++){
          n=$i; sub(/=.*/,"",n); gsub(/^[ \t]+|[ \t]+$/,"",n);
          v=$i; sub(/^[^=]*=/,"",v); gsub(/^[ \t]+|[ \t]+$/,"",v);
          if(n!=""){ if(out!="") out=out","; out=out "(\"" n "\",\"" v "\")"; }
        }
        printf "[%s]", out;
      }')"
    ML_BODY="
  val st = Proof.theorem_cmd NONE (K I) [[(\"$E_ARG\", [])]] ctxt;
  val params = Sledgehammer_Commands.default_params thy $PAIRS;
  val (found, (_, msg)) =
    Sledgehammer.run_sledgehammer params Sledgehammer_Prover.Normal NONE 1
      Sledgehammer_Fact.no_fact_override st;
  val _ = w (\"sledge_found=\" ^ Bool.toString found);
  val _ = w msg;"
    ;;
  thm_oracles)
    ML_BODY="
  val th = Global_Theory.get_thm thy \"$E_ARG\";
  val _ = w (Pretty.string_of (Thm_Deps.pretty_thm_oracles ctxt [th]));"
    ;;
  raw_ml)
    if [[ ! -f "$arg" ]]; then echo "raw_ml: file not found: $arg" >&2; exit 2; fi
    ML_BODY="
  val _ = ML_Context.eval ML_Compiler.flags Position.none
            (ML_Lex.read_text (File.read (Path.explode \"$E_ARG\"), Position.none));"
    ;;
  *)
    echo "unknown command: $cmd" >&2; usage; exit 2;;
esac

# Parent the query session on the tablet's prewarmed warm base when this repo
# has one, so discovery runs in the same context the node theories see. Falls
# back to the bare distribution session otherwise.
TABLET_SESSION_DIR="$repo_dir/isabelle"
EXTRA_D=()
if [[ -f "$TABLET_SESSION_DIR/ROOT" ]] && grep -q 'session[[:space:]]\{1,\}"\{0,1\}Tablet_Base' "$TABLET_SESSION_DIR/ROOT"; then
  PARENT="Tablet_Base"
  EXTRA_D=(-d "$TABLET_SESSION_DIR")
else
  PARENT="HOL-Probability"
fi

E_OUT="$(esc_ml "$OUT")"

# Take the lock BEFORE writing any shared file. The theory, the ROOT and the
# result file are all fixed paths under $SESS_DIR, so a second concurrent query
# that wrote them first would make this one answer the WRONG question (or wipe
# its result between the ML write and the read below).
exec 9>"$WORK/.lock"
if ! flock -w "$LOCK_WAIT" 9; then
  echo "isa-query: timed out waiting for another query to finish" >&2
  exit 1
fi

# Isabelle treats a session as current when its source + input-heap digests are
# unchanged, and then SKIPS executing the theory and exits 0 — so a repeated
# identical query would rebuild nothing, write no result, and report "(no
# output captured)". Since we deleted the previous result, that reads as a dead
# tool. A per-invocation nonce in the theory text keeps the source digest
# unique, forcing the diagnostic to actually run every time.
NONCE="$$-${RANDOM}-$(date +%s%N)"

cat > "$SESS_DIR/IsaQuery.thy" <<EOF
theory IsaQuery
  imports "$IMPORTS"
begin
(* isa-query invocation nonce: $NONCE — forces re-execution; see script header *)
ML \\<open>
  val out = Path.explode "$E_OUT";
  val _ = File.write out "";
  fun w s = File.append out (Protocol_Message.clean_output s ^ "\n");
  val ctxt = \\<^context>;
  val thy = \\<^theory>;
  val _ =
    (case Exn.result (Print_Mode.setmp [] (fn () => let $ML_BODY in () end)) () of
       Exn.Res _ => ()
     | Exn.Exn exn => w ("ERROR: " ^ Runtime.exn_message exn));
\\<close>
end
EOF

cat > "$SESS_DIR/ROOT" <<EOF
session "IsaQuery" in "." = "$PARENT" +
  theories
    IsaQuery
EOF

rm -f "$OUT"

# NOTE: no `-o system_heaps=true` — see HEAP POLICY above.
# Capture the real status: `rc=$?` inside `if ! cmd; then` would record the
# NEGATED status (always 1) and mask timeout's 124, which is the one failure
# mode worth distinguishing (widen ISA_TIMEOUT vs fix the query).
set +e
nice -n 15 timeout "$TIMEOUT" "$ISA" build \
  -o "threads=$THREADS" \
  "${EXTRA_D[@]}" \
  -d "$SESS_DIR" \
  IsaQuery >"$SESS_DIR/build.log" 2>&1
rc=$?
set -e

if [[ $rc -ne 0 ]]; then
  if [[ $rc -eq 124 ]]; then
    echo "### isa-query: TIMED OUT after ${TIMEOUT}s (raise ISA_TIMEOUT for a slow sledgehammer)" >&2
  else
    echo "### isa-query: isabelle build failed (rc=$rc); tail of build.log:" >&2
  fi
  tail -20 "$SESS_DIR/build.log" >&2
  if [[ -s "$OUT" ]]; then echo "### partial result:"; cat "$OUT"; fi
  exit 1
fi

if [[ -s "$OUT" ]]; then
  cat "$OUT"
  # The ML wrapper CATCHES diagnostic exceptions and writes `ERROR: ...` instead
  # of re-raising, so Isabelle still reports the theory as successfully built and
  # the shell would otherwise exit 0 — a failed query looking like a good one to
  # anything checking status (e.g. `isa-query thm_oracles NoSuchTheorem`).
  if head -1 "$OUT" | grep -q '^ERROR:'; then
    exit 1
  fi
else
  echo "(no output captured — command produced nothing)"
fi
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

    # Advisory warm pre-check module. Self-contained (no trellis package
    # imports) so it runs standalone inside the worker sandbox without the
    # package on sys.path; provision it by copying the source verbatim. Phase 3:
    # on an Isabelle-backend run with the warm session ON, provision the WARM
    # ISABELLE advisory module (which drives the isabelle-warm-advisory socket
    # op); otherwise provision the Lean lean-server advisory module, byte-for-
    # byte unchanged. The `incremental-check` wrapper (below) is backend-gated to
    # exec whichever module is provisioned.
    if _repo_targets_isabelle_warm(repo_path):
        incremental_module_name = "isabelle_incremental_check.py"
    else:
        incremental_module_name = "incremental_check.py"
    incremental_src = Path(__file__).resolve().parent / incremental_module_name
    incremental_dst = scripts_dir / incremental_module_name
    incremental_dst.write_text(
        incremental_src.read_text(encoding="utf-8"), encoding="utf-8"
    )
    incremental_dst.chmod(0o755)
    if gid is not None:
        try:
            os.chown(str(incremental_dst), -1, gid)
        except PermissionError:
            pass

    incremental_check_path = scripts_dir / "incremental-check"
    incremental_check_path.write_text(
        generate_incremental_check_sh(
            repo_path,
            state_dir,
        ),
        encoding="utf-8",
    )
    incremental_check_path.chmod(0o755)
    if gid is not None:
        try:
            os.chown(str(incremental_check_path), -1, gid)
        except PermissionError:
            pass

    # Isabelle library-discovery helper. Emitted ONLY on an Isabelle-backend run,
    # so a Lean repo's `.trellis/scripts/` contents stay byte-identical. Lives
    # beside `incremental-check` in the (worker-READ-ONLY) scripts dir, so the
    # worker can exec it but cannot rewrite it; its scratch session goes under
    # `.trellis/scratch/`, which IS worker-writable.
    if _repo_targets_isabelle(repo_path):
        isa_query_path = scripts_dir / "isa-query"
        isa_query_path.write_text(
            generate_isa_query_sh(
                repo_path,
                state_dir,
            ),
            encoding="utf-8",
        )
        isa_query_path.chmod(0o755)
        if gid is not None:
            try:
                os.chown(str(isa_query_path), -1, gid)
            except PermissionError:
                pass


_ATOMIC_ACTION_COMMANDS = frozenset({
    "lean-compile-node",
    "lean-build-tablet",
    "prepare-compiled-support",
    "materialize-tablet-oleans",
    "print-axioms",
    "local-closure-axioms",
    "lean-semantic-payloads",
    "sync-tablet-support",
    # Isabelle backend ops. Forwarded to atomic_actions_main exactly like
    # local-closure-axioms / sync-tablet-support: the same code path reaches
    # the socket layer in atomic_actions/cli.py, where each is a server-only
    # op that routes through TRELLIS_CHECKER_SOCKET (no host fallback).
    "isabelle-check-node",
    "isabelle-thm-oracles",
    "isabelle-thm-deps",
    "isabelle-build-session",
    "isabelle-sync-session",
})

_ARTIFACT_COMMANDS = frozenset({
    "trellis-worker-result",
    "trellis-reviewer-result",
    "trellis-audit-result",
    "trellis-stuck-math-audit-result",
    "paper-faithfulness-result",
    "deviation-authorization-result",
    "substantiveness-result",
    "correspondence-result",
    "soundness-result",
})


def main(argv: Optional[Sequence[str]] = None) -> int:
    raw_args = list(argv if argv is not None else sys.argv[1:])
    if not raw_args:
        print("FAIL: command is required")
        return 2

    command = raw_args[0]
    rest = raw_args[1:]
    if command == "phase0-candidate":
        return _phase0_candidate_main(rest)
    if command in _ATOMIC_ACTION_COMMANDS:
        return atomic_actions_main(raw_args)
    if command == "sync-supervisor-workspace":
        return _sync_supervisor_workspace_main(rest)
    if command == "node":
        return _node_main(rest)
    if command == "tablet":
        return _tablet_main(rest)
    if command in _ARTIFACT_COMMANDS:
        return _artifact_main(command, rest)

    print(f"FAIL: unknown command: {command}")
    print(
        "valid commands: "
        + ", ".join(
            sorted(
                _ATOMIC_ACTION_COMMANDS
                | {"phase0-candidate", "sync-supervisor-workspace", "node", "tablet"}
                | _ARTIFACT_COMMANDS
            )
        )
    )
    return 2


def _phase0_candidate_main(argv: Sequence[str]) -> int:
    """Request diagnostics from the supervisor-owned Phase-0 broker.

    The command accepts no candidate or tool paths. The server freezes the
    authoritative candidate and constructs its request from genesis pins.
    """
    if argv:
        print("FAIL: phase0-candidate accepts no arguments")
        return 2
    socket_path = os.environ.get("TRELLIS_PHASE0_CHECKER_SOCKET", "").strip()
    token = os.environ.get("TRELLIS_PHASE0_CHECKER_TOKEN", "").strip()
    if not socket_path or not token:
        print("FAIL: Phase-0 checker broker is unavailable")
        return 2
    request = json.dumps(
        {"schema": "trellis-phase0-broker-call/v1", "operation": "candidate", "token": token},
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8") + b"\n"
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(3605.0)
            client.connect(socket_path)
            client.sendall(request)
            chunks: list[bytes] = []
            size = 0
            while True:
                chunk = client.recv(65536)
                if not chunk:
                    break
                size += len(chunk)
                if size > 16 * 1024 * 1024:
                    raise OSError("broker response exceeds 16 MiB")
                chunks.append(chunk)
        response = json.loads(b"".join(chunks))
    except (OSError, ValueError) as exc:
        print(f"FAIL: Phase-0 checker broker call failed: {exc}")
        return 1
    print(json.dumps(response, ensure_ascii=False, sort_keys=True, separators=(",", ":")))
    return 0 if response.get("status") in {"passed", "failed"} else 1


def write_phase0_candidate_script(attempt_root: Path) -> Path:
    """Install the Phase-0-only broker client in an otherwise raw attempt.

    The generated program is deliberately standalone: the worker receives no
    Trellis source checkout, kernel binary, extractor, or translation tools.
    The sandbox binds the attempt read-only and overlays only ``candidate`` and
    ``agent-output`` as writable, so this file is immutable during a burst.
    """
    scripts_dir = attempt_root.resolve() / ".trellis" / "scripts"
    scripts_dir.mkdir(parents=True, exist_ok=True)
    path = scripts_dir / "check.py"
    program = '''#!/usr/bin/env python3
import json
import os
import socket
import sys

def main():
    if sys.argv[1:] != ["phase0-candidate"]:
        print("FAIL: only phase0-candidate with no further arguments is available")
        return 2
    socket_path = os.environ.get("TRELLIS_PHASE0_CHECKER_SOCKET", "").strip()
    token = os.environ.get("TRELLIS_PHASE0_CHECKER_TOKEN", "").strip()
    if not socket_path or not token:
        print("FAIL: Phase-0 checker broker is unavailable")
        return 2
    payload = json.dumps(
        {"operation": "candidate", "schema": "trellis-phase0-broker-call/v1", "token": token},
        ensure_ascii=False, sort_keys=True, separators=(",", ":"),
    ).encode("utf-8") + b"\\n"
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(3605.0)
            client.connect(socket_path)
            client.sendall(payload)
            chunks = []
            size = 0
            while True:
                chunk = client.recv(65536)
                if not chunk:
                    break
                size += len(chunk)
                if size > 16 * 1024 * 1024:
                    raise OSError("broker response exceeds 16 MiB")
                chunks.append(chunk)
        response = json.loads(b"".join(chunks))
    except (OSError, ValueError) as exc:
        print(f"FAIL: Phase-0 checker broker call failed: {exc}")
        return 1
    print(json.dumps(response, ensure_ascii=False, sort_keys=True, separators=(",", ":")))
    return 0 if response.get("status") in {"passed", "failed"} else 1

if __name__ == "__main__":
    raise SystemExit(main())
'''
    if path.exists():
        path.chmod(0o755)
    path.write_text(program, encoding="utf-8")
    path.chmod(0o555)
    return path


__all__ = [
    "build_trellis_worker_acceptance_context",
    "check_node",
    "check_tablet",
    "check_tablet_scoped",
    "generate_check_node_sh",
    "generate_check_tablet_sh",
    "generate_incremental_check_sh",
    "generate_isabelle_incremental_check_sh",
    "main",
    "normalize_trellis_audit_result_data",
    "normalize_trellis_stuck_math_audit_result_data",
    "normalize_trellis_reviewer_result_data",
    "normalize_trellis_worker_result_data",
    "load_worker_checker_trace",
    "record_worker_checker_trace",
    "validate_json_artifact",
    "write_phase0_candidate_script",
    "write_scripts",
]
