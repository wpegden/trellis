"""Single-request execution and artifact normalization for the shared wrapper."""

from __future__ import annotations

from dataclasses import dataclass
import json
import os
import time
from pathlib import Path
from typing import Callable, Dict, Optional, Protocol

from trellis.adapters import BurstResult
from trellis.artifacts import artifact_stem, done_marker_path, raw_json_path
from trellis.burst import run_reviewer_burst, run_worker_burst
from trellis.burst_home import cleanup_burst_home
from trellis.checking import validate_json_artifact
from trellis.json_io import append_jsonl, load_json, timestamp_now
from trellis.project_paths import project_feedback_log_path, project_state_dir_for_repo
from trellis.runtime.kernel_cli import KernelCliError, run_kernel_cli

from .protocol import AgentLane, ArtifactSpec, SingleAgentRequest, SingleAgentResponse


@dataclass(frozen=True)
class ArtifactPaths:
    canonical: Path
    raw: Path
    done: Path
    stem: Path


@dataclass(frozen=True)
class ArtifactPromotionResult:
    payload: Optional[Dict[str, object]]
    error: str = ""
    comments: str = ""
    paths: Optional[ArtifactPaths] = None


class LanePortResolver(Protocol):
    def resolve(self, lane: AgentLane, provider_name: str) -> Optional[int]:
        ...


class DefaultLanePortResolver:
    """Resolve logical lanes to the shared fixed port allocation."""

    def __init__(self, *, base_offset: Optional[int] = None) -> None:
        if base_offset is None:
            base_offset = _wrapper_port_base_offset()
        self.base_offset = base_offset

    def resolve(self, lane: AgentLane, provider_name: str) -> Optional[int]:
        if provider_name not in {"claude", "gemini"}:
            return None
        if lane.kind == "worker":
            return 3284 + self.base_offset
        if lane.kind == "reviewer":
            return 3285 + self.base_offset
        if lane.kind == "stuck_math_audit":
            return 3285 + self.base_offset
        if lane.kind == "correspondence":
            return 3286 + self.base_offset + (lane.agent_index * 2)
        if lane.kind == "soundness-batch":
            return 3310 + self.base_offset + (lane.agent_index * 2)
        if lane.kind == "soundness-node":
            return 3310 + self.base_offset + ((lane.node_index % 5) * 10) + (lane.agent_index * 2)
        return None


def _wrapper_port_base_offset() -> int:
    raw = str(os.environ.get("TRELLIS_WRAPPER_PORT_BASE_OFFSET", "") or "").strip()
    if not raw:
        return 0
    try:
        value = int(raw)
    except ValueError:
        return 0
    return max(0, value)


def _record_system_feedback(
    request: SingleAgentRequest,
    *,
    artifact_name: str,
    system_feedback: str,
) -> None:
    text = str(system_feedback or "").strip()
    if not text:
        return
    append_jsonl(
        project_feedback_log_path(project_state_dir_for_repo(request.work_dir)),
        {
            "timestamp": timestamp_now(),
            "cycle": int(request.cycle or 0),
            "kind": request.kind,
            "burst_role": request.burst_role,
            "lane": request.lane.key(),
            "artifact": artifact_name,
            "system_feedback": text,
        },
        mode=0o600,
    )
    # Per fail-loudly policy: every system_feedback emission pauses the run.
    _write_system_feedback_halt_marker(
        request,
        artifact_name=artifact_name,
        system_feedback=text,
    )


def _write_system_feedback_halt_marker(
    request: SingleAgentRequest,
    *,
    artifact_name: str,
    system_feedback: str,
) -> None:
    """Persist a halt marker at `<runtime_root>/system_feedback_halt.json`
    whenever an agent burst returns a non-empty `system_feedback` string.
    Mirrors the checker-disagreement marker pattern (commit 18b59ef):
    sticky across restarts, operator clears by deletion, first marker
    wins so we don't clobber the original diagnostic.
    """
    runtime_root_raw = os.environ.get("TRELLIS_KERNEL_CACHE_ROOT", "").strip()
    if not runtime_root_raw:
        # Test fixtures / replay tools: degrade silently. The feedback
        # log entry above is still recorded.
        return
    try:
        runtime_root = Path(runtime_root_raw)
    except (TypeError, ValueError):
        return
    try:
        runtime_root.mkdir(parents=True, exist_ok=True)
    except OSError:
        return
    marker_path = runtime_root / "system_feedback_halt.json"
    if marker_path.exists():
        # First emission is load-bearing; preserve the original
        # diagnostic. Matches `existing_halt_marker_is_preserved_not_overwritten`
        # semantics on the Rust side.
        return
    payload: Dict[str, object] = {
        "kind": "system_feedback",
        "schema_version": 1,
        "active_node": str(request.lane.node_name or ""),
        "active_coarse_node": "",
        "cycle": int(request.cycle or 0),
        "request_id": str(request.request_id or ""),
        "request_kind": str(request.kind or ""),
        "burst_role": str(request.burst_role or ""),
        "lane": request.lane.key(),
        "artifact": str(artifact_name or ""),
        "system_feedback": system_feedback,
        "reason": "agent burst returned non-empty system_feedback string",
        "unix_ts": int(time.time()),
        "clear_instructions": (
            "The trellis supervisor is HALTED because an agent burst "
            f"returned a non-empty `system_feedback` string on "
            f"request_id={request.request_id} (kind={request.kind}, "
            f"node={request.lane.node_name or ''}, cycle={request.cycle}). "
            "Every system_feedback emission is treated as a design-gap "
            "signal that requires human inspection — the supervisor will "
            "not dispatch new bursts until you review the `system_feedback` "
            f"field above and then DELETE this file to resume: rm {marker_path}"
        ),
    }
    try:
        tmp_path = marker_path.with_suffix(marker_path.suffix + ".tmp")
        tmp_path.write_text(json.dumps(payload, indent=2))
        os.replace(tmp_path, marker_path)
    except OSError:
        # Best-effort: a write failure shouldn't crash the wrapper. The
        # feedback log entry is the durability fallback.
        return


def _extract_private_system_feedback(raw_path: Path) -> str:
    try:
        data = load_json(raw_path, default={})
    except Exception:
        return ""
    if not isinstance(data, dict):
        return ""
    return str(data.get("system_feedback", "") or "").strip()


def prepare_artifact_paths(
    state_dir: Path,
    repo_path: Path,
    canonical_name: str,
) -> ArtifactPaths:
    return ArtifactPaths(
        canonical=repo_path / canonical_name,
        raw=raw_json_path(state_dir, canonical_name),
        done=done_marker_path(state_dir, canonical_name),
        stem=Path(artifact_stem(canonical_name)),
    )


def _clear_artifact_paths(paths: ArtifactPaths) -> None:
    paths.raw.unlink(missing_ok=True)
    paths.done.unlink(missing_ok=True)


def _kernel_soundness_fingerprint(repo_path: Path, node_name: str) -> str:
    try:
        response = run_kernel_cli(
            {
                "action": "observe_soundness_fingerprints",
                "repo_path": str(repo_path),
                "nodes": [node_name],
            }
        )
    except KernelCliError:
        return ""
    if response.get("status") != "observe_soundness_fingerprints_ok":
        return ""
    output = response.get("output")
    if not isinstance(output, dict):
        return ""
    return str(output.get(node_name, "") or "").strip()


def validate_and_promote_artifact(
    request: SingleAgentRequest,
    *,
    artifact: ArtifactSpec,
) -> ArtifactPromotionResult:
    paths = prepare_artifact_paths(request.state_dir, request.work_dir, artifact.canonical_name)
    validation = validate_json_artifact(
        artifact.kind,
        paths.raw,
        phase=artifact.phase,
        node_name=artifact.node_name,
        repo=request.work_dir,
        invalid_attempt=artifact.invalid_attempt,
    )
    if not validation["ok"]:
        return ArtifactPromotionResult(
            payload=None,
            error="; ".join(validation["errors"]),
            paths=paths,
        )

    data = validation["data"]
    assert isinstance(data, dict)
    promoted = dict(data)
    comments = str(promoted.get("comments", promoted.get("feedback", "")) or "")
    if artifact.kind == "soundness-result" and artifact.node_name:
        fp = _kernel_soundness_fingerprint(request.work_dir, artifact.node_name)
        if fp:
            meta = promoted.get("_supervisor_meta", {})
            if not isinstance(meta, dict):
                meta = {}
            meta["soundness_fingerprint"] = fp
            promoted["_supervisor_meta"] = meta

    return ArtifactPromotionResult(
        payload=promoted,
        comments=comments,
        paths=paths,
    )


def execute_agent_request(
    request: SingleAgentRequest,
    *,
    port_resolver: Optional[LanePortResolver] = None,
    worker_runner: Optional[Callable[..., BurstResult]] = None,
    reviewer_runner: Optional[Callable[..., BurstResult]] = None,
    validate_artifact: bool = True,
) -> SingleAgentResponse:
    port_resolver = port_resolver or DefaultLanePortResolver()
    worker_runner = worker_runner or run_worker_burst
    reviewer_runner = reviewer_runner or run_reviewer_burst
    request.state_dir.mkdir(parents=True, exist_ok=True)
    if request.log_dir is not None:
        request.log_dir.mkdir(parents=True, exist_ok=True)
    artifact_paths: Optional[ArtifactPaths] = None
    if request.artifact is not None:
        artifact_paths = prepare_artifact_paths(
            request.state_dir,
            request.work_dir,
            request.artifact.canonical_name,
        )
        artifact_paths.raw.parent.mkdir(parents=True, exist_ok=True)
        _clear_artifact_paths(artifact_paths)

    artifact_prefix = request.artifact_prefix
    if artifact_prefix is None and artifact_paths is not None:
        artifact_prefix = str(artifact_paths.stem)

    port = port_resolver.resolve(request.lane, request.provider.provider)

    # Phase 4 bwrap-only migration: the bridge seeds a per-role
    # stable fake-home under `<runtime>/burst-homes/<worker|reviewer>/`
    # and threads it through `request.burst_home`. The per-role homes
    # are NOT cleaned up between bursts — codex stores absolute rollout
    # paths in its state DB and the next burst's `codex exec resume`
    # only works if the prior burst's rollout file still lives where
    # the DB says it does. (The legacy per-session-name fake-homes
    # were deleted at burst exit; that path is preserved here for
    # tests / configs that still produce non-persistent home keys.)
    burst_home_to_cleanup: Optional[Path] = None
    if request.burst_home is not None:
        try:
            home_resolved = request.burst_home.resolve()
            persistent_home = home_resolved.name in {"worker", "reviewer"}
            if "burst-homes" in home_resolved.parts and not persistent_home:
                burst_home_to_cleanup = home_resolved
        except OSError:
            burst_home_to_cleanup = None

    burst_result: BurstResult
    try:
        if request.burst_role == "worker":
            burst_result = worker_runner(
                request.provider,
                request.prompt,
                session_name=request.session_name,
                work_dir=request.work_dir,
                timeout_seconds=request.timeout_seconds,
                startup_timeout_seconds=request.startup_timeout_seconds,
                log_dir=request.log_dir,
                port=port,
                session_scope=request.session_scope,
                fresh=request.fresh,
                done_file=artifact_paths.done if artifact_paths is not None else None,
                artifact_prefix=artifact_prefix,
                sandbox=request.sandbox,
                burst_home=request.burst_home,
            )
        else:
            burst_result = reviewer_runner(
                request.provider,
                request.prompt,
                session_name=request.session_name,
                work_dir=request.work_dir,
                role=request.burst_role,
                timeout_seconds=request.timeout_seconds,
                startup_timeout_seconds=request.startup_timeout_seconds,
                log_dir=request.log_dir,
                port=port,
                session_scope=request.session_scope,
                fresh=request.fresh,
                done_file=artifact_paths.done if artifact_paths is not None else None,
                artifact_prefix=artifact_prefix,
                sandbox=request.sandbox,
                burst_home=request.burst_home,
            )
    finally:
        if burst_home_to_cleanup is not None:
            cleanup_burst_home(burst_home_to_cleanup)

    payload: Optional[Dict[str, object]] = None
    error = str(getattr(burst_result, "error", "") or "")
    comments = ""
    if validate_artifact and bool(getattr(burst_result, "ok", False)) and request.artifact is not None:
        promotion = validate_and_promote_artifact(
            request,
            artifact=request.artifact,
        )
        payload = promotion.payload
        comments = promotion.comments
        artifact_paths = promotion.paths
        if payload is None:
            error = promotion.error or "missing validated artifact"
    if (
        bool(getattr(burst_result, "ok", False))
        and request.artifact is not None
        and artifact_paths is not None
        and artifact_paths.raw.is_file()
    ):
        _record_system_feedback(
            request,
            artifact_name=request.artifact.canonical_name,
            system_feedback=_extract_private_system_feedback(artifact_paths.raw),
        )

    return SingleAgentResponse(
        request_id=request.request_id,
        cycle=request.cycle,
        kind=request.kind,
        burst_role=request.burst_role,
        ok=(
            bool(getattr(burst_result, "ok", False))
            and (
                request.artifact is None
                or not validate_artifact
                or payload is not None
            )
        ),
        payload=payload,
        error=error,
        comments=comments,
        usage=getattr(burst_result, "usage", None),
        captured_output=str(getattr(burst_result, "captured_output", "") or ""),
        exit_code=getattr(burst_result, "exit_code", None),
        stall_recoveries=int(getattr(burst_result, "stall_recoveries", 0) or 0),
        transcript_path=getattr(burst_result, "transcript_path", None),
        walltime_seconds=float(getattr(burst_result, "duration_seconds", 0.0) or 0.0),
        canonical_path=artifact_paths.canonical if artifact_paths is not None else None,
        raw_path=artifact_paths.raw if artifact_paths is not None else None,
        done_path=artifact_paths.done if artifact_paths is not None else None,
    )
