"""Analyze whether a live in-flight request appears stalled.

The analyzer intentionally uses durable artifacts first:

- external runtime root (`protocol_state.json`, `event_log.jsonl`)
- repo-local bridge state (`.trellis/runtime/<runtime-name>/staging`, `logs`)
- chat artifacts under `.trellis/chats/live`
- request-local filesystem activity such as target-node and scratch writes

This makes it robust against stale PTY screens and reusable agent sessions.
"""

from __future__ import annotations

import argparse
import json
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional, Tuple

from trellis.project_paths import (
    project_config_path,
    project_policy_path,
    project_scratch_dir,
    project_state_dir_for_repo,
)


DEFAULT_STALL_THRESHOLD_SECONDS = 900.0
PROMPT_ONLY_GRACE_MULTIPLIER = 2.0


@dataclass(frozen=True)
class RuntimeLayout:
    runtime_root: Path
    protocol_state_path: Path
    event_log_path: Path
    state_dir: Path


def _read_json(path: Path) -> Dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def _discover_runtime_layout(repo_path: Path) -> Optional[RuntimeLayout]:
    repo_path = repo_path.resolve()
    config_path = project_config_path(repo_path).resolve()
    candidates: List[Path] = []

    repo_state_dir = project_state_dir_for_repo(repo_path)
    runtime_dir = repo_state_dir / "runtime"
    if runtime_dir.is_dir():
        for child in sorted(runtime_dir.iterdir()):
            if child.is_dir():
                candidates.append(child.resolve())

    extra_roots = os.environ.get("TRELLIS_VIEWER_RUNTIME_ROOTS", "")
    for raw_root in extra_roots.split(os.pathsep):
        raw_root = raw_root.strip()
        if raw_root:
            candidates.append(Path(raw_root).resolve())

    search_roots: List[Path] = []
    for root in (
        repo_path,
        repo_path.parent,
        repo_path.parent.parent,
        Path(os.environ.get("PROJECTS_ROOT", "")).resolve()
        if os.environ.get("PROJECTS_ROOT")
        else None,
    ):
        if root and root.exists():
            search_roots.append(root)

    for search_root in _unique_paths(search_roots):
        candidates.extend(_discover_runtime_roots(search_root, max_depth=4))

    best: Optional[RuntimeLayout] = None
    best_mtime = -1.0
    for root in _unique_paths(candidates):
        metadata_path = root / "runtime_metadata.json"
        protocol_state_path = root / "protocol_state.json"
        if not metadata_path.is_file() or not protocol_state_path.is_file():
            continue
        try:
            metadata = _read_json(metadata_path)
        except Exception:
            continue
        metadata_repo = str(metadata.get("repo_path", "") or "").strip()
        metadata_config = str(metadata.get("config_path", "") or "").strip()
        if metadata_repo and Path(metadata_repo).resolve() == repo_path:
            pass
        elif metadata_config and Path(metadata_config).resolve() == config_path:
            pass
        else:
            continue
        mtime = protocol_state_path.stat().st_mtime
        state_dir = repo_state_dir / "runtime" / root.name
        info = RuntimeLayout(
            runtime_root=root,
            protocol_state_path=protocol_state_path,
            event_log_path=root / "event_log.jsonl",
            state_dir=state_dir,
        )
        if mtime > best_mtime:
            best = info
            best_mtime = mtime
    return best


def _unique_paths(paths: Iterable[Path]) -> List[Path]:
    unique: List[Path] = []
    seen: set[Path] = set()
    for path in paths:
        if path in seen:
            continue
        seen.add(path)
        unique.append(path)
    return unique


def _discover_runtime_roots(search_root: Path, *, max_depth: int) -> List[Path]:
    results: List[Path] = []
    base_depth = len(search_root.parts)
    for current, dirs, files in os.walk(search_root):
        current_path = Path(current)
        depth = len(current_path.parts) - base_depth
        if depth > max_depth:
            dirs[:] = []
            continue
        dirs[:] = [
            name
            for name in dirs
            if name not in {".git", "node_modules", "target", "__pycache__", ".lake"}
        ]
        if "runtime_metadata.json" in files and "protocol_state.json" in files:
            results.append(current_path.resolve())
    return results


def _stall_threshold_seconds(repo_path: Path) -> float:
    path = project_policy_path(repo_path)
    if not path.is_file():
        return DEFAULT_STALL_THRESHOLD_SECONDS
    try:
        raw = _read_json(path)
    except Exception:
        return DEFAULT_STALL_THRESHOLD_SECONDS
    timing = raw.get("timing")
    if not isinstance(timing, dict):
        return DEFAULT_STALL_THRESHOLD_SECONDS
    try:
        value = float(timing.get("stall_threshold_seconds", DEFAULT_STALL_THRESHOLD_SECONDS))
        return value if value > 0 else DEFAULT_STALL_THRESHOLD_SECONDS
    except Exception:
        return DEFAULT_STALL_THRESHOLD_SECONDS


def _request_kind_slug(kind: str) -> str:
    return str(kind or "").strip().lower()


def _artifact_stems(state_dir: Path, request_kind: str, request_id: int) -> List[str]:
    kind = _request_kind_slug(request_kind)
    if kind == "worker":
        return [f"trellis_worker_{request_id}_result"]
    if kind == "review":
        return [f"trellis_review_{request_id}_decision"]
    staging = state_dir / "staging"
    if not staging.is_dir():
        return []
    prefix = f"trellis_{kind}_{request_id}_"
    stems = {
        path.name[:-13]
        for path in staging.glob(f"{prefix}*.request.json")
        if path.name.endswith(".request.json")
    }
    return sorted(stems)


def _request_artifact_paths(state_dir: Path, stem: str) -> Dict[str, Path]:
    staging = state_dir / "staging"
    logs = state_dir / "logs"
    return {
        "request": staging / f"{stem}.request.json",
        "raw": staging / f"{stem}.raw.json",
        "done": staging / f"{stem}.done",
        "prompt_log": logs / f"{stem}-prompt.txt",
    }


def _chat_dir(repo_path: Path, stem: str) -> Path:
    return project_state_dir_for_repo(repo_path) / "chats" / "live" / stem


def _mtime(path: Path) -> Optional[float]:
    try:
        return path.stat().st_mtime
    except Exception:
        return None


def _backend_session_status_for_role(role: str) -> Optional[str]:
    """Best-effort liveness probe for the active backend's agent session.

    Returns one of "running", "stable", or None. Currently always returns
    None: the tmux backend has no always-on status endpoint — its liveness
    signal is folded into per-burst transcript/pane scraping and reported
    via `BurstResult` rather than out-of-band. The stall analyzer falls
    through to its artifact-based decision branches.

    Kept as a named hook so a future session-status probe (e.g. "pane was
    modified within last N seconds") could slot in without rearranging
    the analyzer's control flow.
    """
    del role  # currently unused — see docstring
    return None


def _gather_activity(
    *,
    repo_path: Path,
    request: Dict[str, Any],
    stems: List[str],
    state_dir: Path,
    request_started_at: float,
) -> List[Tuple[float, str, str]]:
    activity: List[Tuple[float, str, str]] = []
    for stem in stems:
        paths = _request_artifact_paths(state_dir, stem)
        for label, path in paths.items():
            ts = _mtime(path)
            if ts is not None:
                activity.append((ts, label, str(path)))
        chat_dir = _chat_dir(repo_path, stem)
        if chat_dir.is_dir():
            for name in ("prompt.txt", "output.log", "transcript.jsonl", "transcript.json"):
                path = chat_dir / name
                ts = _mtime(path)
                if ts is not None:
                    activity.append((ts, f"chat:{name}", str(path)))

    if _request_kind_slug(request.get("kind", "")) == "worker":
        active_node = str(request.get("active_node", "") or request.get("node", "") or "").strip()
        if active_node:
            for suffix in (".lean", ".tex"):
                path = repo_path / "Tablet" / f"{active_node}{suffix}"
                ts = _mtime(path)
                if ts is not None and ts >= request_started_at:
                    activity.append((ts, f"active_node:{suffix[1:]}", str(path)))
        scratch_dir = project_scratch_dir(project_state_dir_for_repo(repo_path))
        if scratch_dir.is_dir():
            for path in scratch_dir.rglob("*"):
                if not path.is_file():
                    continue
                ts = _mtime(path)
                if ts is not None and ts >= request_started_at:
                    activity.append((ts, "scratch", str(path)))
    return sorted(activity)


def _is_substantive_activity(label: str) -> bool:
    return label not in {"request", "prompt_log", "chat:prompt.txt"}


def analyze_inflight_request(
    repo_path: Path,
    *,
    runtime_root: Optional[Path] = None,
    now: Optional[float] = None,
) -> Dict[str, Any]:
    repo_path = repo_path.resolve()
    layout = (
        RuntimeLayout(
            runtime_root=runtime_root.resolve(),
            protocol_state_path=runtime_root.resolve() / "protocol_state.json",
            event_log_path=runtime_root.resolve() / "event_log.jsonl",
            state_dir=project_state_dir_for_repo(repo_path) / "runtime" / runtime_root.resolve().name,
        )
        if runtime_root is not None
        else _discover_runtime_layout(repo_path)
    )
    if layout is None or not layout.protocol_state_path.is_file():
        return {
            "ok": False,
            "status": "no_runtime",
            "stalled": False,
            "reason": "no matching runtime root found",
        }

    protocol_state = _read_json(layout.protocol_state_path)
    request = protocol_state.get("in_flight_request")
    if not isinstance(request, dict):
        return {
            "ok": True,
            "status": "no_request",
            "stalled": False,
            "runtime_root": str(layout.runtime_root),
            "state_dir": str(layout.state_dir),
        }

    request_id = int(request.get("id", 0) or 0)
    request_kind = _request_kind_slug(request.get("kind", ""))
    stems = _artifact_stems(layout.state_dir, request_kind, request_id)
    stall_threshold = _stall_threshold_seconds(repo_path)
    clock = float(now if now is not None else __import__("time").time())

    artifact_paths = [_request_artifact_paths(layout.state_dir, stem) for stem in stems]
    request_times = [
        ts
        for paths in artifact_paths
        for key in ("request", "prompt_log")
        for ts in [_mtime(paths[key])]
        if ts is not None
    ]
    request_started_at = min(request_times) if request_times else _mtime(layout.protocol_state_path) or clock

    activity = _gather_activity(
        repo_path=repo_path,
        request=request,
        stems=stems,
        state_dir=layout.state_dir,
        request_started_at=request_started_at,
    )
    last_progress = activity[-1] if activity else None
    substantive_activity = [entry for entry in activity if _is_substantive_activity(entry[1])]
    last_substantive_progress = substantive_activity[-1] if substantive_activity else None
    last_progress_seconds_ago = (
        max(0.0, clock - last_progress[0]) if last_progress is not None else max(0.0, clock - request_started_at)
    )
    last_substantive_progress_seconds_ago = (
        max(0.0, clock - last_substantive_progress[0])
        if last_substantive_progress is not None
        else None
    )

    raw_exists = any(paths["raw"].exists() for paths in artifact_paths)
    done_exists = any(paths["done"].exists() for paths in artifact_paths)
    prompt_exists = any(paths["prompt_log"].exists() for paths in artifact_paths) or any(
        _chat_dir(repo_path, stem).joinpath("prompt.txt").exists() for stem in stems
    )

    role = "worker" if request_kind == "worker" else "reviewer" if request_kind == "review" else ""
    backend_status = _backend_session_status_for_role(role) if role else None

    status = "active"
    reason_code = "progressing"
    reason = "request shows recent activity"
    stalled = False

    if raw_exists and done_exists:
        status = "post_handoff_pending"
        reason_code = "awaiting_consumption"
        reason = "raw and done artifacts exist; waiting for runtime consumption"
        if last_progress_seconds_ago > stall_threshold:
            status = "stalled"
            reason_code = "post_handoff_unconsumed"
            reason = "raw and done artifacts exist, but the runtime has not consumed them within the stall threshold"
            stalled = True
    elif backend_status == "running":
        status = "active"
        reason_code = "backend_running"
        reason = "agent backend reports running"
    elif backend_status == "stable":
        status = "idle"
        reason_code = "backend_stable"
        reason = "agent backend is stable and no handoff artifacts exist yet"
        if last_substantive_progress is not None:
            if last_substantive_progress_seconds_ago is not None and last_substantive_progress_seconds_ago > stall_threshold:
                status = "stalled"
                reason_code = "stable_after_work_no_handoff"
                reason = (
                    "agent backend is stable, the request showed substantive activity, "
                    "but there has been no further progress or handoff within the stall threshold"
                )
                stalled = True
        elif prompt_exists and (clock - request_started_at) > stall_threshold * PROMPT_ONLY_GRACE_MULTIPLIER:
            status = "stalled"
            reason_code = "stable_prompt_only_too_long"
            reason = (
                "agent backend is stable, only prompt-delivery artifacts are visible, "
                "and the request has remained quiet beyond the prompt-only grace window"
            )
            stalled = True
    elif prompt_exists and last_substantive_progress is not None and (
        last_substantive_progress_seconds_ago is not None and last_substantive_progress_seconds_ago > stall_threshold
    ):
        status = "stalled"
        reason_code = "progress_quiet_no_backend"
        reason = (
            "prompt and post-prompt activity exist, but there has been no further progress "
            "within the stall threshold and no backend status is available"
        )
        stalled = True
    elif prompt_exists and (clock - request_started_at) > stall_threshold * PROMPT_ONLY_GRACE_MULTIPLIER:
        status = "stalled"
        reason_code = "prompt_only_too_long"
        reason = (
            "prompt artifacts exist, but there is still no post-prompt activity and the "
            "request has exceeded the prompt-only grace window"
        )
        stalled = True

    signals: List[str] = []
    if prompt_exists:
        signals.append("prompt_exists")
    if raw_exists:
        signals.append("raw_exists")
    if done_exists:
        signals.append("done_exists")
    if last_substantive_progress is not None:
        signals.append("substantive_progress")
    if backend_status:
        signals.append(f"backend:{backend_status}")

    result = {
        "ok": True,
        "status": status,
        "stalled": stalled,
        "reason_code": reason_code,
        "reason": reason,
        "stall_threshold_seconds": stall_threshold,
        "prompt_only_grace_seconds": stall_threshold * PROMPT_ONLY_GRACE_MULTIPLIER,
        "request_age_seconds": max(0.0, clock - request_started_at),
        "last_progress_seconds_ago": last_progress_seconds_ago,
        "last_substantive_progress_seconds_ago": last_substantive_progress_seconds_ago,
        "runtime_root": str(layout.runtime_root),
        "state_dir": str(layout.state_dir),
        "request": {
            "id": request_id,
            "kind": request_kind,
            "cycle": int(request.get("cycle", 0) or 0),
            "phase": str(request.get("phase", "") or ""),
            "active_node": str(
                request.get("active_node", "")
                or protocol_state.get("active_node", "")
                or ""
            ).strip(),
        },
        "artifact_stems": stems,
        "prompt_exists": prompt_exists,
        "raw_exists": raw_exists,
        "done_exists": done_exists,
        "backend_status": backend_status,
        "signals": signals,
        "last_progress": (
            {
                "label": last_progress[1],
                "path": last_progress[2],
            }
            if last_progress is not None
            else None
        ),
        "last_substantive_progress": (
            {
                "label": last_substantive_progress[1],
                "path": last_substantive_progress[2],
            }
            if last_substantive_progress is not None
            else None
        ),
    }
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("repo_path")
    parser.add_argument("--runtime-root", default="")
    args = parser.parse_args()
    payload = analyze_inflight_request(
        Path(args.repo_path),
        runtime_root=Path(args.runtime_root).resolve() if str(args.runtime_root).strip() else None,
    )
    json.dump(payload, __import__("sys").stdout, indent=2)
    __import__("sys").stdout.write("\n")
    return 0


if __name__ == "__main__":  # pragma: no cover - CLI entrypoint
    raise SystemExit(main())
