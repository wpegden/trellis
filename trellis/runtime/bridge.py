"""Dumb Python bridge for the trellis supervisor runtime.

This module does not decide protocol behavior. It renders prompts from the
Rust-owned request payload, executes agents through the shared wrapper, and
maps validated raw outputs back into Rust-shaped responses.
"""

from __future__ import annotations

import hashlib
import json
import os
import secrets
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Dict, Iterable, List, Mapping, MutableMapping, Optional, Sequence

from trellis.adapters import ProviderConfig
from trellis.agent_wrapper.executor import DefaultLanePortResolver, execute_agent_request
from trellis.agent_wrapper.panels import execute_panel_raw
from trellis.burst_home import seed_burst_home
from trellis.agent_wrapper.protocol import (
    AgentLane,
    ArtifactSpec,
    PanelExecutionResponse,
    PanelRequest,
    SingleAgentRequest,
    SingleAgentResponse,
)
from trellis.checking import (
    build_trellis_worker_acceptance_context,
    load_worker_checker_trace,
    normalize_trellis_audit_result_data,
    normalize_trellis_reviewer_result_data,
    normalize_trellis_stuck_math_audit_result_data,
    normalize_trellis_worker_result_data,
    record_worker_checker_trace,
    write_scripts,
)
from trellis.config import Config, Policy, PolicyManager, load_config
from trellis.history_artifacts import (
    last_invalid_dir,
    last_invalid_metadata_path,
)
from trellis.json_io import load_json, save_json
from trellis.supervisor_workspace import (
    propagate_tablet_back_to_worker,
    sync_supervisor_workspace,
)
from trellis.worker_scratch import ensure_worker_scratch_workspace

from .bridge_prompts import (
    build_audit_prompt,
    build_paper_faithfulness_prompt,
    build_correspondence_prompt,
    build_review_prompt,
    build_soundness_prompt,
    build_stuck_math_audit_prompt,
    build_worker_prompt,
)
from .kernel_cli import KernelCliError, run_kernel_cli
from .bridge_protocol import BridgeCliRequest
class BridgeError(RuntimeError):
    """Raised when the trellis bridge cannot fulfill a request honestly."""


_CHECKER_MISMATCH_PREFIX = "authoritative checker mismatch:"
_CHECKER_MISMATCH_DETAIL_CHAR_LIMIT = 2000
_CHECKER_MISMATCH_ITEM_CHAR_LIMIT = 500


def _bridge_dir(runtime_root: Path) -> Path:
    return runtime_root / "bridge"


# ----- Phase 2/3 of the bwrap-only migration plan -----
# (SANDBOX_BWRAP_ONLY_MIGRATION_PLAN_2026-06-03.md §3)
#
# Each `handle_bridge_request` invocation is a fresh Python subprocess,
# so we mint a new burst token per dispatch (Phase 2), register it in
# the on-disk file the checker server reloads on every accept, and
# append a record to `burst-dispatch.jsonl` for post-hoc forensics
# (Phase 3). The token is exported via `os.environ` BEFORE any sandbox
# command is built so `sandbox._passthrough_value_envs()` forwards it
# into the burst via `--setenv TRELLIS_CHECKER_TOKEN`.
#
# The burst itself NEVER sees `burst-tokens.json` (the runtime root is
# not bind-mounted into the burst's bwrap), so the env-var path is the
# only channel by which the burst learns its own token.

_CHECKER_STATE_SUBDIR = "checker-state"
_BURST_TOKENS_FILENAME = "burst-tokens.json"
_BURST_DISPATCH_LOG_FILENAME = "burst-dispatch.jsonl"
# `feedback_fail_loudly_on_dual_check`: persisted halt marker filename
# written by the kernel's runtime_cli_observations module when the
# local-closure dual-collector detects a primary-vs-axcheck disagreement.
# Sticky across kernel rebuilds and supervisor restarts; only operator
# deletion clears it. Bridge refuses to dispatch new bursts while
# present.
CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME = "checker_disagreement_halt.json"
# Per fail-loudly policy: every system_feedback emission pauses the run.
# Distinct marker filename from the checker-disagreement marker so an
# operator inspecting `<runtime_root>/` can tell the two halt causes
# apart at a glance. Same sticky-across-restart semantics; only
# operator deletion clears it. Bridge refuses to dispatch new bursts
# while present.
SYSTEM_FEEDBACK_HALT_MARKER_FILENAME = "system_feedback_halt.json"
# How long a minted token stays in the on-disk registry. Bursts are
# bounded by `policy.timing.burst_timeout_seconds` (typically a few
# hours); 12h gives generous headroom plus avoids permanent growth of
# the file across long runs. The bridge subprocess is short-lived so
# we don't keep an in-memory expiry timer; instead we GC expired
# entries on every write.
_BURST_TOKEN_TTL_SECONDS = 12 * 60 * 60
# Supervisor-lifetime tokens are minted once per `trellis.sh run`
# (kind="supervisor") and live as long as the supervisor process —
# routinely longer than the 12h burst TTL. Without a longer TTL the
# supervisor's own check.py invocations
# (prepare_compiled_support, lean_compile_node, ...) fail with
# `auth_required` exactly 12h into a long run because the bridge's
# next dispatch GCs the supervisor's still-load-bearing token. Use a
# 30-day window so a single supervisor lifetime is never bounded by
# this gate; the supervisor itself shuts down well within 30 days.
_SUPERVISOR_TOKEN_TTL_SECONDS = 30 * 24 * 60 * 60


def _checker_state_dir(runtime_root: Path) -> Path:
    return runtime_root / _CHECKER_STATE_SUBDIR


def _burst_tokens_path(runtime_root: Path) -> Path:
    return _checker_state_dir(runtime_root) / _BURST_TOKENS_FILENAME


def _burst_dispatch_log_path(runtime_root: Path) -> Path:
    return _checker_state_dir(runtime_root) / _BURST_DISPATCH_LOG_FILENAME


def checker_disagreement_halt_marker_path(runtime_root: Path) -> Path:
    """Path of the dual-collector halt marker. The marker's existence
    pins the bridge: every `handle_bridge_request` invocation refuses
    to dispatch a new burst until an operator deletes the file.
    """
    return Path(runtime_root) / CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME


def checker_disagreement_halt_marker_present(runtime_root: Path) -> bool:
    return checker_disagreement_halt_marker_path(runtime_root).exists()


def system_feedback_halt_marker_path(runtime_root: Path) -> Path:
    """Path of the system-feedback halt marker. Mirrors
    `checker_disagreement_halt_marker_path` at a distinct filename so the
    two halt causes don't collide. Per fail-loudly policy: every
    system_feedback emission pauses the run.
    """
    return Path(runtime_root) / SYSTEM_FEEDBACK_HALT_MARKER_FILENAME


def system_feedback_halt_marker_present(runtime_root: Path) -> bool:
    return system_feedback_halt_marker_path(runtime_root).exists()


def any_halt_marker_present(runtime_root: Path) -> bool:
    return checker_disagreement_halt_marker_present(runtime_root) or (
        system_feedback_halt_marker_present(runtime_root)
    )


def _load_burst_tokens_file(path: Path) -> Dict[str, Any]:
    """Return the current `burst-tokens.json` payload as a dict.

    Tolerant of all I/O and JSON errors: any failure returns the empty
    schema so a transient mid-rename read by the server simply sees the
    previous file content (atomic rename guarantees we never observe a
    torn write) and our own writer always overwrites with a complete
    payload.
    """
    try:
        with open(path, "rb") as fh:
            data = json.loads(fh.read().decode("utf-8") or "{}")
    except (FileNotFoundError, OSError, json.JSONDecodeError, UnicodeDecodeError):
        return {"tokens": [], "entries": []}
    if not isinstance(data, dict):
        return {"tokens": [], "entries": []}
    tokens = data.get("tokens", [])
    entries = data.get("entries", [])
    if not isinstance(tokens, list):
        tokens = []
    if not isinstance(entries, list):
        entries = []
    return {"tokens": list(tokens), "entries": list(entries)}


def _atomic_write_burst_tokens(path: Path, payload: Mapping[str, Any]) -> None:
    """Atomically (`os.replace`) write the burst-tokens JSON file.

    Mode 0o600 on both the temp file and the final inode so the bursts'
    own uid (post-Phase-4 the supervisor user; pre-Phase-4 the burst user) sees
    EACCES if it tries to read directly. The runtime root is NOT
    bind-mounted into the burst's bwrap regardless, but the mode tightens
    the host-side surface in case a future caller widens the bind set.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    text = json.dumps(payload, separators=(",", ":"), ensure_ascii=False)
    fd, tmp_name = tempfile.mkstemp(
        prefix=f".{_BURST_TOKENS_FILENAME}.", dir=str(path.parent)
    )
    try:
        os.write(fd, text.encode("utf-8"))
        os.fsync(fd)
    finally:
        os.close(fd)
    try:
        os.chmod(tmp_name, 0o600)
    except OSError:
        pass
    os.replace(tmp_name, path)


def _register_burst_token(
    runtime_root: Path,
    *,
    token: str,
    burst_id: str,
    kind: str,
    request_id: int,
    cycle: int,
) -> None:
    """Insert ``token`` into the on-disk burst-tokens registry.

    GCs entries whose ``ts`` is older than ``_BURST_TOKEN_TTL_SECONDS``
    so the file does not grow unbounded across a long run. Idempotent
    with respect to a token already present in the file (no duplicate
    insertion).
    """
    path = _burst_tokens_path(runtime_root)
    current = _load_burst_tokens_file(path)
    now = time.time()
    cutoff = now - _BURST_TOKEN_TTL_SECONDS
    supervisor_cutoff = now - _SUPERVISOR_TOKEN_TTL_SECONDS
    fresh_entries: List[Dict[str, Any]] = []
    fresh_tokens: List[str] = []
    seen: set[str] = set()
    for raw_entry in current.get("entries", []):
        if not isinstance(raw_entry, dict):
            continue
        entry_token = raw_entry.get("token")
        entry_ts = raw_entry.get("ts")
        if not isinstance(entry_token, str) or not entry_token.strip():
            continue
        entry_cutoff = supervisor_cutoff if raw_entry.get("kind") == "supervisor" else cutoff
        if not isinstance(entry_ts, (int, float)) or entry_ts < entry_cutoff:
            continue
        if entry_token in seen:
            continue
        seen.add(entry_token)
        fresh_entries.append(dict(raw_entry))
        fresh_tokens.append(entry_token)
    if token not in seen:
        fresh_entries.append(
            {
                "token": token,
                "ts": now,
                "burst_id": burst_id,
                "kind": kind,
                "request_id": request_id,
                "cycle": cycle,
            }
        )
        fresh_tokens.append(token)
    _atomic_write_burst_tokens(
        path,
        {"tokens": fresh_tokens, "entries": fresh_entries},
    )


def _append_burst_dispatch_log(
    runtime_root: Path,
    *,
    burst_id: str,
    kind: str,
    request_id: int,
    cycle: int,
    bridge_pid: int,
    extra: Optional[Mapping[str, Any]] = None,
) -> None:
    """Phase 3 (bwrap-only migration): append-only per-dispatch record.

    Lives at ``<runtime>/checker-state/burst-dispatch.jsonl``; line per
    burst, never rotated by the bridge itself (operator-managed).
    Best-effort: any I/O failure is swallowed because attribution is
    a forensic aid, not a correctness gate. Schema is intentionally
    additive — old readers tolerate new fields.
    """
    path = _burst_dispatch_log_path(runtime_root)
    record: Dict[str, Any] = {
        "ts_ns": time.time_ns(),
        "burst_id": burst_id,
        "kind": kind,
        "request_id": request_id,
        "cycle": cycle,
        "bridge_pid": int(bridge_pid),
    }
    if extra:
        for key, value in extra.items():
            if key not in record:
                record[key] = value
    line = json.dumps(record, separators=(",", ":"), ensure_ascii=False) + "\n"
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        with open(path, "ab", buffering=0) as fh:
            fh.write(line.encode("utf-8"))
    except OSError:
        # Append-only forensic log; never blocks dispatch.
        pass


def _mint_burst_token() -> str:
    """Return a fresh URL-safe burst token. 16-byte entropy → 22 chars."""
    return secrets.token_urlsafe(16)


def _burst_id_for_request(request: Mapping[str, Any]) -> str:
    """Compose a stable burst id from request kind + request_id + cycle."""
    kind = _request_kind(request) or "unknown"
    rid = _request_id(request)
    cycle = _request_cycle(request)
    return f"{kind}-c{cycle}-r{rid}"


def _bridge_state_dir(repo_path: Path, runtime_root: Path) -> Path:
    return repo_path / ".trellis" / "runtime" / runtime_root.name


def _bridge_private_state_dir(repo_path: Path, runtime_root: Path) -> Path:
    """Supervisor-only sibling of the staging dir.

    Holds bridge artifacts that the supervisor must trust on read-back —
    most importantly `.acceptance.json`, which the SIGHUP-recovery path
    loads as the normalization baseline (audit followup #2 added that
    read site at `_finalize_recovered_worker_response`). `staging/` is
    in `_repo_writable_paths` for every burst role (sandbox.py:181-186)
    so any worker could rewrite `staging/<...>.acceptance.json` between
    writing `.done` and a supervisor restart, poisoning the recovered
    baseline. This directory is intentionally NOT added to the writable
    allowlist; the worker still gets read access via the repo-wide
    `--ro-bind` (sandbox.py:136), which is sufficient for the worker's
    `--context-json {{acceptance_context_path}}` self-check.
    """
    return repo_path / ".trellis" / "runtime" / runtime_root.name / "private"


def _ensure_project_runtime_support(config: Config) -> None:
    write_scripts(config.repo_path, config.state_dir)


def _bridge_dry_run_enabled() -> bool:
    import os

    return os.environ.get("TRELLIS_TRELLIS_BRIDGE_DRY_RUN", "").strip().lower() in {
        "1",
        "true",
        "yes",
    }


def _save_bridge_json(runtime_root: Path, name: str, payload: Any) -> None:
    path = _bridge_dir(runtime_root) / name
    save_json(path, payload)


def _save_bridge_text(runtime_root: Path, name: str, text: str) -> Path:
    path = _bridge_dir(runtime_root) / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    return path


def _load_bridge_json(runtime_root: Path, name: str) -> Any:
    path = _bridge_dir(runtime_root) / name
    if not path.exists():
        return None
    return json.loads(path.read_text(encoding="utf-8"))


def _request_kind(request: Mapping[str, Any]) -> str:
    return str(request.get("kind", "") or "").strip().lower()


def _request_id(request: Mapping[str, Any]) -> int:
    return int(request.get("id", 0) or 0)


def _request_cycle(request: Mapping[str, Any]) -> int:
    return int(request.get("cycle", 0) or 0)


def _normalize_phase_name(raw_phase: Any) -> str:
    phase = str(raw_phase or "").strip().lower()
    return {
        "theoremstating": "theorem_stating",
        "proofformalization": "proof_formalization",
        "humangate": "human_gate",
    }.get(phase, phase)


def _phase_name(request: Mapping[str, Any]) -> str:
    return _normalize_phase_name(request.get("phase", ""))


def _sorted_strs(values: Iterable[Any]) -> List[str]:
    return sorted(str(value).strip() for value in values if str(value).strip())


def _artifact_name(kind: str, request_id: int, suffix: str) -> str:
    safe_suffix = suffix.replace("/", "_")
    return f"trellis_{kind}_{request_id}_{safe_suffix}.json"


def _session_scope(
    request: Mapping[str, Any],
    provider: ProviderConfig,
    burst_role: str,
    *,
    lane_id: str = "",
) -> str:
    phase = _phase_name(request) or "unknown_phase"
    provider_name = str(provider.provider or "").strip().lower() or "unknown_provider"
    model_name = str(provider.model or "auto").strip() or "auto"
    effort_name = str(provider.effort or "").strip().lower() or "default"
    if burst_role == "worker":
        return (
            f"{phase}:{burst_role}:"
            f"{provider_name}:{model_name}:{effort_name}"
        )
    request_kind = _request_kind(request) or "unknown_kind"
    if burst_role == "reviewer" and request_kind == "review":
        return ":".join(
            [phase, burst_role, request_kind, provider_name, model_name, effort_name]
        )
    request_id = _request_id(request)
    effective_lane_id = (
        str(lane_id or "").strip()
        or str(request.get("lane_id", "") or "").strip()
    )
    parts = [
        phase,
        burst_role,
        request_kind,
        str(request_id),
    ]
    if effective_lane_id:
        parts.append(effective_lane_id)
    parts.extend([provider_name, model_name, effort_name])
    return ":".join(parts)


def _workflow_approved_axioms_path(config: Config) -> Optional[Path]:
    workflow = getattr(config, "workflow", None)
    raw = getattr(workflow, "approved_axioms_path", None) if workflow is not None else None
    if raw is None:
        return None
    return Path(raw)


def _tablet_snapshot(repo_path: Path) -> Dict[str, str]:
    tablet_dir = repo_path / "Tablet"
    snapshot: Dict[str, str] = {}
    if not tablet_dir.exists():
        return snapshot
    for path in sorted(tablet_dir.iterdir()):
        if path.is_file():
            snapshot[path.name] = hashlib.sha256(path.read_bytes()).hexdigest()
    return snapshot


def _update_bool(value: bool) -> Dict[str, Any]:
    return {"Set": value}


def _policy(config: Config) -> Policy:
    return PolicyManager(config).current()


def _theorem_initial_dag_size_guidance(policy: Policy) -> str:
    return (
        f"{policy.prompt_notes.initial_theorem_dag_size_min}"
        f"-{policy.prompt_notes.initial_theorem_dag_size_max}"
    )


def _prepare_worker_support_files(
    *,
    repo_path: Path,
    request: Mapping[str, Any],
) -> Dict[str, Any]:
    fresh_context = bool(request.get("fresh_context", False))
    scratch = ensure_worker_scratch_workspace(repo_path, reset=fresh_context)
    invalid_root = last_invalid_dir(repo_path)
    invalid_metadata = last_invalid_metadata_path(repo_path)
    return {
        "scratch_workspace_path": scratch["workspace_path"],
        "scratch_readme_path": scratch["readme_path"],
        "scratch_notes_path": scratch["notes_path"],
        "scratch_example_path": scratch["example_path"],
        "scratch_workspace_status_text": scratch["status_text"],
        "last_invalid_path": invalid_root if invalid_root.is_dir() else None,
        "last_invalid_metadata_path": invalid_metadata if invalid_metadata.is_file() else None,
    }


def _provider_from_agent(agent: Any) -> ProviderConfig:
    return ProviderConfig(
        provider=str(getattr(agent, "provider", "") or ""),
        model=(str(getattr(agent, "model", "") or "").strip() or None),
        effort=(str(getattr(agent, "effort", "") or "").strip() or None),
        extra_args=list(getattr(agent, "extra_args", []) or []),
        fallback_models=list(getattr(agent, "fallback_models", []) or []),
    )


def _provider_from_lane_binding(binding: Mapping[str, Any]) -> ProviderConfig:
    return ProviderConfig(
        provider=str(binding.get("provider", "") or ""),
        model=(str(binding.get("model", "") or "").strip() or None),
        effort=(str(binding.get("effort", "") or "").strip() or None),
        extra_args=[str(item) for item in binding.get("extra_args", []) or [] if str(item).strip()],
        fallback_models=[
            str(item) for item in binding.get("fallback_models", []) or [] if str(item).strip()
        ],
    )


def _provider_from_request_binding(
    request: Mapping[str, Any],
    *,
    field_name: str,
) -> ProviderConfig:
    raw_binding = request.get(field_name)
    if not isinstance(raw_binding, Mapping):
        raise BridgeError(f"{field_name} must be an object")
    provider = _provider_from_lane_binding(raw_binding)
    if not provider.provider.strip():
        raise BridgeError(f"{field_name} is missing provider")
    return provider


def _verification_lane_bindings(
    request: Mapping[str, Any],
    *,
    kind: str,
) -> List[Mapping[str, Any]]:
    binding_key = {
        "paper": "paper_verify_lane_bindings",
        "corr": "corr_verify_lane_bindings",
        "sound": "sound_verify_lane_bindings",
    }.get(kind)
    if binding_key is None:
        raise BridgeError(f"unsupported verification kind: {kind}")
    raw_bindings = request.get(binding_key, [])
    if not isinstance(raw_bindings, list):
        raise BridgeError(f"{binding_key} must be a list")
    expected_lanes = _sorted_strs(request.get("verify_lanes", []))
    binding_by_lane: Dict[str, Mapping[str, Any]] = {}
    for raw in raw_bindings:
        if not isinstance(raw, Mapping):
            raise BridgeError(f"{binding_key} entries must be objects")
        lane_id = str(raw.get("lane_id", "") or "").strip()
        if not lane_id:
            raise BridgeError(f"{binding_key} entry is missing lane_id")
        if lane_id in binding_by_lane:
            raise BridgeError(f"{binding_key} contains duplicate lane binding for {lane_id}")
        binding_by_lane[lane_id] = raw
    if sorted(binding_by_lane.keys()) != expected_lanes:
        raise BridgeError(f"{binding_key} must cover exactly verify_lanes")
    return [binding_by_lane[lane_id] for lane_id in expected_lanes]


_PERSISTENT_BURST_HOME_NAMES = frozenset({"worker", "reviewer"})


def _burst_home_key(burst_role: str) -> str:
    """Stable fake-home key per burst role.

    Returns ``worker`` for worker bursts and ``reviewer`` for reviewer +
    verifier bursts (all verifier dispatches share ``burst_role="reviewer"``).
    The two roles get separate fake-homes so codex's state DB doesn't
    cross-contaminate between worker and reviewer thread namespaces.
    """
    role = str(burst_role or "").strip().lower()
    if role == "worker":
        return "worker"
    return "reviewer"


def _runtime_session_namespace(runtime_root: Path) -> str:
    raw = runtime_root.parent.name or runtime_root.name or "runtime"
    cleaned = "".join(ch if ch.isalnum() or ch in {"-", "_"} else "-" for ch in raw.strip())
    cleaned = cleaned.strip("-_") or "runtime"
    return cleaned[:48]


def _single_request_common(
    *,
    config: Config,
    runtime_root: Path,
    request: Mapping[str, Any],
    provider: ProviderConfig,
    lane: AgentLane,
    kind_label: str,
    burst_role: str,
    prompt: str,
    artifact: Optional[ArtifactSpec],
) -> SingleAgentRequest:
    request_id = f"{_request_kind(request)}-{_request_id(request)}-{kind_label}"
    session_name = (
        f"trellis-{_runtime_session_namespace(runtime_root)}-"
        f"{_request_kind(request)}-{_request_id(request)}-{kind_label}"
    )
    # Stable per-role HOME (worker / reviewer) under
    # `<runtime>/burst-homes/<role>/` seeded with hard-links of the
    # supervisor's `~/.codex`, `~/.claude`, `~/.gemini`. The bwrap then
    # binds that dir as the burst's `$HOME`. Sharing the home across
    # bursts of the same role keeps codex's state DB → absolute rollout
    # path round-trip valid for `codex exec resume` (a per-burst home
    # would invalidate the stored path the moment cleanup runs). Trellis
    # dispatches bursts serially so sharing per role is race-free.
    #
    # Dry-run / preview skips the seed: previews never launch a burst.
    home_key = _burst_home_key(burst_role)
    effective_burst_home: Optional[Path] = None
    if not _bridge_dry_run_enabled():
        effective_burst_home = seed_burst_home(
            runtime_root, home_key, persistent=True
        )
    return SingleAgentRequest(
        request_id=request_id,
        cycle=_request_cycle(request),
        kind=_request_kind(request),
        burst_role=burst_role,
        provider=provider,
        prompt=prompt,
        work_dir=config.repo_path,
        state_dir=_bridge_state_dir(config.repo_path, runtime_root),
        # session_name becomes the tmux session for this burst. For multi-lane
        # verifier panels (corr/paper/sound with v1+v2), the SAME request
        # produces several SingleAgentRequests (one per lane). Without
        # `kind_label` in the session_name, every lane's burst would use an
        # identical tmux session and `tmux new-session` would kill the peer
        # lane. Always include `kind_label` to guarantee per-lane isolation.
        session_name=session_name,
        session_scope=_session_scope(
            request,
            provider,
            burst_role,
            lane_id=lane.node_name if burst_role != "worker" else "",
        ),
        lane=lane,
        timeout_seconds=float(_policy(config).timing.burst_timeout_seconds),
        startup_timeout_seconds=float(config.startup_timeout_seconds),
        burst_home=effective_burst_home,
        log_dir=_bridge_state_dir(config.repo_path, runtime_root) / "logs",
        fresh=bool(request.get("fresh_context", False)),
        artifact=artifact,
        artifact_prefix=(artifact.canonical_name[:-5] if artifact is not None else None),
        sandbox=config.sandbox,
    )


def _dry_run_single(
    *,
    runtime_root: Path,
    request: Mapping[str, Any],
    single: SingleAgentRequest,
    prompt: str,
) -> Dict[str, Any]:
    request_id = _request_id(request)
    kind = _request_kind(request)
    prompt_path = _save_bridge_text(
        runtime_root,
        f"preview_{kind}_{request_id}.prompt.txt",
        prompt,
    )
    payload = {
        "dry_run": True,
        "kind": kind,
        "request_id": request_id,
        "cycle": _request_cycle(request),
        "prompt_path": str(prompt_path),
        "single_request": single.to_dict(),
    }
    _save_bridge_json(runtime_root, f"preview_{kind}_{request_id}.json", payload)
    return payload


def _preview_label(raw: str) -> str:
    label = str(raw or "").strip()
    safe = "".join(ch if ch.isalnum() or ch in {"-", "_"} else "_" for ch in label)
    return safe or "lane"


def _dry_run_panel(
    *,
    runtime_root: Path,
    request: Mapping[str, Any],
    members: Sequence[SingleAgentRequest],
) -> Dict[str, Any]:
    request_id = _request_id(request)
    kind = _request_kind(request)
    prompt_paths: List[str] = []
    single_requests: List[Dict[str, Any]] = []
    for index, member in enumerate(members):
        label = _preview_label(member.lane.node_name or member.lane.kind or str(index))
        prompt_path = _save_bridge_text(
            runtime_root,
            f"preview_{kind}_{request_id}_{index}_{label}.prompt.txt",
            member.prompt,
        )
        prompt_paths.append(str(prompt_path))
        single_requests.append(member.to_dict())
    payload = {
        "dry_run": True,
        "kind": kind,
        "request_id": request_id,
        "cycle": _request_cycle(request),
        "prompt_paths": prompt_paths,
        "single_requests": single_requests,
    }
    if prompt_paths:
        payload["prompt_path"] = prompt_paths[0]
    if single_requests:
        payload["single_request"] = single_requests[0]
    _save_bridge_json(runtime_root, f"preview_{kind}_{request_id}.json", payload)
    return payload


def _load_raw_response_json(response: Any) -> Dict[str, Any]:
    raw_path = getattr(response, "raw_path", None)
    if raw_path is None:
        raise BridgeError("bridge response is missing raw_path")
    path = Path(raw_path)
    if not path.exists():
        raise BridgeError(f"bridge raw artifact missing: {path}")
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:
        raise BridgeError(f"invalid bridge raw artifact JSON: {exc}") from exc
    if not isinstance(data, dict):
        raise BridgeError("bridge raw artifact must be a JSON object")
    return data


@dataclass(frozen=True)
class _RecoveredArtifact:
    """Bug X principled fix Phase 4: result of `_recover_done_artifact`.

    `payload` is the parsed `.raw.json` if present and parseable.
    `parse_error` is set if a `.done` exists but the artifact couldn't be
    consumed — caller should treat as transport-flavored Malformed.
    Both fields None means there's nothing to recover (no `.done` present).
    """
    payload: Optional[Dict[str, Any]] = None
    parse_error: Optional[str] = None


def _recover_done_artifact(
    *,
    done_path: Path,
    raw_path: Path,
) -> _RecoveredArtifact:
    """Bug X principled fix Phase 4: SIGHUP recovery helper.

    When the supervisor restarts mid-burst, the kernel reissues the same
    in-flight request from persisted state. If the prior worker had
    already written `.done` + `.raw.json` before the restart killed it,
    we can consume that result instead of relaunching the worker — saving
    the cost of a fresh agent invocation and avoiding the duplicate-work
    that breaks Bug X's `before_snapshot` invariant (the new worker would
    see a baseline mutated by the prior worker's writes).

    Returns:
      - `_RecoveredArtifact(payload=..., parse_error=None)` if `.done`
        exists AND `.raw.json` is parseable. Caller should skip the burst
        and use this payload as if `execute_agent_request` had returned
        ok=True with payload=this dict.
      - `_RecoveredArtifact(payload=None, parse_error=detail)` if `.done`
        exists but `.raw.json` is missing or unparseable. Caller should
        emit a Transport-flavored Malformed `WrapperResponse`.
      - `_RecoveredArtifact()` (both None) if `.done` doesn't exist —
        caller should run the burst as normal.
    """
    if not done_path.exists():
        return _RecoveredArtifact()
    if not raw_path.exists():
        return _RecoveredArtifact(
            parse_error=(
                f"recovery: done marker {done_path.name} present but raw "
                f"artifact {raw_path.name} is missing"
            ),
        )
    try:
        data = json.loads(raw_path.read_text(encoding="utf-8"))
    except Exception as exc:
        return _RecoveredArtifact(
            parse_error=(
                f"recovery: done marker {done_path.name} present but raw "
                f"artifact unparseable: {exc}"
            ),
        )
    if not isinstance(data, dict):
        return _RecoveredArtifact(
            parse_error=(
                f"recovery: done marker {done_path.name} present but raw "
                f"artifact is not a JSON object"
            ),
        )
    return _RecoveredArtifact(payload=data)


def _build_malformed_response(
    *,
    kind: str,
    request: Mapping[str, Any],
) -> Dict[str, Any]:
    result = run_kernel_cli(
        {
            "action": "build_malformed_response",
            "kind": kind,
            "request_id": _request_id(request),
            "cycle": _request_cycle(request),
        }
    )
    if result.get("status") != "build_malformed_response_ok":
        raise BridgeError(
            f"unexpected build_malformed_response status: {result.get('status')!r}"
        )
    output = result.get("output")
    if not isinstance(output, dict):
        raise BridgeError("build_malformed_response response is missing output")
    return output


def _panel_members_for_kernel(
    *,
    kind_label: str,
    raw_panel: Any,
    request_members: Mapping[str, SingleAgentRequest],
) -> List[Dict[str, Any]]:
    members: List[Dict[str, Any]] = []
    for member in raw_panel.member_responses:
        original = request_members.get(member.request_id)
        if original is None:
            raise BridgeError(f"unexpected {kind_label} member response {member.request_id}")
        members.append(
            {
                "lane_id": original.lane.node_name,
                "ok": bool(member.ok),
                "payload": dict(member.payload) if isinstance(member.payload, dict) else None,
                "error": str(member.error or ""),
            }
        )
    return members


def _run_kernel_normalize(action: str, **input_kwargs: Any) -> Dict[str, Any]:
    """Invoke a kernel CLI `action` whose response shape is the standard
    `{"status": f"{action}_ok", "output": {"response": <dict>}}` envelope.

    Each `_normalize_*_via_kernel` / `_hydrate_*_via_kernel` wrapper below
    funnels through this helper: it builds the request, normalizes
    `KernelCliError` to `BridgeError`, validates the status string, and
    extracts the nested `response` dict.
    """
    try:
        kernel_response = run_kernel_cli(
            {"action": action, "input": input_kwargs}
        )
    except KernelCliError as exc:
        raise BridgeError(f"kernel CLI failed: {exc}") from exc
    expected_status = f"{action}_ok"
    if kernel_response.get("status") != expected_status:
        raise BridgeError(
            f"unexpected kernel {action} response status: {kernel_response.get('status')!r}"
        )
    output = kernel_response.get("output")
    if not isinstance(output, dict):
        raise BridgeError(f"kernel {action} response is missing output")
    response = output.get("response")
    if not isinstance(response, dict):
        raise BridgeError(f"kernel {action} response is missing response")
    return response


def _normalize_corr_via_kernel(
    *,
    request_id: int,
    cycle: int,
    verify_lanes: Iterable[str],
    verify_nodes: Iterable[str],
    verify_targets: Iterable[str],
    raw_panel: Any,
    request_members: Mapping[str, SingleAgentRequest],
) -> Dict[str, Any]:
    return _run_kernel_normalize(
        "normalize_corr",
        request_id=request_id,
        cycle=cycle,
        verify_lanes=_sorted_strs(verify_lanes),
        verify_nodes=_sorted_strs(verify_nodes),
        verify_targets=_sorted_strs(verify_targets),
        members=_panel_members_for_kernel(
            kind_label="correspondence",
            raw_panel=raw_panel,
            request_members=request_members,
        ),
    )


def _normalize_paper_via_kernel(
    *,
    request_id: int,
    cycle: int,
    verify_lanes: Iterable[str],
    verify_targets: Iterable[str],
    verify_nodes: Iterable[str] = (),
    verify_deviations: Iterable[str] = (),
    raw_panel: Any,
    request_members: Mapping[str, SingleAgentRequest],
) -> Dict[str, Any]:
    """
    Normalize a Paper response via the kernel CLI. Handles both the
    target-package and per-node scenarios; pass `verify_nodes` for the
    per-node lane (kernel emits `node_lane_updates` populated by lane ×
    node × SubstantivenessStatus, including `NotDoneYet`).
    """
    return _run_kernel_normalize(
        "normalize_paper",
        request_id=request_id,
        cycle=cycle,
        verify_lanes=_sorted_strs(verify_lanes),
        verify_targets=_sorted_strs(verify_targets),
        verify_nodes=_sorted_strs(verify_nodes),
        verify_deviations=_sorted_strs(verify_deviations),
        members=_panel_members_for_kernel(
            kind_label="paper-faithfulness",
            raw_panel=raw_panel,
            request_members=request_members,
        ),
    )


def _normalize_sound_via_kernel(
    *,
    request_id: int,
    cycle: int,
    verify_lanes: Iterable[str],
    verify_nodes: Iterable[str],
    raw_panel: Any,
    request_members: Mapping[str, SingleAgentRequest],
) -> Dict[str, Any]:
    return _run_kernel_normalize(
        "normalize_sound",
        request_id=request_id,
        cycle=cycle,
        verify_lanes=_sorted_strs(verify_lanes),
        verify_nodes=_sorted_strs(verify_nodes),
        members=_panel_members_for_kernel(
            kind_label="soundness",
            raw_panel=raw_panel,
            request_members=request_members,
        ),
    )

def _worker_provider_for_request(request: Mapping[str, Any]) -> ProviderConfig:
    return _provider_from_request_binding(request, field_name="worker_binding")


def _worker_burst_role(request: Mapping[str, Any]) -> str:
    return "worker"


def _hydrate_worker_response_via_kernel(
    *,
    repo_path: Path,
    acceptance_context: Mapping[str, Any],
    response: Mapping[str, Any],
) -> Dict[str, Any]:
    return _run_kernel_normalize(
        "hydrate_worker_response",
        repo_path=str(repo_path),
        configured_targets=list(acceptance_context.get("configured_targets", [])),
        current_target_claims=dict(acceptance_context.get("current_target_claims", {})),
        approved_paper_fingerprints=dict(
            acceptance_context.get("current_paper_approved_fingerprints", {})
        ),
        response=dict(response),
    )


def _downgrade_worker_response_to_invalid(
    response: Mapping[str, Any],
) -> Dict[str, Any]:
    downgraded = dict(response)
    downgraded["outcome"] = "Invalid"
    return downgraded


def _checker_comparable_result(result: Mapping[str, Any]) -> Dict[str, Any]:
    response = result.get("response")
    comparable_response = dict(response) if isinstance(response, Mapping) else None
    return {
        "ok": bool(result.get("ok", False)),
        "final_outcome": str(result.get("final_outcome", "") or "").strip().lower(),
        "errors": [str(item) for item in result.get("errors", []) or []],
        "validation_errors": [
            str(item) for item in result.get("validation_errors", []) or []
        ],
        "contract_errors": [
            str(item) for item in result.get("contract_errors", []) or []
        ],
        "response": comparable_response,
    }


def _checker_mismatch_detail(
    worker_trace: Optional[Mapping[str, Any]],
    supervisor_result: Mapping[str, Any],
) -> str:
    # Authoritative-checker-only policy: the supervisor-side re-run of
    # the acceptance checker is authoritative. We only flag a mismatch
    # when the worker claimed their in-burst check passed but the
    # supervisor's re-run rejects — that's a cheat signal (agent ran the
    # check against something different than what it ultimately
    # submitted, then claimed OK).
    #
    # We deliberately do NOT flag:
    #   - supervisor ok + worker ok with different traces (e.g. Gemini's
    #     placeholder-then-rewrite pattern: the agent writes a stub
    #     raw.json, runs the check, then rewrites raw.json with the
    #     real content before writing done_file — supervisor re-run
    #     sees the real content, both pass, traces differ, but the
    #     state transition is correct because the tablet state comes
    #     from the filesystem which the supervisor observes
    #     authoritatively);
    #   - supervisor ok + worker missing/fail (no false-positive from
    #     the agent);
    #   - supervisor fail + worker fail (both agreed it's bad — this is
    #     an ordinary Invalid response, handled downstream without
    #     needing a cheat marker).
    if bool(supervisor_result.get("ok", False)):
        return ""
    if not isinstance(worker_trace, Mapping):
        return ""
    worker_result = worker_trace.get("result")
    if not isinstance(worker_result, Mapping):
        return ""
    if not bool(worker_result.get("ok", False)):
        return ""
    # Cheat signal: worker claimed OK, supervisor says not OK. Keep this
    # reason prompt-sized; the full comparable traces can contain large
    # fingerprint snapshots and are already available in bridge artifacts.
    worker_summary = _compact_checker_result_summary(worker_result)
    supervisor_summary = _compact_checker_result_summary(supervisor_result)
    summary = _truncate_checker_mismatch_text(
        json.dumps(
            {
                "worker": worker_summary,
                "supervisor": supervisor_summary,
            },
            sort_keys=True,
        ),
        _CHECKER_MISMATCH_DETAIL_CHAR_LIMIT,
    )
    return (
        f"{_CHECKER_MISMATCH_PREFIX} "
        "worker-side acceptance reported success, but the supervisor's "
        "authoritative check rejected the submitted result. "
        f"summary={summary}"
    )


def _truncate_checker_mismatch_text(text: str, limit: int) -> str:
    if len(text) <= limit:
        return text
    return (
        text[:limit]
        + "... [truncated; inspect the prompt-listed checker artifacts for full detail]"
    )


def _compact_checker_messages(value: Any) -> List[str]:
    if isinstance(value, Sequence) and not isinstance(value, (str, bytes, bytearray)):
        items = list(value)
    elif value:
        items = [value]
    else:
        items = []
    return [
        _truncate_checker_mismatch_text(str(item), _CHECKER_MISMATCH_ITEM_CHAR_LIMIT)
        for item in items[:3]
    ]


def _compact_checker_result_summary(result: Mapping[str, Any]) -> Dict[str, Any]:
    response = result.get("response")
    response_summary = ""
    response_outcome = ""
    if isinstance(response, Mapping):
        response_summary = _truncate_checker_mismatch_text(
            str(response.get("summary", "") or ""),
            _CHECKER_MISMATCH_ITEM_CHAR_LIMIT,
        )
        response_outcome = str(response.get("outcome", "") or "")
    return {
        "ok": bool(result.get("ok", False)),
        "final_outcome": str(result.get("final_outcome", "") or ""),
        "response_outcome": response_outcome,
        "response_summary": response_summary,
        "errors": _compact_checker_messages(result.get("errors", [])),
        "validation_errors": _compact_checker_messages(
            result.get("validation_errors", [])
        ),
        "contract_errors": _compact_checker_messages(result.get("contract_errors", [])),
    }


def _invalidate_for_checker_mismatch(
    normalized_result: Mapping[str, Any],
    *,
    detail: str,
) -> Dict[str, Any]:
    response = normalized_result.get("response")
    rewritten_response = dict(response) if isinstance(response, Mapping) else {}
    rewritten_response["outcome"] = "Invalid"
    reasons = [
        str(item)
        for item in rewritten_response.get("deterministic_rejection_reasons", []) or []
    ]
    reasons.append(detail)
    rewritten_response["deterministic_rejection_reasons"] = reasons
    validation_errors = [
        str(item) for item in normalized_result.get("validation_errors", []) or []
    ]
    validation_errors.append(detail)
    errors = [str(item) for item in normalized_result.get("errors", []) or []]
    errors.append(detail)
    return {
        "ok": False,
        "errors": errors,
        "data": normalized_result.get("data"),
        "response": rewritten_response,
        "validation_step_results": list(
            normalized_result.get("validation_step_results", []) or []
        ),
        "contract_errors": list(normalized_result.get("contract_errors", []) or []),
        "validation_errors": validation_errors,
        "final_outcome": "invalid",
    }


def _save_malformed_worker_result(
    *,
    runtime_root: Path,
    request: Mapping[str, Any],
    raw_payload: Mapping[str, Any] | None,
    acceptance_context_path: Path | None,
    authoritative_repo_path: Path | None,
    errors: Sequence[str],
    validation_errors: Sequence[str] | None = None,
    contract_errors: Sequence[str] | None = None,
    transport_failure: bool = False,
) -> Dict[str, Any]:
    """Persist a malformed worker result.

    Bug X principled fix: `transport_failure=True` flags this as an
    infrastructure failure (agent never produced output, timeout, rate-limit
    retries exhausted, etc.) so the kernel routes the rejection through the
    `RetryOutcomeKind::Transport` path — distinct from a worker that
    actually ran but emitted bad JSON.
    """
    malformed = _build_malformed_response(kind="worker", request=request)
    if transport_failure:
        # Round-trip the kernel's malformed template through the same JSON
        # shape it expects on the way back, then flip the transport flag.
        # Default in the kernel is false, so we only need to set when true.
        malformed = dict(malformed)
        malformed["transport_failure"] = True
    _save_bridge_json(
        runtime_root,
        "latest_worker.json",
        {
            "raw": dict(raw_payload) if isinstance(raw_payload, Mapping) else raw_payload,
            "response": malformed,
            "acceptance_context_path": (
                str(acceptance_context_path) if acceptance_context_path is not None else None
            ),
            "authoritative_repo_path": (
                str(authoritative_repo_path) if authoritative_repo_path is not None else None
            ),
            "errors": [str(item) for item in errors],
            "validation_errors": [str(item) for item in (validation_errors or [])],
            "contract_errors": [str(item) for item in (contract_errors or [])],
            "final_outcome": ("transport_failure" if transport_failure else "malformed"),
        },
    )
    return malformed


def _finalize_recovered_worker_response(
    *,
    runtime_root: Path,
    request: Mapping[str, Any],
    config: Config,
    recovered_payload: Dict[str, Any],
    acceptance_context: Dict[str, Any],
    acceptance_context_path: Path,
    authoritative_repo: Path,
    raw_path: Path,
    done_path: Path,
) -> Dict[str, Any]:
    """Audit followup #2 (Problem A): SIGHUP-recovery normalization path.

    `recovered_payload` was loaded from a `.raw.json` written by a
    pre-restart worker burst. `acceptance_context` is the ORIGINAL saved
    `.acceptance.json` from that same pre-restart burst — NOT a
    rebuilt-against-dirty-disk replacement. This helper runs the same
    post-burst normalization the live path runs (`normalize_*`,
    `record_worker_checker_trace`, mismatch detection, `_save_bridge_json`),
    using the original baseline so unauthorized worker writes are
    correctly identified as candidate changes rather than absorbed into a
    fresh baseline.
    """
    response = SingleAgentResponse(
        request_id=str(_request_id(request)),
        cycle=_request_cycle(request),
        kind="worker",
        burst_role=_worker_burst_role(request),
        ok=True,
        payload=recovered_payload,
        raw_path=raw_path,
        done_path=done_path,
    )
    raw_payload: Dict[str, Any] | None = None
    normalized_result: Dict[str, Any]
    try:
        raw_payload = (
            dict(response.payload)
            if isinstance(response.payload, dict)
            else _load_raw_response_json(response)
        )
        normalized_result = normalize_trellis_worker_result_data(
            raw_payload,
            repo=authoritative_repo,
            acceptance_context=acceptance_context,
        )
        propagate_tablet_back_to_worker(authoritative_repo, config.repo_path)
        record_worker_checker_trace(
            authoritative_repo,
            acceptance_context=acceptance_context,
            result=normalized_result,
            source="supervisor",
        )
        mismatch_detail = _checker_mismatch_detail(
            load_worker_checker_trace(
                config.repo_path,
                request_id=_request_id(request),
            ),
            normalized_result,
        )
        if mismatch_detail:
            normalized_result = _invalidate_for_checker_mismatch(
                normalized_result,
                detail=mismatch_detail,
            )
    except BridgeError as exc:
        normalized_result = {"errors": [str(exc)]}
    normalized = normalized_result.get("response")
    if not isinstance(normalized, dict):
        errors = list(normalized_result.get("errors", []))
        return _save_malformed_worker_result(
            runtime_root=runtime_root,
            request=request,
            raw_payload=raw_payload,
            acceptance_context_path=acceptance_context_path,
            authoritative_repo_path=authoritative_repo,
            errors=errors,
            validation_errors=list(normalized_result.get("validation_errors", [])),
            contract_errors=list(normalized_result.get("contract_errors", [])),
        )
    final_outcome = str(normalized_result.get("final_outcome", "") or "").strip().lower()
    normalized["kind"] = "worker"
    _save_bridge_json(
        runtime_root,
        "latest_worker.json",
        {
            "raw": raw_payload,
            "response": normalized,
            "acceptance_context_path": str(acceptance_context_path),
            "authoritative_repo_path": str(authoritative_repo),
            "errors": [],
            "validation_errors": list(normalized_result.get("validation_errors", [])),
            "contract_errors": list(normalized_result.get("contract_errors", [])),
            "final_outcome": final_outcome,
        },
    )
    return normalized


def _restore_active_worker_base_via_kernel(runtime_root: Path) -> bool:
    """Audit followup #2 (Problem B): ask the kernel CLI to restore the
    worker repo's `Tablet/` to the captured `active_worker_base` snapshot.

    Called from `_handle_worker` BEFORE rebuilding the acceptance context
    when no `.done` artifact exists, so a previous partial worker burst
    (killed mid-write by a SIGHUP / crash) doesn't leave dirty Tablet
    files that would become the new `before_snapshot` baseline.

    Returns True iff the restore actually ran (an in-flight worker
    request existed AND a snapshot was present). Failures bubble up as
    `KernelCliError` — caller treats those as transport-failure so the
    kernel uses the transport budget, since the bridge couldn't even
    establish a clean baseline for the worker to start from.

    Fast-path: when `runtime_root/protocol_state.json` doesn't exist
    (test fixtures, dry-run scaffolding), there's no SupervisorRuntime
    to load and so nothing to restore. Treat as a no-op rather than
    paying the kernel-CLI roundtrip just to get an error back.
    """
    if not (runtime_root / "protocol_state.json").is_file():
        return False
    response = run_kernel_cli(
        {
            "action": "restore_active_worker_base",
            "root": str(runtime_root),
        }
    )
    if response.get("status") != "restore_active_worker_base_ok":
        raise KernelCliError(
            f"unexpected kernel CLI status for restore_active_worker_base: "
            f"{response.get('status')!r}"
        )
    return bool(response.get("restored", False))


def _handle_worker(
    *,
    config: Config,
    runtime_root: Path,
    request: Mapping[str, Any],
) -> Dict[str, Any]:
    canonical_name = _artifact_name("worker", _request_id(request), "result")
    artifact = ArtifactSpec(
        canonical_name=canonical_name,
        kind="trellis-worker-result",
        phase=_phase_name(request),
    )
    staging_dir = _bridge_state_dir(config.repo_path, runtime_root) / "staging"
    staging_dir.mkdir(parents=True, exist_ok=True)
    private_dir = _bridge_private_state_dir(config.repo_path, runtime_root)
    private_dir.mkdir(parents=True, exist_ok=True)
    raw_path = staging_dir / canonical_name.replace(".json", ".raw.json")
    done_path = staging_dir / canonical_name.replace(".json", ".done")
    # Audit followup: `.acceptance.json` lives in the private dir
    # (NOT in the worker-writable staging allowlist) because the
    # SIGHUP-recovery path loads it back as the trusted normalization
    # baseline. See _bridge_private_state_dir.
    acceptance_context_path = private_dir / canonical_name.replace(
        ".json",
        ".acceptance.json",
    )
    authoritative_repo = config.repo_path.resolve()
    # Audit followup #2 (Problem A): SIGHUP recovery ordering. If the
    # prior worker burst already wrote `.done + .raw.json + .acceptance.json`
    # before the supervisor was killed, we MUST consume those artifacts
    # using the originally saved acceptance context — NOT rebuild a fresh
    # one against the current (post-worker-mutation) disk. Rebuilding
    # would let unauthorized worker writes become the new baseline
    # (`before_snapshot`), defeating Bug X's invariant.
    #
    # Detection runs BEFORE any sync / acceptance-context rebuild, so
    # the recovery path is purely read-only with respect to the gate
    # state captured pre-restart. We still sync the supervisor workspace
    # so `authoritative_repo` reflects the worker's writes (the
    # acceptance context was anchored to the *pre-burst* baseline; the
    # worker's writes are the candidate diff to evaluate).
    recovered = _recover_done_artifact(done_path=done_path, raw_path=raw_path)
    if recovered.parse_error is not None:
        # `.done` present but `.raw.json` unconsumable → transport-flavored
        # Malformed (the agent wrote SOMETHING but we can't use it).
        return _save_malformed_worker_result(
            runtime_root=runtime_root,
            request=request,
            raw_payload=None,
            acceptance_context_path=(
                acceptance_context_path if acceptance_context_path.exists() else None
            ),
            authoritative_repo_path=authoritative_repo,
            errors=[recovered.parse_error],
            transport_failure=True,
        )
    if recovered.payload is not None:
        # SIGHUP-recovery happy path: load the originally saved acceptance
        # context, sync (to capture the worker's writes onto the
        # supervisor repo), and skip the rebuild.
        if not acceptance_context_path.exists():
            return _save_malformed_worker_result(
                runtime_root=runtime_root,
                request=request,
                raw_payload=None,
                acceptance_context_path=None,
                authoritative_repo_path=authoritative_repo,
                errors=[
                    "recovery: done marker present but original "
                    f"acceptance context {acceptance_context_path.name} "
                    "is missing — cannot normalize against pre-burst baseline"
                ],
                transport_failure=True,
            )
        try:
            acceptance_context = load_json(acceptance_context_path)
            if not isinstance(acceptance_context, dict):
                raise BridgeError(
                    f"recovery: saved acceptance context "
                    f"{acceptance_context_path.name} is not a JSON object"
                )
            authoritative_sync = sync_supervisor_workspace(config.repo_path)
            authoritative_repo = Path(
                str(authoritative_sync.get("authoritative_repo_path", "") or "")
            ).resolve()
        except Exception as exc:
            return _save_malformed_worker_result(
                runtime_root=runtime_root,
                request=request,
                raw_payload=None,
                acceptance_context_path=acceptance_context_path,
                authoritative_repo_path=authoritative_repo,
                errors=[str(exc)],
                transport_failure=True,
            )
        return _finalize_recovered_worker_response(
            runtime_root=runtime_root,
            request=request,
            config=config,
            recovered_payload=recovered.payload,
            acceptance_context=acceptance_context,
            acceptance_context_path=acceptance_context_path,
            authoritative_repo=authoritative_repo,
            raw_path=raw_path,
            done_path=done_path,
        )
    # No `.done`: this is either the first dispatch of the request OR a
    # restart where the prior worker crashed before producing artifacts.
    # In the crash case the worker may have left `Tablet/` dirty before
    # dying; restore from the kernel-captured `active_worker_base`
    # snapshot BEFORE syncing so the supervisor sees the clean baseline.
    # The kernel returns Ok(false) only for genuinely benign cases
    # (no in-flight request, non-Worker request, no metadata) where
    # there is nothing to restore. The hazard case — in-flight Worker
    # request exists but the snapshot dir is missing — fails loudly
    # with InvalidRuntimeState, surfaces here as a KernelCliError, and
    # is correctly classified as a transport_failure below. So we don't
    # need to inspect the boolean return here; if we got back here
    # without an exception, the bridge can safely proceed.
    try:
        if not _bridge_dry_run_enabled():
            _restore_active_worker_base_via_kernel(runtime_root)
    except Exception as exc:
        return _save_malformed_worker_result(
            runtime_root=runtime_root,
            request=request,
            raw_payload=None,
            acceptance_context_path=None,
            authoritative_repo_path=authoritative_repo,
            errors=[f"restore active_worker_base failed: {exc}"],
            transport_failure=True,
        )
    try:
        authoritative_sync = sync_supervisor_workspace(config.repo_path)
        authoritative_repo = Path(
            str(authoritative_sync.get("authoritative_repo_path", "") or "")
        ).resolve()
        acceptance_context_result = build_trellis_worker_acceptance_context(
            authoritative_repo,
            request,
            collect_observations=not _bridge_dry_run_enabled(),
            paper_source_path=config.workflow.paper_tex_path,
        )
        if not acceptance_context_result["ok"] or not isinstance(
            acceptance_context_result["data"], dict
        ):
            # Bug X: gate-prep failure = the worker never had a chance to
            # run. Treat as transport-failure so the kernel uses the
            # transport_invalid_review_threshold budget rather than the
            # work-quality (invalid_attempt) budget.
            return _save_malformed_worker_result(
                runtime_root=runtime_root,
                request=request,
                raw_payload=None,
                acceptance_context_path=None,
                authoritative_repo_path=authoritative_repo,
                errors=list(acceptance_context_result["errors"]),
                validation_errors=list(acceptance_context_result.get("validation_errors", [])),
                contract_errors=list(acceptance_context_result.get("contract_errors", [])),
                transport_failure=True,
            )
        # Bug C fix: build_trellis_worker_acceptance_context invokes the
        # kernel's `prepare_worker_gate_output`, which runs
        # `sync_tablet_render_support_from_repo` on the supervisor repo. That
        # regenerates `Tablet/INDEX.md`, `Tablet/README.md`, `Tablet/header.tex`
        # and captures `before_snapshot` immediately after. The worker repo
        # (which the worker's own check.py invocations validate against) still
        # has stale git-HEAD versions of those files. Without back-propagating
        # the freshly-generated supervisor versions to the worker repo, the
        # worker's self-validation reports phantom modifications on these
        # auto-managed files — Easy mode rejects, the worker can't revert
        # files outside its scope, deadlock. Back-propagating BEFORE the
        # worker burst keeps both repos in sync for the worker's view.
        propagate_tablet_back_to_worker(authoritative_repo, config.repo_path)
        acceptance_context = dict(acceptance_context_result["data"])
        save_json(acceptance_context_path, acceptance_context)
    except Exception as exc:
        # Bug X: pre-burst exception (sync_supervisor_workspace,
        # build_trellis_worker_acceptance_context, propagate_tablet_back,
        # save_json) — the worker never ran. Transport.
        return _save_malformed_worker_result(
            runtime_root=runtime_root,
            request=request,
            raw_payload=None,
            acceptance_context_path=acceptance_context_path,
            authoritative_repo_path=authoritative_repo,
            errors=[str(exc)],
            transport_failure=True,
        )
    request_path = staging_dir / canonical_name.replace(
        ".json",
        ".request.json",
    )
    save_json(request_path, dict(request))
    raw_verifier_evidence = request.get("review_verifier_evidence", {})
    verifier_evidence = (
        dict(raw_verifier_evidence)
        if isinstance(raw_verifier_evidence, Mapping)
        else {}
    )
    verifier_evidence_path = None
    if verifier_evidence:
        verifier_evidence_path = staging_dir / canonical_name.replace(
            ".json",
            ".verifier_evidence.json",
        )
        save_json(verifier_evidence_path, verifier_evidence)
    worker_support = _prepare_worker_support_files(
        repo_path=config.repo_path,
        request=request,
    )
    try:
        prompt = build_worker_prompt(
            request=dict(request),
            worker_gate=acceptance_context,
            repo_path=config.repo_path,
            raw_output_path=raw_path,
            done_path=done_path,
            acceptance_context_path=acceptance_context_path,
            runtime_root=runtime_root,
            verifier_evidence_path=verifier_evidence_path,
            theorem_initial_dag_size_guidance=_theorem_initial_dag_size_guidance(_policy(config)),
            scratch_workspace_path=worker_support["scratch_workspace_path"],
            scratch_workspace_status_text=str(worker_support["scratch_workspace_status_text"]),
            scratch_readme_path=worker_support["scratch_readme_path"],
            scratch_notes_path=worker_support["scratch_notes_path"],
            scratch_example_path=worker_support["scratch_example_path"],
            last_invalid_root_path=worker_support["last_invalid_path"],
            last_invalid_metadata_file_path=worker_support["last_invalid_metadata_path"],
        )
    except ValueError as exc:
        # Bug X: prompt-construction failure — the worker never ran.
        # Transport (this is a bridge-side template/validation issue, not a
        # worker-quality issue).
        return _save_malformed_worker_result(
            runtime_root=runtime_root,
            request=request,
            raw_payload=None,
            acceptance_context_path=acceptance_context_path,
            authoritative_repo_path=authoritative_repo,
            errors=[str(exc)],
            transport_failure=True,
        )
    single = _single_request_common(
        config=config,
        runtime_root=runtime_root,
        request=request,
        provider=_worker_provider_for_request(request),
        lane=AgentLane(kind="worker"),
        kind_label="worker",
        burst_role=_worker_burst_role(request),
        prompt=prompt,
        artifact=artifact,
    )
    if _bridge_dry_run_enabled():
        return _dry_run_single(
            runtime_root=runtime_root,
            request=request,
            single=single,
            prompt=prompt,
        )
    # Audit followup #2 (Problem A): SIGHUP recovery is now handled at the
    # top of this function (before the acceptance context is rebuilt).
    # By this point we know `recovered.payload` was None — there's no
    # `.done` artifact, so the worker has not yet produced output and we
    # need to run the burst.
    response = execute_agent_request(
        single,
        port_resolver=DefaultLanePortResolver(),
        validate_artifact=False,
    )
    if not response.ok:
        # Bug X: this is the primary transport-failure path — the agent
        # burst (run_worker_burst → tmux_backend / codex_headless / etc.)
        # returned ok=False. Reasons include: timeout / hang
        # (stable_without_done_file), agent crash mid-burst (silent_failure,
        # SIGKILL/SIGHUP), missing done marker after completion,
        # rate-limit retries exhausted, agent never settled. All of these
        # mean the worker never produced any meaningful output the kernel
        # could evaluate — flag as transport_failure so the kernel uses the
        # transport budget rather than the work-quality budget.
        return _save_malformed_worker_result(
            runtime_root=runtime_root,
            request=request,
            raw_payload=None,
            acceptance_context_path=acceptance_context_path,
            authoritative_repo_path=authoritative_repo,
            errors=[str(response.error or "worker execution failed")],
            transport_failure=True,
        )
    raw_payload: Dict[str, Any] | None = None
    normalized_result: Dict[str, Any]
    try:
        raw_payload = (
            dict(response.payload)
            if isinstance(response.payload, dict)
            else _load_raw_response_json(response)
        )
        authoritative_sync = sync_supervisor_workspace(config.repo_path)
        authoritative_repo = Path(
            str(authoritative_sync.get("authoritative_repo_path", "") or "")
        ).resolve()
        normalized_result = normalize_trellis_worker_result_data(
            raw_payload,
            repo=authoritative_repo,
            acceptance_context=acceptance_context,
        )
        # Auto-fix is now decoupled from validation. validate_worker_result
        # runs the same pure check on both the worker burst's check.py and
        # here on the supervisor — they agree by construction (no more
        # "authoritative checker mismatch" from auto-fix-induced asymmetry).
        # #55: orphan-import auto-fix runs INSIDE the kernel CLI subcommand
        # (`check_trellis_worker_result_output`) BEFORE
        # `populate_response_fingerprints`, so kernel state and disk agree
        # by construction. The bridge no longer calls auto_fix; it only
        # propagates the post-auto-fix Tablet/ to the worker repo.
        propagate_tablet_back_to_worker(authoritative_repo, config.repo_path)
        record_worker_checker_trace(
            authoritative_repo,
            acceptance_context=acceptance_context,
            result=normalized_result,
            source="supervisor",
        )
        mismatch_detail = _checker_mismatch_detail(
            load_worker_checker_trace(
                config.repo_path,
                request_id=_request_id(request),
            ),
            normalized_result,
        )
        if mismatch_detail:
            normalized_result = _invalidate_for_checker_mismatch(
                normalized_result,
                detail=mismatch_detail,
            )
    except BridgeError as exc:
        normalized_result = {"errors": [str(exc)]}
    normalized = normalized_result.get("response")
    if not isinstance(normalized, dict):
        errors = list(normalized_result.get("errors", []))
        return _save_malformed_worker_result(
            runtime_root=runtime_root,
            request=request,
            raw_payload=raw_payload,
            acceptance_context_path=acceptance_context_path,
            authoritative_repo_path=authoritative_repo,
            errors=errors,
            validation_errors=list(normalized_result.get("validation_errors", [])),
            contract_errors=list(normalized_result.get("contract_errors", [])),
        )
    final_outcome = str(normalized_result.get("final_outcome", "") or "").strip().lower()
    normalized["kind"] = "worker"
    _save_bridge_json(
        runtime_root,
        "latest_worker.json",
        {
            "raw": raw_payload,
            "response": normalized,
            "acceptance_context_path": str(acceptance_context_path),
            "authoritative_repo_path": str(authoritative_repo),
            # Match the schema written by _save_malformed_worker_result so
            # bridge/latest_worker.json reflects the current burst unambiguously
            # — without `errors`, a stale list from a prior malformed burst
            # could bleed into the reader's interpretation of this success.
            "errors": [],
            "validation_errors": list(normalized_result.get("validation_errors", [])),
            "contract_errors": list(normalized_result.get("contract_errors", [])),
            "final_outcome": final_outcome,
        },
    )
    return normalized


def _run_corr_panel(
    *,
    config: Config,
    runtime_root: Path,
    request: Mapping[str, Any],
    verify_nodes: Iterable[str],
    verify_targets: Iterable[str],
) -> tuple[Any, Dict[str, SingleAgentRequest], Dict[str, Any]]:
    lanes = _sorted_strs(request.get("verify_lanes", []))
    bindings = _verification_lane_bindings(request, kind="corr")
    prompt_request = dict(request)
    prompt_request["verify_nodes"] = _sorted_strs(verify_nodes)
    prompt_request["verify_targets"] = _sorted_strs(verify_targets)
    members: List[SingleAgentRequest] = []
    member_map: Dict[str, SingleAgentRequest] = {}
    for index, lane_id in enumerate(lanes):
        provider = _provider_from_lane_binding(bindings[index])
        canonical_name = _artifact_name("corr", _request_id(request), lane_id)
        artifact = ArtifactSpec(canonical_name=canonical_name, kind="correspondence-result")
        raw_path = _bridge_state_dir(config.repo_path, runtime_root) / "staging" / canonical_name.replace(".json", ".raw.json")
        done_path = _bridge_state_dir(config.repo_path, runtime_root) / "staging" / canonical_name.replace(".json", ".done")
        request_path = _bridge_state_dir(config.repo_path, runtime_root) / "staging" / canonical_name.replace(".json", ".request.json")
        save_json(request_path, prompt_request)
        prompt = build_correspondence_prompt(
            request=prompt_request,
            repo_path=config.repo_path,
            lane_id=lane_id,
            raw_output_path=raw_path,
            done_path=done_path,
        )
        member = _single_request_common(
            config=config,
            runtime_root=runtime_root,
            request=request,
            provider=provider,
            lane=AgentLane(kind="correspondence", agent_index=index, node_name=lane_id),
            kind_label=lane_id,
            burst_role="reviewer",
            prompt=prompt,
            artifact=artifact,
        )
        members.append(member)
        member_map[member.request_id] = member
    if _bridge_dry_run_enabled():
        return _dry_run_panel(
            runtime_root=runtime_root,
            request=request,
            members=members,
        )
    raw = execute_panel_raw(
        PanelRequest(
            request_id=f"corr-{_request_id(request)}",
            cycle=_request_cycle(request),
            kind="corr",
            members=members,
        ),
        port_resolver=DefaultLanePortResolver(),
    )
    return raw, member_map, prompt_request


def _run_paper_panel(
    *,
    config: Config,
    runtime_root: Path,
    request: Mapping[str, Any],
    verify_targets: Iterable[str],
) -> tuple[Any, Dict[str, SingleAgentRequest], Dict[str, Any]]:
    lanes = _sorted_strs(request.get("verify_lanes", []))
    bindings = _verification_lane_bindings(request, kind="paper")
    prompt_request = dict(request)
    prompt_request["verify_targets"] = _sorted_strs(verify_targets)
    # Per-node and deviation scenarios: forward kernel scenario fields.
    # so the prompt builder can detect the per-node scenario via
    # `node_paper_basis_inputs` in the contract.
    prompt_request["substantiveness_verify_nodes"] = _sorted_strs(
        request.get("substantiveness_verify_nodes", [])
    )
    prompt_request["deviation_verify_id"] = request.get("deviation_verify_id") or None
    prompt_request["deviation_verify_path"] = str(request.get("deviation_verify_path") or "")
    # Artifact kind branches by scenario:
    #   - target-level Paper request → `paper-faithfulness-result` (issues[])
    #   - per-node substantiveness request → `substantiveness-result`
    #     (verdicts[] with explicit Pass/Fail/NotDoneYet per node)
    is_per_node_scenario = bool(prompt_request["substantiveness_verify_nodes"]) and not bool(
        prompt_request["verify_targets"]
    )
    is_deviation_scenario = bool(prompt_request["deviation_verify_id"]) and not bool(
        prompt_request["verify_targets"]
    )
    if is_deviation_scenario:
        artifact_kind = "deviation-authorization-result"
    elif is_per_node_scenario:
        artifact_kind = "substantiveness-result"
    else:
        artifact_kind = "paper-faithfulness-result"
    members: List[SingleAgentRequest] = []
    member_map: Dict[str, SingleAgentRequest] = {}
    for index, lane_id in enumerate(lanes):
        provider = _provider_from_lane_binding(bindings[index])
        canonical_name = _artifact_name("paper", _request_id(request), lane_id)
        artifact = ArtifactSpec(
            canonical_name=canonical_name,
            kind=artifact_kind,
        )
        raw_path = _bridge_state_dir(config.repo_path, runtime_root) / "staging" / canonical_name.replace(".json", ".raw.json")
        done_path = _bridge_state_dir(config.repo_path, runtime_root) / "staging" / canonical_name.replace(".json", ".done")
        request_path = _bridge_state_dir(config.repo_path, runtime_root) / "staging" / canonical_name.replace(".json", ".request.json")
        save_json(request_path, prompt_request)
        prompt = build_paper_faithfulness_prompt(
            request=prompt_request,
            repo_path=config.repo_path,
            lane_id=lane_id,
            raw_output_path=raw_path,
            done_path=done_path,
        )
        member = _single_request_common(
            config=config,
            runtime_root=runtime_root,
            request=request,
            provider=provider,
            lane=AgentLane(kind="paper-faithfulness", agent_index=index, node_name=lane_id),
            kind_label=lane_id,
            burst_role="reviewer",
            prompt=prompt,
            artifact=artifact,
        )
        members.append(member)
        member_map[member.request_id] = member
    if _bridge_dry_run_enabled():
        return _dry_run_panel(
            runtime_root=runtime_root,
            request=request,
            members=members,
        )
    raw = execute_panel_raw(
        PanelRequest(
            request_id=f"paper-{_request_id(request)}",
            cycle=_request_cycle(request),
            kind="paper",
            members=members,
        ),
        port_resolver=DefaultLanePortResolver(),
    )
    return raw, member_map, prompt_request


def _run_sound_panel(
    *,
    config: Config,
    runtime_root: Path,
    request: Mapping[str, Any],
    verify_nodes: Iterable[str],
) -> tuple[Any, Dict[str, SingleAgentRequest], Dict[str, Any]]:
    lanes = _sorted_strs(request.get("verify_lanes", []))
    bindings = _verification_lane_bindings(request, kind="sound")
    nodes = _sorted_strs(verify_nodes)
    node_name = str(request.get("sound_verify_node", "") or "").strip()
    if not node_name:
        raise BridgeError("sound request is missing kernel-authored sound_verify_node")
    prompt_request = dict(request)
    prompt_request["verify_nodes"] = nodes
    members: List[SingleAgentRequest] = []
    member_map: Dict[str, SingleAgentRequest] = {}
    for index, lane_id in enumerate(lanes):
        provider = _provider_from_lane_binding(bindings[index])
        canonical_name = _artifact_name("sound", _request_id(request), lane_id)
        artifact = ArtifactSpec(
            canonical_name=canonical_name,
            kind="soundness-result",
            node_name=node_name,
        )
        raw_path = _bridge_state_dir(config.repo_path, runtime_root) / "staging" / canonical_name.replace(".json", ".raw.json")
        done_path = _bridge_state_dir(config.repo_path, runtime_root) / "staging" / canonical_name.replace(".json", ".done")
        request_path = _bridge_state_dir(config.repo_path, runtime_root) / "staging" / canonical_name.replace(".json", ".request.json")
        save_json(request_path, prompt_request)
        prompt = build_soundness_prompt(
            request=prompt_request,
            repo_path=config.repo_path,
            lane_id=lane_id,
            node_name=node_name,
            raw_output_path=raw_path,
            done_path=done_path,
        )
        member = _single_request_common(
            config=config,
            runtime_root=runtime_root,
            request=request,
            provider=provider,
            lane=AgentLane(kind="soundness-node", agent_index=index, node_name=lane_id),
            kind_label=lane_id,
            burst_role="reviewer",
            prompt=prompt,
            artifact=artifact,
        )
        members.append(member)
        member_map[member.request_id] = member
    if _bridge_dry_run_enabled():
        return _dry_run_panel(
            runtime_root=runtime_root,
            request=request,
            members=members,
        )
    raw = execute_panel_raw(
        PanelRequest(
            request_id=f"sound-{_request_id(request)}",
            cycle=_request_cycle(request),
            kind="sound",
            members=members,
        ),
        port_resolver=DefaultLanePortResolver(),
    )
    return raw, member_map, prompt_request


def _handle_corr(
    *,
    config: Config,
    runtime_root: Path,
    request: Mapping[str, Any],
) -> Dict[str, Any]:
    panel_result = _run_corr_panel(
        config=config,
        runtime_root=runtime_root,
        request=request,
        verify_nodes=request.get("verify_nodes", []),
        verify_targets=request.get("verify_targets", []),
    )
    if isinstance(panel_result, dict):
        return panel_result
    raw, member_map, prompt_request = panel_result
    normalized = _normalize_corr_via_kernel(
        request_id=_request_id(request),
        cycle=_request_cycle(request),
        verify_lanes=prompt_request.get("verify_lanes", []),
        verify_nodes=prompt_request.get("verify_nodes", []),
        verify_targets=prompt_request.get("verify_targets", []),
        raw_panel=raw,
        request_members=member_map,
    )
    corr_payload = raw.to_dict()
    corr_payload["normalized"] = normalized
    _save_bridge_json(runtime_root, "latest_corr.json", corr_payload)
    return {
        "kind": "corr",
        "request_id": _request_id(request),
        "cycle": _request_cycle(request),
        "status": "Ok",
        "node_lane_updates": normalized.get("node_lane_updates", {}),
        "target_lane_updates": normalized.get("target_lane_updates", {}),
        "reviewer_evidence": normalized.get("reviewer_evidence", {}),
    }


def _handle_paper(
    *,
    config: Config,
    runtime_root: Path,
    request: Mapping[str, Any],
) -> Dict[str, Any]:
    """
    Two scenarios share `RequestKind::Paper` and `Stage::VerifyPaper`:
      - Target-package: `paper_verify_targets` non-empty.
      - Deviation:      `deviation_verify_id` populated.
      - Per-node:       `substantiveness_verify_nodes` non-empty.
    The kernel cycle scheduler guarantees exactly one frontier is
    populated per request; we branch on that here. Both flow through
    `_run_paper_panel` (same prompt builder; the prompt itself routes to
    the per-node fragment when `substantiveness_verify_nodes` is non-empty), and
    the kernel normalizer handles target vs node bucketing.
    """
    paper_verify_targets = _sorted_strs(request.get("paper_verify_targets", []))
    substantiveness_verify_nodes = _sorted_strs(request.get("substantiveness_verify_nodes", []))
    deviation_verify_id = str(request.get("deviation_verify_id") or "").strip()
    deviation_verify_ids = [deviation_verify_id] if deviation_verify_id else []
    if paper_verify_targets or substantiveness_verify_nodes or deviation_verify_ids:
        panel_result = _run_paper_panel(
            config=config,
            runtime_root=runtime_root,
            request=request,
            verify_targets=paper_verify_targets,
        )
        if isinstance(panel_result, dict):
            return panel_result
        raw, member_map, prompt_request = panel_result
    else:
        raw = PanelExecutionResponse(
            request_id=f"paper-{_request_id(request)}",
            cycle=_request_cycle(request),
            kind="paper",
            member_responses=[],
        )
        member_map = {}
        prompt_request = dict(request)
        prompt_request["verify_targets"] = []
        prompt_request["substantiveness_verify_nodes"] = []
        prompt_request["deviation_verify_id"] = None
        prompt_request["deviation_verify_path"] = ""
    normalized = _normalize_paper_via_kernel(
        request_id=_request_id(request),
        cycle=_request_cycle(request),
        verify_lanes=prompt_request.get("verify_lanes", []),
        verify_targets=prompt_request.get("verify_targets", []),
        verify_nodes=prompt_request.get("substantiveness_verify_nodes", []),
        verify_deviations=[prompt_request["deviation_verify_id"]]
        if prompt_request.get("deviation_verify_id")
        else [],
        raw_panel=raw,
        request_members=member_map,
    )
    paper_payload = raw.to_dict()
    paper_payload["normalized"] = normalized
    _save_bridge_json(runtime_root, "latest_paper.json", paper_payload)
    return {
        "kind": "paper",
        "request_id": _request_id(request),
        "cycle": _request_cycle(request),
        "status": "Ok",
        "target_lane_updates": normalized.get("target_lane_updates", {}),
        "node_lane_updates": normalized.get("node_lane_updates", {}),
        "deviation_lane_updates": normalized.get("deviation_lane_updates", {}),
        "reviewer_evidence": normalized.get("reviewer_evidence", {}),
        "node_reviewer_evidence": normalized.get("node_reviewer_evidence", {}),
    }


def _handle_sound(
    *,
    config: Config,
    runtime_root: Path,
    request: Mapping[str, Any],
) -> Dict[str, Any]:
    panel_result = _run_sound_panel(
        config=config,
        runtime_root=runtime_root,
        request=request,
        verify_nodes=request.get("verify_nodes", []),
    )
    if isinstance(panel_result, dict):
        return panel_result
    raw, member_map, prompt_request = panel_result
    normalized = _normalize_sound_via_kernel(
        request_id=_request_id(request),
        cycle=_request_cycle(request),
        verify_lanes=prompt_request.get("verify_lanes", []),
        verify_nodes=prompt_request.get("verify_nodes", []),
        raw_panel=raw,
        request_members=member_map,
    )
    sound_payload = raw.to_dict()
    sound_payload["normalized"] = normalized
    _save_bridge_json(runtime_root, "latest_sound.json", sound_payload)
    return {
        "kind": "sound",
        "request_id": _request_id(request),
        "cycle": _request_cycle(request),
        "status": "Ok",
        "lane_updates": normalized.get("lane_updates", {}),
        "reviewer_evidence": normalized.get("reviewer_evidence", {}),
    }


def _handle_review(
    *,
    config: Config,
    runtime_root: Path,
    request: Mapping[str, Any],
) -> Dict[str, Any]:
    canonical_name = _artifact_name("review", _request_id(request), "decision")
    artifact = ArtifactSpec(
        canonical_name=canonical_name,
        kind="trellis-reviewer-result",
        phase=_phase_name(request),
        invalid_attempt=bool(request.get("invalid_attempt", False)),
    )
    raw_path = _bridge_state_dir(config.repo_path, runtime_root) / "staging" / canonical_name.replace(".json", ".raw.json")
    done_path = _bridge_state_dir(config.repo_path, runtime_root) / "staging" / canonical_name.replace(".json", ".done")
    context_json_path = (
        _bridge_state_dir(config.repo_path, runtime_root)
        / "staging"
        / canonical_name.replace(".json", ".context.json")
    )
    save_json(context_json_path, dict(request))
    request_path = (
        _bridge_state_dir(config.repo_path, runtime_root)
        / "staging"
        / canonical_name.replace(".json", ".request.json")
    )
    save_json(request_path, dict(request))
    verifier_evidence_path = (
        _bridge_state_dir(config.repo_path, runtime_root)
        / "staging"
        / canonical_name.replace(".json", ".evidence.json")
    )
    verifier_evidence = dict((request.get("review_contract") or {}).get("verifier_evidence") or {})
    if verifier_evidence:
        save_json(verifier_evidence_path, verifier_evidence)
    try:
        prompt = build_review_prompt(
            request=dict(request),
            repo_path=config.repo_path,
            runtime_root=runtime_root,
            raw_output_path=raw_path,
            done_path=done_path,
            context_json_path=context_json_path,
            theorem_initial_dag_size_guidance=_theorem_initial_dag_size_guidance(_policy(config)),
        )
    except ValueError as exc:
        raise BridgeError(str(exc)) from exc
    single = _single_request_common(
        config=config,
        runtime_root=runtime_root,
        request=request,
        provider=_provider_from_request_binding(request, field_name="reviewer_binding"),
        lane=AgentLane(kind="reviewer"),
        kind_label="reviewer",
        burst_role="reviewer",
        prompt=prompt,
        artifact=artifact,
    )
    if _bridge_dry_run_enabled():
        return _dry_run_single(
            runtime_root=runtime_root,
            request=request,
            single=single,
            prompt=prompt,
        )
    # Bug X principled fix Phase 4: SIGHUP recovery for reviewer too.
    # See _handle_worker for the rationale. Reviewer's `done` artifact
    # at the same naming convention.
    recovered = _recover_done_artifact(done_path=done_path, raw_path=raw_path)
    if recovered.parse_error is not None:
        errors = [recovered.parse_error]
        malformed = _build_malformed_response(kind="review", request=request)
        _save_bridge_json(
            runtime_root,
            "latest_review.json",
            {
                "raw": None,
                "response": malformed,
                "context_json_path": str(context_json_path),
                "errors": errors,
            },
        )
        return malformed
    if recovered.payload is not None:
        response = SingleAgentResponse(
            request_id=str(_request_id(request)),
            cycle=_request_cycle(request),
            kind="review",
            burst_role="reviewer",
            ok=True,
            payload=recovered.payload,
            raw_path=raw_path,
            done_path=done_path,
        )
    else:
        response = execute_agent_request(
            single,
            port_resolver=DefaultLanePortResolver(),
            validate_artifact=False,
        )
    if not response.ok:
        errors = [str(response.error or "reviewer execution failed")]
        malformed = _build_malformed_response(kind="review", request=request)
        _save_bridge_json(
            runtime_root,
            "latest_review.json",
            {
                "raw": None,
                "response": malformed,
                "context_json_path": str(context_json_path),
                "errors": errors,
            },
        )
        return malformed
    raw_payload: Dict[str, Any] | None = None
    normalized_result: Dict[str, Any]
    try:
        raw_payload = _load_raw_response_json(response)
        normalized_result = normalize_trellis_reviewer_result_data(
            raw_payload,
            review_request=request,
        )
    except BridgeError as exc:
        normalized_result = {"errors": [str(exc)]}
    normalized = normalized_result.get("response")
    if not isinstance(normalized, dict):
        errors = list(normalized_result.get("errors", []))
        malformed = _build_malformed_response(kind="review", request=request)
        _save_bridge_json(
            runtime_root,
            "latest_review.json",
            {
                "raw": raw_payload,
                "response": malformed,
                "context_json_path": str(context_json_path),
                "errors": errors,
            },
        )
        return malformed
    _save_bridge_json(
        runtime_root,
        "latest_review.json",
        {
            "raw": raw_payload,
            "response": normalized,
            "context_json_path": str(context_json_path),
        },
    )
    return normalized


def _handle_audit(
    *,
    config: Config,
    runtime_root: Path,
    request: Mapping[str, Any],
) -> Dict[str, Any]:
    """Cleanup-v2 (audit Finding 1): handle a `RequestKind::Audit` bridge
    request.

    Parallel to `_handle_review`. Routes through the `reviewer_binding`
    actor (per `CLAUDES_NOTES_cleanup_v2_impl_plan.md` §2.6 — reviewer
    binding chosen as the minimal config burden), emits the audit prompt
    via `build_audit_prompt`, and normalizes the LLM's structured JSON
    output via `normalize_trellis_audit_result_data` (which round-trips
    through the kernel CLI to enforce shape).
    """
    canonical_name = _artifact_name("audit", _request_id(request), "result")
    artifact = ArtifactSpec(
        canonical_name=canonical_name,
        kind="trellis-audit-result",
        phase=_phase_name(request),
        invalid_attempt=bool(request.get("invalid_attempt", False)),
    )
    raw_path = (
        _bridge_state_dir(config.repo_path, runtime_root)
        / "staging"
        / canonical_name.replace(".json", ".raw.json")
    )
    done_path = (
        _bridge_state_dir(config.repo_path, runtime_root)
        / "staging"
        / canonical_name.replace(".json", ".done")
    )
    context_json_path = (
        _bridge_state_dir(config.repo_path, runtime_root)
        / "staging"
        / canonical_name.replace(".json", ".context.json")
    )
    save_json(context_json_path, dict(request))
    request_path = (
        _bridge_state_dir(config.repo_path, runtime_root)
        / "staging"
        / canonical_name.replace(".json", ".request.json")
    )
    save_json(request_path, dict(request))
    try:
        prompt = build_audit_prompt(
            request=dict(request),
            repo_path=config.repo_path,
            runtime_root=runtime_root,
            raw_output_path=raw_path,
            done_path=done_path,
            context_json_path=context_json_path,
        )
    except ValueError as exc:
        raise BridgeError(str(exc)) from exc
    single = _single_request_common(
        config=config,
        runtime_root=runtime_root,
        request=request,
        provider=_provider_from_request_binding(request, field_name="reviewer_binding"),
        lane=AgentLane(kind="reviewer"),
        kind_label="audit",
        burst_role="reviewer",
        prompt=prompt,
        artifact=artifact,
    )
    if _bridge_dry_run_enabled():
        return _dry_run_single(
            runtime_root=runtime_root,
            request=request,
            single=single,
            prompt=prompt,
        )
    # SIGHUP recovery: same pattern as reviewer (`_handle_review`).
    recovered = _recover_done_artifact(done_path=done_path, raw_path=raw_path)
    if recovered.parse_error is not None:
        errors = [recovered.parse_error]
        malformed = _build_malformed_response(kind="audit", request=request)
        _save_bridge_json(
            runtime_root,
            "latest_audit.json",
            {
                "raw": None,
                "response": malformed,
                "context_json_path": str(context_json_path),
                "errors": errors,
            },
        )
        return malformed
    if recovered.payload is not None:
        response = SingleAgentResponse(
            request_id=str(_request_id(request)),
            cycle=_request_cycle(request),
            kind="audit",
            burst_role="reviewer",
            ok=True,
            payload=recovered.payload,
            raw_path=raw_path,
            done_path=done_path,
        )
    else:
        response = execute_agent_request(
            single,
            port_resolver=DefaultLanePortResolver(),
            validate_artifact=False,
        )
    if not response.ok:
        errors = [str(response.error or "audit execution failed")]
        malformed = _build_malformed_response(kind="audit", request=request)
        _save_bridge_json(
            runtime_root,
            "latest_audit.json",
            {
                "raw": None,
                "response": malformed,
                "context_json_path": str(context_json_path),
                "errors": errors,
            },
        )
        return malformed
    raw_payload: Dict[str, Any] | None = None
    normalized_result: Dict[str, Any]
    try:
        raw_payload = _load_raw_response_json(response)
        normalized_result = normalize_trellis_audit_result_data(
            raw_payload,
            audit_request=request,
        )
    except BridgeError as exc:
        normalized_result = {"errors": [str(exc)]}
    normalized = normalized_result.get("response")
    if not isinstance(normalized, dict):
        errors = list(normalized_result.get("errors", []))
        malformed = _build_malformed_response(kind="audit", request=request)
        _save_bridge_json(
            runtime_root,
            "latest_audit.json",
            {
                "raw": raw_payload,
                "response": malformed,
                "context_json_path": str(context_json_path),
                "errors": errors,
            },
        )
        return malformed
    _save_bridge_json(
        runtime_root,
        "latest_audit.json",
        {
            "raw": raw_payload,
            "response": normalized,
            "context_json_path": str(context_json_path),
        },
    )
    return normalized


def _handle_stuck_math_audit(
    *,
    config: Config,
    runtime_root: Path,
    request: Mapping[str, Any],
) -> Dict[str, Any]:
    canonical_name = _artifact_name("stuck_math_audit", _request_id(request), "result")
    artifact = ArtifactSpec(
        canonical_name=canonical_name,
        kind="trellis-stuck-math-audit-result",
        phase=_phase_name(request),
        invalid_attempt=bool(request.get("invalid_attempt", False)),
    )
    staging_dir = _bridge_state_dir(config.repo_path, runtime_root) / "staging"
    raw_path = staging_dir / canonical_name.replace(".json", ".raw.json")
    done_path = staging_dir / canonical_name.replace(".json", ".done")
    context_json_path = staging_dir / canonical_name.replace(".json", ".context.json")
    save_json(context_json_path, dict(request))
    request_path = staging_dir / canonical_name.replace(".json", ".request.json")
    save_json(request_path, dict(request))
    try:
        prompt = build_stuck_math_audit_prompt(
            request=dict(request),
            repo_path=config.repo_path,
            runtime_root=runtime_root,
            raw_output_path=raw_path,
            done_path=done_path,
            context_json_path=context_json_path,
        )
    except ValueError as exc:
        raise BridgeError(str(exc)) from exc
    stuck_math_contract = request.get("stuck_math_audit_contract")
    burst_role = (
        str(stuck_math_contract.get("burst_role") or "stuck_math_audit")
        if isinstance(stuck_math_contract, Mapping)
        else "stuck_math_audit"
    )
    single = _single_request_common(
        config=config,
        runtime_root=runtime_root,
        request=request,
        provider=_provider_from_request_binding(request, field_name="stuck_math_audit_binding"),
        lane=AgentLane(kind="stuck_math_audit"),
        kind_label="audit",
        burst_role=burst_role,
        prompt=prompt,
        artifact=artifact,
    )
    if _bridge_dry_run_enabled():
        return _dry_run_single(
            runtime_root=runtime_root,
            request=request,
            single=single,
            prompt=prompt,
        )
    recovered = _recover_done_artifact(done_path=done_path, raw_path=raw_path)
    if recovered.parse_error is not None:
        errors = [recovered.parse_error]
        malformed = _build_malformed_response(kind="stuck_math_audit", request=request)
        _save_bridge_json(
            runtime_root,
            "latest_stuck_math_audit.json",
            {
                "raw": None,
                "response": malformed,
                "context_json_path": str(context_json_path),
                "errors": errors,
            },
        )
        return malformed
    if recovered.payload is not None:
        response = SingleAgentResponse(
            request_id=str(_request_id(request)),
            cycle=_request_cycle(request),
            kind="stuck_math_audit",
            burst_role=burst_role,
            ok=True,
            payload=recovered.payload,
            raw_path=raw_path,
            done_path=done_path,
        )
    else:
        response = execute_agent_request(
            single,
            port_resolver=DefaultLanePortResolver(),
            validate_artifact=False,
        )
    if not response.ok:
        errors = [str(response.error or "stuck math audit execution failed")]
        malformed = _build_malformed_response(kind="stuck_math_audit", request=request)
        _save_bridge_json(
            runtime_root,
            "latest_stuck_math_audit.json",
            {
                "raw": None,
                "response": malformed,
                "context_json_path": str(context_json_path),
                "errors": errors,
            },
        )
        return malformed
    raw_payload: Dict[str, Any] | None = None
    normalized_result: Dict[str, Any]
    try:
        raw_payload = _load_raw_response_json(response)
        normalized_result = normalize_trellis_stuck_math_audit_result_data(
            raw_payload,
            audit_request=request,
            repo=config.repo_path,
        )
    except BridgeError as exc:
        normalized_result = {"errors": [str(exc)]}
    normalized = normalized_result.get("response")
    if not isinstance(normalized, dict):
        errors = list(normalized_result.get("errors", []))
        malformed = _build_malformed_response(kind="stuck_math_audit", request=request)
        _save_bridge_json(
            runtime_root,
            "latest_stuck_math_audit.json",
            {
                "raw": raw_payload,
                "response": malformed,
                "context_json_path": str(context_json_path),
                "errors": errors,
            },
        )
        return malformed
    _save_bridge_json(
        runtime_root,
        "latest_stuck_math_audit.json",
        {
            "raw": raw_payload,
            "response": normalized,
            "context_json_path": str(context_json_path),
        },
    )
    return normalized


def _handle_human_gate(
    *,
    runtime_root: Path,
    request: Mapping[str, Any],
) -> Dict[str, Any]:
    path = runtime_root / "human_gate_response.json"
    if not path.exists():
        raise BridgeError(f"missing human gate response file: {path}")
    try:
        raw_payload_text = path.read_text(encoding="utf-8")
    except Exception as exc:
        raise BridgeError(f"failed to read human gate response file: {exc}") from exc
    try:
        kernel_response = run_kernel_cli(
            {
                "action": "normalize_human_gate",
                "request_id": _request_id(request),
                "cycle": _request_cycle(request),
                "raw_payload_text": raw_payload_text,
            }
        )
    except KernelCliError as exc:
        raise BridgeError(f"kernel CLI failed: {exc}") from exc
    if kernel_response.get("status") != "normalize_human_gate_ok":
        raise BridgeError(
            "unexpected kernel normalize_human_gate response status: "
            f"{kernel_response.get('status')!r}"
        )
    output = kernel_response.get("output")
    if not isinstance(output, dict):
        raise BridgeError("kernel normalize_human_gate response is missing output")
    return output


def handle_bridge_request(bridge_request: BridgeCliRequest) -> Dict[str, Any]:
    # Fail-loudly halt: if the kernel persisted a checker-disagreement
    # marker on a prior step, refuse to dispatch this burst.
    # `feedback_fail_loudly_on_dual_check`: the disagreement is
    # structural — retries will reproduce it — so the supervisor must
    # stop instead of burning provider quota chasing ghosts. Operator
    # clears the halt by deleting the marker (see clear_instructions
    # field inside the JSON).
    bridge_request.runtime_root.mkdir(parents=True, exist_ok=True)
    halt_marker = checker_disagreement_halt_marker_path(bridge_request.runtime_root)
    if halt_marker.exists():
        raise BridgeError(
            "trellis: checker_disagreement halt marker present at "
            f"{halt_marker}; refusing to dispatch new burst. Inspect "
            "the marker's JSON for diagnostics and clear_instructions; "
            "delete the file to resume."
        )
    # Per fail-loudly policy: every system_feedback emission pauses the run.
    system_feedback_marker = system_feedback_halt_marker_path(bridge_request.runtime_root)
    if system_feedback_marker.exists():
        raise BridgeError(
            "trellis: system_feedback halt marker present at "
            f"{system_feedback_marker}; refusing to dispatch new burst. "
            "An agent burst returned a non-empty system_feedback string; "
            "inspect the marker's JSON for diagnostics and "
            "clear_instructions; delete the file to resume."
        )

    config = load_config(bridge_request.config_path)
    _bridge_dir(bridge_request.runtime_root).mkdir(parents=True, exist_ok=True)
    _bridge_state_dir(config.repo_path, bridge_request.runtime_root).mkdir(parents=True, exist_ok=True)

    request = bridge_request.request

    # Phase 2 + Phase 3 (bwrap-only migration plan §3): per-burst token
    # plumbing + dispatch attribution log. Ordering:
    #   1. Mint a fresh URL-safe token (this subprocess is short-lived;
    #      a token per `handle_bridge_request` call gives the server's
    #      per-accept reload a tight registry footprint).
    #   2. Export it via os.environ BEFORE any subsequent call may build
    #      a bwrap command line — sandbox._passthrough_value_envs() reads
    #      from os.environ when wrap_command runs.
    #   3. Register the token on disk so the checker server (post-restart)
    #      sees it on its next accept(). Pre-restart the live server's
    #      legacy UID gate still admits the burst — the file is harmless
    #      to the old server which never reads it.
    #   4. Append the burst-dispatch.jsonl record (Phase 3) — forensic
    #      attribution that survives the UID-trail collapse Phase 4 will
    #      cause.
    burst_kind = _request_kind(request) or "unknown"
    burst_request_id = _request_id(request)
    burst_cycle = _request_cycle(request)
    burst_id = _burst_id_for_request(request)
    burst_token = _mint_burst_token()
    os.environ["TRELLIS_CHECKER_TOKEN"] = burst_token
    try:
        _register_burst_token(
            bridge_request.runtime_root,
            token=burst_token,
            burst_id=burst_id,
            kind=burst_kind,
            request_id=burst_request_id,
            cycle=burst_cycle,
        )
    except OSError:
        # Token registration is best-effort: when the runtime root is
        # not writable (highly unusual) the burst still proceeds — the
        # dormant server path admits it via UID and the post-restart
        # server's empty-registry fallback also admits it. A failure here
        # never blocks dispatch.
        pass
    _append_burst_dispatch_log(
        bridge_request.runtime_root,
        burst_id=burst_id,
        kind=burst_kind,
        request_id=burst_request_id,
        cycle=burst_cycle,
        bridge_pid=os.getpid(),
    )
    runtime_support_required = request.get("runtime_support_required")
    if not isinstance(runtime_support_required, bool):
        raise BridgeError("request is missing kernel-authored runtime_support_required")
    if runtime_support_required:
        _ensure_project_runtime_support(config)
    kind = _request_kind(request)
    if kind == "worker":
        return _handle_worker(config=config, runtime_root=bridge_request.runtime_root, request=request)
    if kind == "paper":
        return _handle_paper(config=config, runtime_root=bridge_request.runtime_root, request=request)
    if kind == "corr":
        return _handle_corr(config=config, runtime_root=bridge_request.runtime_root, request=request)
    if kind == "sound":
        return _handle_sound(config=config, runtime_root=bridge_request.runtime_root, request=request)
    if kind == "review":
        return _handle_review(config=config, runtime_root=bridge_request.runtime_root, request=request)
    if kind == "audit":
        # Cleanup-v2 (audit Finding 1): dispatch the audit-burst lane.
        return _handle_audit(config=config, runtime_root=bridge_request.runtime_root, request=request)
    if kind == "stuck_math_audit":
        return _handle_stuck_math_audit(
            config=config,
            runtime_root=bridge_request.runtime_root,
            request=request,
        )
    if kind == "human_gate":
        return _handle_human_gate(runtime_root=bridge_request.runtime_root, request=request)
    raise BridgeError(f"unsupported request kind: {kind}")
