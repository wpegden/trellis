"""Dumb Python bridge for the trellis supervisor runtime.

This module does not decide protocol behavior. It renders prompts from the
Rust-owned request payload, executes agents through the shared wrapper, and
maps validated raw outputs back into Rust-shaped responses.
"""

from __future__ import annotations

import hashlib
import json
import os
import random
import secrets
import sys
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
from trellis.json_io import append_jsonl, load_json, save_json, timestamp_now
from trellis.operation_capabilities import validate_commissioned_operation
from trellis.supervisor_workspace import (
    propagate_tablet_back_to_worker,
    sync_supervisor_workspace,
)
from trellis.worker_scratch import ensure_worker_scratch_workspace
from trellis.active_node_prewarm.supervisor_hook import (
    export_worker_burst_env,
    maybe_prewarm_active_node,
)

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
# Written by the wrapper on a non-empty system_feedback emission when
# system_feedback halting is enabled (opt-in: top-level
# `system_feedback_halt` in trellis.config.json or
# TRELLIS_SYSTEM_FEEDBACK_HALT=1; default is log-and-continue).
# Distinct marker filename from the checker-disagreement marker so an
# operator inspecting `<runtime_root>/` can tell the two halt causes
# apart at a glance. Same sticky-across-restart semantics; only
# operator deletion clears it. The bridge CHECK below is unconditional
# regardless of the knob — a marker on disk refuses new dispatch.
SYSTEM_FEEDBACK_HALT_MARKER_FILENAME = "system_feedback_halt.json"
# How long a minted token stays in the on-disk registry. Bursts are
# bounded by `policy.timing.burst_timeout_seconds` (typically a few
# hours); 12h gives generous headroom plus avoids permanent growth of
# the file across long runs. The bridge subprocess is short-lived so
# we don't keep an in-memory expiry timer; instead we GC expired
# entries on every write.
_BURST_TOKEN_TTL_SECONDS = 12 * 60 * 60
# Supervisor-lifetime tokens are minted once per `trellis.sh run`; there is no
# sound wall-clock upper bound on that run. Refresh the active supervisor entry
# whenever any burst is registered. Registering a new supervisor retires older
# supervisor entries in the same runtime root, so an old run's credential does
# not become a permanent exemption.


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
    two halt causes don't collide. The marker is only WRITTEN when
    system_feedback halting is enabled (opt-in knob), but its presence
    always pauses the run.
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
        is_supervisor = raw_entry.get("kind") == "supervisor"
        if is_supervisor and kind == "supervisor" and entry_token != token:
            continue
        if not is_supervisor and (
            not isinstance(entry_ts, (int, float)) or entry_ts < cutoff
        ):
            continue
        if entry_token in seen:
            continue
        seen.add(entry_token)
        retained = dict(raw_entry)
        if is_supervisor:
            retained["ts"] = now
        fresh_entries.append(retained)
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
        {
            # S4: stamp the owning runtime so a checker serving a DIFFERENT
            # runtime (a copied/seeded checker-state, or a legacy file predating
            # this field) ignores these tokens instead of flipping its auth gate
            # active against a token-less client (e.g. a manual prep → the
            # `auth_required` footgun). The matching checker honors them.
            "runtime_root": str(runtime_root.resolve()),
            "tokens": fresh_tokens,
            "entries": fresh_entries,
        },
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
    most importantly `.acceptance.json`, which records the immutable
    pre-burst normalization baseline. `staging/` is
    in `_repo_writable_paths` for every burst role (sandbox.py:181-186)
    so any worker could rewrite `staging/<...>.acceptance.json` between
    writing `.done`, poisoning the forensic baseline. This directory is
    intentionally NOT added to the writable
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


def _truthy(value: Any) -> bool:
    return str(value or "").strip().lower() in {"1", "true", "yes", "on"}


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


def _coerce_int(value: Any) -> Optional[int]:
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def _cached_worker_response_for_request(
    runtime_root: Path,
    request: Mapping[str, Any],
    *,
    repo_path: Path,
) -> Optional[Dict[str, Any]]:
    try:
        latest = _load_bridge_json(runtime_root, "latest_worker.json")
    except (OSError, json.JSONDecodeError, TypeError):
        return None
    if not isinstance(latest, Mapping):
        return None
    response = latest.get("response")
    if not isinstance(response, Mapping):
        return None
    if _request_kind(response) != "worker":
        return None
    if _coerce_int(response.get("request_id")) != _request_id(request):
        return None
    if _coerce_int(response.get("cycle")) != _request_cycle(request):
        return None
    # A Valid response is authoritative only for the exact source bytes the
    # checker normalized.  Node presence and semantic fingerprints do not
    # cover proof-body-only edits, which made a cached response silently
    # reusable after an operator/kernel worktree restore.  The snapshot is
    # runtime-authored in the cache envelope after normalization; it is not a
    # worker result field and cannot be supplied through the artifact
    # allowlist.
    if response.get("status") == "Ok" and response.get("outcome") == "Valid":
        expected = latest.get("tablet_snapshot")
        current = _tablet_snapshot(repo_path)
        if not isinstance(expected, Mapping) or dict(expected) != current:
            raise BridgeError(
                "stale worker replay refused: the cached Valid response is not bound "
                "to this exact Tablet byte snapshot (the binding is missing or the "
                "worktree changed after normalization); re-dispatch the worker against "
                "the current active_worker_base"
            )
    return dict(response)


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


WORKER_MODEL_AB_ENV = "TRELLIS_WORKER_MODEL_AB"
WORKER_MODEL_AB_A_MODEL_ENV = "TRELLIS_WORKER_MODEL_AB_A"
WORKER_MODEL_AB_B_MODEL_ENV = "TRELLIS_WORKER_MODEL_AB_B"
WORKER_MODEL_AB_B_PROB_ENV = "TRELLIS_WORKER_MODEL_AB_B_PROB"
WORKER_MODEL_AB_FLAG_NAME = ".trellis/worker_model_ab.enabled"
WORKER_MODEL_AB_CONFIG_NAME = ".trellis/worker_model_ab.json"
WORKER_MODEL_AB_LOG_NAME = ".trellis/logs/worker_model_ab.jsonl"
# Opt-in worker A/B arms (enabled by TRELLIS_WORKER_MODEL_AB or the
# `.trellis/worker_model_ab.enabled` flag; inert otherwise). Both arms must
# name a CURRENT model — the point is comparing two live options, and a
# stale arm silently spends a fraction of the run's bursts on a superseded
# generation. Kept as two distinct 5.6 variants so the harness still
# compares something: sol is the quality arm, terra the cheaper one.
DEFAULT_WORKER_MODEL_AB_A_MODEL = "gpt-5.6-terra"
DEFAULT_WORKER_MODEL_AB_B_MODEL = "gpt-5.6-sol"


@dataclass(frozen=True)
class _WorkerModelAbSettings:
    a_model: str
    b_model: str
    b_prob: float


@dataclass(frozen=True)
class _WorkerModelAbDecision:
    arm: str
    configured_model: str
    chosen_model: str
    a_model: str
    b_model: str
    b_prob: float


def _nonempty_str(value: Any) -> str:
    return str(value or "").strip()


def _clamped_probability(value: Any, *, default: float) -> float:
    try:
        return min(1.0, max(0.0, float(value)))
    except (TypeError, ValueError):
        return default


def _load_worker_model_ab_config(work_dir: Path) -> tuple[Dict[str, Any], bool]:
    path = Path(work_dir) / WORKER_MODEL_AB_CONFIG_NAME
    try:
        if not path.exists():
            return {}, False
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return {}, False
    if not isinstance(data, dict):
        return {}, False
    return dict(data), True


def _worker_model_ab_flag_enabled(work_dir: Path) -> bool:
    try:
        return (Path(work_dir) / WORKER_MODEL_AB_FLAG_NAME).exists()
    except OSError:
        return False


def _worker_model_ab_settings(
    work_dir: Path,
    *,
    configured_model: Optional[str],
) -> Optional[_WorkerModelAbSettings]:
    file_config, config_exists = _load_worker_model_ab_config(work_dir)
    config_enables = False
    if config_exists:
        raw_enabled = file_config.get("enabled")
        config_enables = True if raw_enabled is None else _truthy(raw_enabled)
    enabled = (
        _truthy(os.environ.get(WORKER_MODEL_AB_ENV, ""))
        or _worker_model_ab_flag_enabled(work_dir)
        or config_enables
    )
    if not enabled:
        return None

    a_model = (
        _nonempty_str(os.environ.get(WORKER_MODEL_AB_A_MODEL_ENV))
        or _nonempty_str(file_config.get("a_model"))
        or DEFAULT_WORKER_MODEL_AB_A_MODEL
    )
    b_model = (
        _nonempty_str(os.environ.get(WORKER_MODEL_AB_B_MODEL_ENV))
        or _nonempty_str(file_config.get("b_model"))
        or _nonempty_str(configured_model)
        or DEFAULT_WORKER_MODEL_AB_B_MODEL
    )
    if not a_model or not b_model or a_model == b_model:
        return None

    raw_b_prob = os.environ.get(WORKER_MODEL_AB_B_PROB_ENV)
    if _nonempty_str(raw_b_prob):
        b_prob = _clamped_probability(raw_b_prob, default=0.5)
    else:
        b_prob = _clamped_probability(file_config.get("b_prob"), default=0.5)
    return _WorkerModelAbSettings(a_model=a_model, b_model=b_model, b_prob=b_prob)


def _maybe_apply_worker_model_ab(
    *,
    work_dir: Path,
    provider: ProviderConfig,
) -> tuple[ProviderConfig, Optional[_WorkerModelAbDecision]]:
    if str(provider.provider or "").strip().lower() != "codex":
        return provider, None
    configured_model = _nonempty_str(provider.model)
    settings = _worker_model_ab_settings(
        work_dir,
        configured_model=configured_model or None,
    )
    if settings is None:
        return provider, None

    arm = "b" if random.random() < settings.b_prob else "a"
    chosen_model = settings.b_model if arm == "b" else settings.a_model
    effective_provider = ProviderConfig(
        provider=provider.provider,
        model=chosen_model,
        effort=provider.effort,
        extra_args=list(provider.extra_args or []),
        fallback_models=list(provider.fallback_models or []),
    )
    return effective_provider, _WorkerModelAbDecision(
        arm=arm,
        configured_model=configured_model,
        chosen_model=chosen_model,
        a_model=settings.a_model,
        b_model=settings.b_model,
        b_prob=settings.b_prob,
    )


def _record_worker_model_ab(
    *,
    work_dir: Path,
    request: Mapping[str, Any],
    decision: _WorkerModelAbDecision,
    artifact_prefix: str,
    session_scope: str,
) -> None:
    try:
        append_jsonl(
            Path(work_dir) / WORKER_MODEL_AB_LOG_NAME,
            {
                "timestamp": timestamp_now(),
                "ts_ms": int(time.time() * 1000),
                "request_id": _request_id(request),
                "cycle": _request_cycle(request),
                "phase": _phase_name(request),
                "burst_prefix": str(artifact_prefix or ""),
                "arm": decision.arm,
                "configured_model": decision.configured_model,
                "chosen_model": decision.chosen_model,
                "a_model": decision.a_model,
                "b_model": decision.b_model,
                "b_prob": decision.b_prob,
                "session_scope": str(session_scope or ""),
            },
        )
    except OSError:
        pass


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
    # Namespace by the RUN SLUG (the runtime root's own name with a trailing
    # `-runtime` stripped, e.g. `connectivity-isa-runtime` -> `connectivity-isa`),
    # NOT by `runtime_root.parent.name`. Every run lives under `~/math/`, so the
    # parent name is the literal `math` for every run, making every run's burst
    # tmux session byte-identical (`trellis-math-worker-1-worker`) on the shared
    # `tmux -L trellis` server. That let one run's tmux sweep/kill destroy another
    # run's live worker. The run slug matches the PROJECT_SLUG convention used by
    # the run/checker session names elsewhere.
    raw = runtime_root.name.removesuffix("-runtime") or runtime_root.name or "runtime"
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


def _verifier_artifact_retry_budget() -> int:
    """Bounded consecutive-missing-artifact budget per verifier kind before
    the bridge escalates to a clean halt (vs re-dispatching forever).
    Overridable via `TRELLIS_VERIFIER_ARTIFACT_RETRY_BUDGET`.

    Off-by-one (round-2 clarification): the budget is the number of
    consecutive missing-artifact failures the bridge tolerates WITHOUT
    tripping the breaker. With the default 3, failures at streak 1, 2, 3 each
    return Malformed (kernel re-dispatches) and do NOT write the halt
    sentinel; the FIRST failure past the budget — streak 4, i.e.
    ``streak > budget`` — writes the sentinel (it still returns Malformed for
    that cycle, so the run halts at the next checkpoint boundary rather than
    mid-burst). So budget=N ⇒ N clean retries, breaker on attempt N+1.
    budget=0 ⇒ the first failure trips the breaker immediately."""
    raw = os.environ.get("TRELLIS_VERIFIER_ARTIFACT_RETRY_BUDGET", "").strip()
    if raw:
        try:
            return max(0, int(raw))
        except ValueError:
            pass
    return 3


def _maybe_malformed_verifier_response(
    *,
    kind: str,
    runtime_root: Path,
    config: Config,
    request: Mapping[str, Any],
    raw: Any,
) -> Optional[Dict[str, Any]]:
    """Bug 1 (live-run incident 2026-06-26): a verifier panel member that
    finished WITHOUT a usable result artifact (``ok=False`` — missing or
    unparseable ``…raw.json``) must be RETRYABLE, not fatal.

    A genuine Pass/Reject verdict is ``ok=True`` with the verdict inside the
    payload, so ``ok=False`` is always a transport / no-artifact failure.
    Previously such a member was fed straight to the kernel normalizer, whose
    ``collect_members`` ``Err``s ("lane … failed: … not found") → BridgeError
    → adapter error → fatal ``runtime step failed`` → the whole supervisor
    exited. Instead we return a transport-flavored Malformed verifier
    response. The engine's ``apply_{paper,corr,sound}_response`` Malformed
    branch re-issues the same verifier request next cycle (a fresh burst),
    mirroring the worker Malformed re-dispatch, so a transient verifier flake
    self-heals instead of taking the run down.

    Bounded: a per-``kind`` consecutive-failure streak is persisted in the
    bridge state dir. While at or under budget (``streak <= budget``) the
    bridge returns Malformed (the kernel retries). On the first failure PAST
    the budget (``streak > budget``; with the default budget 3 that is the 4th
    consecutive failure) it ALSO writes the kernel circuit-breaker sentinel
    (``.trellis-stop-after-checkpoint``) so the run halts cleanly at the next
    checkpoint boundary for operator triage instead of re-dispatching forever.
    A panel that produced ALL artifacts clears the streak.

    Returns the Malformed response dict to short-circuit normalization, or
    ``None`` when every lane produced an artifact (proceed normally).
    """
    failed = [
        member
        for member in getattr(raw, "member_responses", [])
        if not bool(getattr(member, "ok", True))
    ]
    counters = _load_bridge_json(runtime_root, "verifier_artifact_retry.json")
    if not isinstance(counters, dict):
        counters = {}
    if not failed:
        if counters.pop(kind, None) is not None:
            _save_bridge_json(runtime_root, "verifier_artifact_retry.json", counters)
        return None
    streak = int(counters.get(kind, 0) or 0) + 1
    counters[kind] = streak
    _save_bridge_json(runtime_root, "verifier_artifact_retry.json", counters)
    errors = "; ".join(
        f"{getattr(m, 'request_id', '?')}: {getattr(m, 'error', '') or 'no artifact'}"
        for m in failed
    )
    budget = _verifier_artifact_retry_budget()
    print(
        f"trellis-bridge: {kind} verifier produced no result artifact for "
        f"{len(failed)} lane(s) (streak {streak}/{budget}): {errors}. "
        f"Returning Malformed so the kernel re-dispatches the verifier.",
        file=sys.stderr,
    )
    if streak > budget:
        try:
            sentinel = config.repo_path / ".trellis-stop-after-checkpoint"
            sentinel.write_text(
                f"[bridge circuit-breaker] {kind} verifier produced no result "
                f"artifact for {streak} consecutive bursts (budget {budget}). "
                f"Last errors: {errors}\n",
                encoding="utf-8",
            )
            print(
                f"trellis-bridge: wrote halt sentinel {sentinel} after {streak} "
                f"consecutive {kind} verifier artifact failures.",
                file=sys.stderr,
            )
        except OSError as exc:
            print(
                f"trellis-bridge: failed to write halt sentinel: {exc}",
                file=sys.stderr,
            )
    return _build_malformed_response(kind=kind, request=request)


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
    rust_witness_artifact_correspondence: Any = None,
    conditional_theorem_correspondence: Any = None,
) -> Dict[str, Any]:
    return _run_kernel_normalize(
        "normalize_corr",
        request_id=request_id,
        cycle=cycle,
        verify_lanes=_sorted_strs(verify_lanes),
        verify_nodes=_sorted_strs(verify_nodes),
        verify_targets=_sorted_strs(verify_targets),
        rust_witness_artifact_correspondence=rust_witness_artifact_correspondence,
        conditional_theorem_correspondence=conditional_theorem_correspondence,
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


def _worker_system_feedback(response: Any) -> str:
    try:
        raw = _load_raw_response_json(response)
    except BridgeError:
        return ""
    return str(raw.get("system_feedback", "") or "").strip()


def _worker_rejection_codes(messages: Sequence[Any]) -> List[str]:
    codes: List[str] = []
    for message in messages:
        text = str(message or "").strip()
        if text.startswith("[") and "]" in text:
            code = text[1 : text.index("]")].strip()
            if code and code not in codes:
                codes.append(code)
    return codes


def _annotate_worker_rejection(
    normalized_result: Mapping[str, Any],
    *,
    request: Mapping[str, Any],
    system_feedback: str,
) -> Dict[str, Any]:
    result = dict(normalized_result)
    response = result.get("response")
    if not isinstance(response, Mapping):
        return result
    final_outcome = str(result.get("final_outcome", "") or "").strip().lower()
    if final_outcome != "invalid":
        return result

    rewritten_response = dict(response)
    existing = [
        str(item)
        for item in rewritten_response.get("deterministic_rejection_reasons", []) or []
    ]
    contract_errors = [str(item) for item in result.get("contract_errors", []) or []]
    validation_errors = [str(item) for item in result.get("validation_errors", []) or []]
    errors = [str(item) for item in result.get("errors", []) or []]
    if any(item.startswith(_CHECKER_MISMATCH_PREFIX) for item in existing + errors):
        kind = "checker_contradiction"
        phase = "supervisor_cross_check"
    elif contract_errors:
        kind = "contract_violation"
        phase = "response_normalization"
    elif validation_errors:
        kind = "implementation_rejected"
        phase = "semantic_acceptance"
    else:
        kind = str(rewritten_response.get("invalid_kind", "") or "").strip()
        if not kind:
            kind = "worker_declared_invalid"
        phase = "worker_report"
    codes = _worker_rejection_codes(
        contract_errors + validation_errors + errors + existing
    )
    header = (
        f"[worker_rejection] kind={kind} phase={phase} "
        f"codes={json.dumps(codes)}"
    )
    identity = str(request.get("acceptance_logic_identity", "") or "").strip()
    reasons = [header]
    if identity:
        reasons.append(f"[acceptance_logic_identity] {identity}")
    if system_feedback:
        reasons.append(f"[worker_system_feedback] {system_feedback}")
    reasons.extend(existing)
    rewritten_response["deterministic_rejection_reasons"] = list(dict.fromkeys(reasons))
    result["response"] = rewritten_response
    return result


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
    kernel_failure: bool = False,
) -> Dict[str, Any]:
    """Persist a malformed worker result.

    Bug X principled fix: `transport_failure=True` flags this as an
    infrastructure failure (agent never produced output, timeout, rate-limit
    retries exhausted, etc.) so the kernel routes the rejection through the
    `RetryOutcomeKind::Transport` path — distinct from a worker that
    actually ran but emitted bad JSON.
    """
    malformed = _build_malformed_response(kind="worker", request=request)
    if kernel_failure:
        rejection_kind = "kernel_failure"
        rejection_phase = "kernel_validation"
    elif transport_failure:
        rejection_kind = "transport_failure"
        rejection_phase = "agent_transport"
    else:
        rejection_kind = "malformed_output"
        rejection_phase = "artifact_validation"
    codes = _worker_rejection_codes(
        list(contract_errors or [])
        + list(validation_errors or [])
        + list(errors)
    )
    reasons = [
        f"[worker_rejection] kind={rejection_kind} phase={rejection_phase} "
        f"codes={json.dumps(codes)}"
    ]
    identity = str(request.get("acceptance_logic_identity", "") or "").strip()
    if identity:
        reasons.append(f"[acceptance_logic_identity] {identity}")
    feedback = ""
    if isinstance(raw_payload, Mapping):
        feedback = str(raw_payload.get("system_feedback", "") or "").strip()
    if feedback:
        reasons.append(f"[worker_system_feedback] {feedback}")
    reasons.extend(str(item) for item in errors)
    malformed["deterministic_rejection_reasons"] = list(dict.fromkeys(reasons))
    if transport_failure or kernel_failure:
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


def _save_worker_normalization_failure(
    *,
    runtime_root: Path,
    request: Mapping[str, Any],
    raw_payload: Mapping[str, Any] | None,
    acceptance_context_path: Path | None,
    authoritative_repo_path: Path | None,
    normalized_result: Mapping[str, Any],
) -> Dict[str, Any]:
    """Classify a missing normalized response without blaming the worker.

    The checker wrapper reports kernel-process faults with stable diagnostic
    prefixes. Those failures mean the worker artifact was never adjudicated,
    so route them through the existing infrastructure retry bit and label the
    rejection as a kernel-validation failure. Ordinary contract/validation
    errors remain worker-attributable.
    """
    errors = [str(item) for item in normalized_result.get("errors", [])]
    kernel_failure_prefixes = (
        "kernel CLI failed:",
        "unexpected kernel response status:",
        "kernel response is missing output",
    )
    kernel_failure = any(
        error.startswith(kernel_failure_prefixes) for error in errors
    )
    return _save_malformed_worker_result(
        runtime_root=runtime_root,
        request=request,
        raw_payload=raw_payload,
        acceptance_context_path=acceptance_context_path,
        authoritative_repo_path=authoritative_repo_path,
        errors=errors,
        validation_errors=list(normalized_result.get("validation_errors", [])),
        contract_errors=list(normalized_result.get("contract_errors", [])),
        kernel_failure=kernel_failure,
    )


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
    # `.acceptance.json` lives outside the worker-writable staging allowlist
    # so the immutable pre-burst baseline remains trustworthy for live
    # normalization and postmortem diagnosis. See _bridge_private_state_dir.
    acceptance_context_path = private_dir / canonical_name.replace(
        ".json",
        ".acceptance.json",
    )
    authoritative_repo = config.repo_path.resolve()
    # Replay is decided before any sync or acceptance-context rebuild.  A
    # normalized cache carries an exact post-burst Tablet snapshot and is
    # reusable only while those bytes remain on disk.  A bare `.done` has no
    # corresponding post-burst binding and therefore fails closed below.
    try:
        cached_response = _cached_worker_response_for_request(
            runtime_root,
            request,
            repo_path=config.repo_path,
        )
    except BridgeError as exc:
        return _save_malformed_worker_result(
            runtime_root=runtime_root,
            request=request,
            raw_payload=None,
            acceptance_context_path=(
                acceptance_context_path if acceptance_context_path.exists() else None
            ),
            authoritative_repo_path=authoritative_repo,
            errors=[str(exc)],
            transport_failure=True,
        )
    if cached_response is not None:
        return cached_response
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
        # `.done` authenticates completion, not the post-burst source bytes.
        # The saved acceptance context authenticates only the PRE-burst
        # baseline.  If the kernel restored active_worker_base after `.done`
        # was written (including the inter-rename crash window), normalizing
        # now would describe those restored bytes and silently erase a
        # content-only worker edit.  Only latest_worker.json, written after
        # normalization with an exact Tablet snapshot, is replayable.  A bare
        # `.done` therefore fails closed as transport and lets the kernel
        # restore/re-dispatch.
        return _save_malformed_worker_result(
            runtime_root=runtime_root,
            request=request,
            raw_payload=recovered.payload,
            acceptance_context_path=(
                acceptance_context_path if acceptance_context_path.exists() else None
            ),
            authoritative_repo_path=authoritative_repo,
            errors=[
                "recovery: done marker has no authenticated post-burst source snapshot; "
                "refusing to re-normalize it because the worktree may have been restored "
                "after completion"
            ],
            transport_failure=True,
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
            goal_prose_path=getattr(config, "goal_file", None),
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
    worker_provider, worker_model_ab_decision = _maybe_apply_worker_model_ab(
        work_dir=config.repo_path,
        provider=_worker_provider_for_request(request),
    )
    single = _single_request_common(
        config=config,
        runtime_root=runtime_root,
        request=request,
        provider=worker_provider,
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
    if worker_model_ab_decision is not None:
        _record_worker_model_ab(
            work_dir=config.repo_path,
            request=request,
            decision=worker_model_ab_decision,
            artifact_prefix=str(single.artifact_prefix or ""),
            session_scope=single.session_scope,
        )
    # By this point there is no cached response and no `.done` artifact; the
    # worker has not produced replayable output, so run the burst.
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
        normalized_result = _annotate_worker_rejection(
            normalized_result,
            request=request,
            system_feedback=_worker_system_feedback(response),
        )
    except BridgeError as exc:
        normalized_result = {"errors": [str(exc)]}
    normalized = normalized_result.get("response")
    if not isinstance(normalized, dict):
        return _save_worker_normalization_failure(
            runtime_root=runtime_root,
            request=request,
            raw_payload=raw_payload,
            acceptance_context_path=acceptance_context_path,
            authoritative_repo_path=authoritative_repo,
            normalized_result=normalized_result,
        )
    final_outcome = str(normalized_result.get("final_outcome", "") or "").strip().lower()
    normalized["kind"] = "worker"
    _save_bridge_json(
        runtime_root,
        "latest_worker.json",
        {
            "raw": raw_payload,
            "response": normalized,
            "tablet_snapshot": _tablet_snapshot(config.repo_path),
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
    malformed = _maybe_malformed_verifier_response(
        kind="corr",
        runtime_root=runtime_root,
        config=config,
        request=request,
        raw=raw,
    )
    if malformed is not None:
        return malformed
    normalized = _normalize_corr_via_kernel(
        request_id=_request_id(request),
        cycle=_request_cycle(request),
        verify_lanes=prompt_request.get("verify_lanes", []),
        verify_nodes=prompt_request.get("verify_nodes", []),
        verify_targets=prompt_request.get("verify_targets", []),
        rust_witness_artifact_correspondence=prompt_request.get(
            "rust_witness_artifact_correspondence"
        ),
        conditional_theorem_correspondence=prompt_request.get(
            "conditional_theorem_correspondence"
        ),
        raw_panel=raw,
        request_members=member_map,
    )
    corr_payload = raw.to_dict()
    corr_payload["normalized"] = normalized
    _save_bridge_json(runtime_root, "latest_corr.json", corr_payload)
    response = {
        "kind": "corr",
        "request_id": _request_id(request),
        "cycle": _request_cycle(request),
        "status": "Ok",
        "node_lane_updates": normalized.get("node_lane_updates", {}),
        "target_lane_updates": normalized.get("target_lane_updates", {}),
        "reviewer_evidence": normalized.get("reviewer_evidence", {}),
    }
    if normalized.get("rust_witness_artifact_correspondence") is not None:
        response["rust_witness_artifact_correspondence"] = normalized[
            "rust_witness_artifact_correspondence"
        ]
    if normalized.get("conditional_theorem_correspondence") is not None:
        response["conditional_theorem_correspondence"] = normalized[
            "conditional_theorem_correspondence"
        ]
    return response


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
        malformed = _maybe_malformed_verifier_response(
            kind="paper",
            runtime_root=runtime_root,
            config=config,
            request=request,
            raw=raw,
        )
        if malformed is not None:
            return malformed
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
    malformed = _maybe_malformed_verifier_response(
        kind="sound",
        runtime_root=runtime_root,
        config=config,
        request=request,
        raw=raw,
    )
    if malformed is not None:
        return malformed
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


_PREAMBLE_NODE = "Preamble"
_ASSUMPTIONS_NODE = "Assumptions"

# `Phase` variants in which the kernel's `current_substantiveness_state` runs
# the lane; every other phase short-circuits to Pass outside the Cleanup
# substitution scope.
_SUBSTANTIVENESS_ACTIVE_PHASES = frozenset(
    {"TheoremStating", "RevisionStating", "ProofFormalization"}
)

# `SoundStatus` -> `SoundAssessmentStatus`, as `legacy_sound_assessment` maps it.
_LEGACY_SOUND_ASSESSMENT_STATUS = {
    "Unknown": "FreshUnknown",
    "Pass": "VerifierPass",
    "Fail": "VerifierFail",
    "Structural": "VerifierStructural",
}

# `SoundAssessmentStatus` groupings. `_SOUND_FAIL_STATUSES` and
# `_SOUND_PASS_STATUSES` are the `current_sound_state` arms; the drift sets are
# the arms `current_sound_assessment` takes when a fingerprint part moves.
_SOUND_PASS_STATUSES = frozenset({"VerifierPass"})
_SOUND_FAIL_STATUSES = frozenset(
    {
        "VerifierFail",
        "VerifierStructural",
        "ReviewerPinnedFail",
        "SketchAutoFail",
        "DepEditOnlyStaleFail",
    }
)
_SOUND_DRIFT_PASS_STATUSES = frozenset(
    {"VerifierPass", "ReviewerAcceptedPass", "DepEditOnlyStalePassDeferred"}
)
_SOUND_DEP_DRIFT_CARRIED_STATUSES = frozenset(
    {"FreshUnknown", "SelfEditUnknown", "SplitUnknown"}
)
_SOUND_COMBINED_DRIFT_FAIL_STATUSES = frozenset(
    {
        "VerifierFail",
        "VerifierStructural",
        "ReviewerPinnedFail",
        "DepEditOnlyStaleFail",
    }
)


class _MalformedProtocolState(Exception):
    """A `protocol_state.json` field holds a type the kernel never writes."""


def _protocol_state_names(value: Any) -> set:
    """A kernel `BTreeSet<NodeId>` / `Vec<NodeId>` field as a set of names.

    A missing field is the kernel's empty default. Any other shape — a bare
    string above all, which iterates into one entry per character — is
    malformed, and the caller degrades the whole record rather than inventing
    node names.
    """
    if value is None:
        return set()
    if isinstance(value, (list, tuple, set, frozenset)):
        return {str(item) for item in value}
    raise _MalformedProtocolState(repr(type(value)))


def _protocol_state_name_list(value: Any) -> List[str]:
    if value is None:
        return []
    if isinstance(value, (list, tuple)):
        return [str(item) for item in value]
    raise _MalformedProtocolState(repr(type(value)))


def _protocol_state_mapping(value: Any) -> Mapping[str, Any]:
    if value is None:
        return {}
    if isinstance(value, Mapping):
        return value
    raise _MalformedProtocolState(repr(type(value)))


class _KernelLaneStates:
    """Port of the kernel's per-node lane predicates over a state snapshot.

    Mirrors `current_corr_state`, `current_substantiveness_state` and
    `current_sound_assessment` in kernel/src/model.rs, each with the
    short-circuits it takes ahead of the stored-status-plus-fingerprint tail.
    """

    def __init__(self, state: Mapping[str, Any]) -> None:
        live = _protocol_state_mapping(state.get("live"))
        self.phase = str(state.get("phase") or "")
        self.present_node_order = _protocol_state_name_list(live.get("present_nodes"))
        self.present_nodes = set(self.present_node_order)
        self.open_nodes = _protocol_state_names(live.get("open_nodes"))
        self.proof_nodes = _protocol_state_names(state.get("proof_nodes"))
        self.sketch_proof_nodes = _protocol_state_names(live.get("sketch_proof_nodes"))
        self.placeholder_definition_nodes = _protocol_state_names(
            live.get("placeholder_definition_nodes")
        )

        self.closure_records = _protocol_state_mapping(state.get("local_closure_records"))
        self.closure_failures = _protocol_state_mapping(state.get("local_closure_failures"))
        self.closure_unverified = _protocol_state_names(
            state.get("local_closure_unverified_nodes")
        )

        self.node_role = _protocol_state_mapping(state.get("node_role"))
        self.challenge_claims = _protocol_state_mapping(state.get("challenge_claims"))
        self.configured_challenge_targets = _protocol_state_mapping(
            state.get("configured_challenge_targets")
        )
        self.pv_tablet_configured = bool(state.get("pv_tablet_configured"))
        self.pending_assumption_draft = state.get("pending_assumption_draft")

        self.corr_status = _protocol_state_mapping(state.get("corr_status"))
        self.corr_approved = _protocol_state_mapping(state.get("corr_approved_fingerprints"))
        self.corr_current = _protocol_state_mapping(live.get("corr_current_fingerprints"))

        self.substantiveness_status = _protocol_state_mapping(
            state.get("substantiveness_status")
        )
        self.substantiveness_approved = _protocol_state_mapping(
            state.get("substantiveness_approved_fingerprints")
        )
        self.substantiveness_current = _protocol_state_mapping(
            live.get("substantiveness_current_fingerprints")
        )

        self.node_deviation_claims = _protocol_state_mapping(
            state.get("node_deviation_claims")
        )
        self.deviation_status = _protocol_state_mapping(state.get("deviation_status"))
        self.deviation_approved = _protocol_state_mapping(
            state.get("deviation_approved_fingerprints")
        )
        self.deviation_current = _protocol_state_mapping(
            live.get("deviation_current_fingerprints")
        )

        self.sound_assessments = _protocol_state_mapping(state.get("sound_assessments"))
        self.sound_status = _protocol_state_mapping(state.get("sound_status"))
        self.sound_approved = _protocol_state_mapping(state.get("sound_approved_fingerprints"))
        self.sound_current = _protocol_state_mapping(live.get("sound_current_fingerprints"))
        self.sound_current_parts = _protocol_state_mapping(
            live.get("sound_current_fingerprint_parts")
        )

        self.cleanup_substantiveness_scope = self._cleanup_substantiveness_scope(state)

    # ----- shared short-circuits -------------------------------------------

    def _cleanup_substantiveness_scope(self, state: Mapping[str, Any]) -> set:
        if self.phase != "Cleanup":
            return set()
        index = state.get("cleanup_active_task")
        if not isinstance(index, int) or isinstance(index, bool):
            return set()
        tasks = state.get("cleanup_audit_tasks")
        if not isinstance(tasks, (list, tuple)) or not 0 <= index < len(tasks):
            return set()
        task = tasks[index]
        if not isinstance(task, Mapping):
            return set()
        kind = _protocol_state_mapping(task.get("kind"))
        if str(kind.get("kind") or "") != "substitution":
            return set()
        pending = state.get("pending_task")
        scope = set()
        if isinstance(pending, Mapping):
            scope = _protocol_state_names(pending.get("authorized_nodes"))
        scope.add(str(task.get("target_node") or ""))
        return scope

    def _pv_review_suppressed(self, node: str) -> bool:
        return str(self.node_role.get(node) or "") == "ExtractionModel"

    def _lane_waived(self, node: str) -> bool:
        """`correspondence_waived` / `substantiveness_waived`: identical
        predicates over `challenge_claims`, D6-re-keyed on statement
        provenance — waived iff the node claims >=1 target and NO claimed
        target's spec is `worker_authored` (the kernel's
        `challenge_claim_waives_lanes`; absent spec / absent field default
        to seed_pinned)."""
        claims = self.challenge_claims.get(node)
        if not (isinstance(claims, (list, tuple, set, frozenset)) and len(claims) > 0):
            return False
        for target in claims:
            spec = self.configured_challenge_targets.get(str(target))
            provenance = (
                str(spec.get("statement_provenance") or "seed_pinned")
                if isinstance(spec, Mapping)
                else "seed_pinned"
            )
            if provenance == "worker_authored":
                return False
        return True

    def _is_under_model_assumptions_node(self, node: str) -> bool:
        return (
            self.pv_tablet_configured
            and node == _ASSUMPTIONS_NODE
            and str(self.node_role.get(node) or "") == "UnderModelAssumptions"
        )

    @staticmethod
    def _fingerprint_state(
        status: Any,
        current: Any,
        approved: Any,
        *,
        allow_empty_fingerprint: bool,
    ) -> str:
        if current is None or approved is None or current != approved:
            return "?"
        if not allow_empty_fingerprint and current == "":
            return "?"
        status = str(status or "")
        if status == "Pass":
            return "pass"
        if status == "Fail":
            return "fail"
        return "?"

    # ----- Deviation lane ---------------------------------------------------

    def _deviation_state(self, deviation_id: str) -> str:
        return self._fingerprint_state(
            self.deviation_status.get(deviation_id),
            self.deviation_current.get(deviation_id),
            self.deviation_approved.get(deviation_id),
            allow_empty_fingerprint=False,
        )

    def _node_deviation_states(self, node: str) -> List[str]:
        claims = _protocol_state_names(self.node_deviation_claims.get(node))
        return [self._deviation_state(claim) for claim in sorted(claims)]

    # ----- Correspondence ---------------------------------------------------

    def corr_state(self, node: str) -> str:
        if node not in self.present_nodes:
            return "?"
        if self._pv_review_suppressed(node) or self._lane_waived(node):
            return "pass"
        if node in self.placeholder_definition_nodes:
            return "fail"
        bootstrap_empty = (
            self._is_under_model_assumptions_node(node)
            and self.pending_assumption_draft is None
        )
        if bootstrap_empty and str(self.corr_status.get(node) or "") == "Pass":
            current = self.corr_current.get(node)
            approved = self.corr_approved.get(node)
            if (current or "") == "" and (approved or "") == "":
                return "pass"
        return self._fingerprint_state(
            self.corr_status.get(node),
            self.corr_current.get(node),
            self.corr_approved.get(node),
            allow_empty_fingerprint=(node == _PREAMBLE_NODE or bootstrap_empty),
        )

    # ----- Substantiveness --------------------------------------------------

    def substantiveness_state(self, node: str) -> str:
        if node not in self.present_nodes:
            return "?"
        if self._pv_review_suppressed(node) or self._lane_waived(node):
            return "pass"
        scope_active = (
            self.phase == "Cleanup" and node in self.cleanup_substantiveness_scope
        )
        if self.phase not in _SUBSTANTIVENESS_ACTIVE_PHASES and not scope_active:
            return "pass"
        if node == _PREAMBLE_NODE or self._is_under_model_assumptions_node(node):
            return "pass"
        deviation_states = self._node_deviation_states(node)
        if "fail" in deviation_states:
            return "fail"
        if any(state != "pass" for state in deviation_states):
            return "?"
        return self._fingerprint_state(
            self.substantiveness_status.get(node),
            self.substantiveness_current.get(node),
            self.substantiveness_approved.get(node),
            allow_empty_fingerprint=True,
        )

    # ----- Soundness --------------------------------------------------------

    def needs_sound(self, node: str) -> bool:
        return (
            node in self.present_nodes
            and node in self.open_nodes
            and node in self.proof_nodes
        )

    def _current_sound_parts(self, node: str) -> Dict[str, Any]:
        parts = self.sound_current_parts.get(node)
        if isinstance(parts, Mapping):
            return {
                "own_tex_hash": parts.get("own_tex_hash") or "",
                "dep_statement_hashes": _protocol_state_mapping(
                    parts.get("dep_statement_hashes")
                ),
                "combined_sound_fp": parts.get("combined_sound_fp") or "",
            }
        return {
            "own_tex_hash": "",
            "dep_statement_hashes": {},
            "combined_sound_fp": self.sound_current.get(node) or "",
        }

    def stored_sound_assessment(self, node: str) -> Optional[Dict[str, Any]]:
        """`sound_assessments[node]`, falling back to `legacy_sound_assessment`."""
        stored = self.sound_assessments.get(node)
        if isinstance(stored, Mapping):
            fingerprints = _protocol_state_mapping(stored.get("fingerprints"))
            return {
                "status": str(stored.get("status") or ""),
                "own_tex_hash": fingerprints.get("own_tex_hash") or "",
                "dep_statement_hashes": _protocol_state_mapping(
                    fingerprints.get("dep_statement_hashes")
                ),
                "combined_sound_fp": fingerprints.get("combined_sound_fp") or "",
            }
        status = self.sound_status.get(node)
        current = self.sound_current.get(node)
        approved = self.sound_approved.get(node)
        if status is None or current is None or approved is None or current != approved:
            return None
        legacy = _LEGACY_SOUND_ASSESSMENT_STATUS.get(str(status))
        if legacy is None:
            return None
        parts = self._current_sound_parts(node)
        return {
            "status": legacy,
            "own_tex_hash": parts["own_tex_hash"],
            "dep_statement_hashes": parts["dep_statement_hashes"],
            "combined_sound_fp": parts["combined_sound_fp"],
        }

    def sound_assessment_status(self, node: str) -> str:
        if not self.needs_sound(node):
            return "VerifierPass"
        if node in self.sketch_proof_nodes:
            return "SketchAutoFail"
        stored = self.stored_sound_assessment(node)
        if stored is None:
            return "FreshUnknown"
        status = stored["status"]
        current = self._current_sound_parts(node)
        if (
            current["own_tex_hash"]
            and current["own_tex_hash"] != stored["own_tex_hash"]
        ):
            return "SelfEditUnknown"
        if (
            current["dep_statement_hashes"]
            and stored["dep_statement_hashes"]
            and current["dep_statement_hashes"] != stored["dep_statement_hashes"]
        ):
            if status in _SOUND_DRIFT_PASS_STATUSES:
                return "DepEditOnlyStalePassDeferred"
            if status in _SOUND_DEP_DRIFT_CARRIED_STATUSES:
                return status
            return "DepEditOnlyStaleFail"
        if (
            current["combined_sound_fp"]
            and stored["combined_sound_fp"]
            and current["combined_sound_fp"] != stored["combined_sound_fp"]
        ):
            if status in _SOUND_DRIFT_PASS_STATUSES:
                return "DepEditOnlyStalePassDeferred"
            if status in _SOUND_COMBINED_DRIFT_FAIL_STATUSES:
                return "DepEditOnlyStaleFail"
            return "SelfEditUnknown"
        return status

    def sound_state(self, node: str) -> str:
        """`current_sound_state`, with the `!needs_sound` synthetic
        VerifierPass split out under its own token."""
        if not self.needs_sound(node):
            return "not_required"
        status = self.sound_assessment_status(node)
        if status in _SOUND_PASS_STATUSES:
            return "pass"
        if status in _SOUND_FAIL_STATUSES:
            return "fail"
        return "?"

    # ----- Lean closure -----------------------------------------------------

    def lean_closure_state(self, node: str) -> Optional[str]:
        if node not in self.proof_nodes:
            return None
        if node in self.open_nodes:
            return "open"
        if node in self.closure_failures:
            return "failed"
        if node in self.closure_unverified:
            return "unverified"
        if node in self.closure_records:
            return "closed"
        return "unverified"


def _stuck_math_audit_node_lane_verdicts(runtime_root: Path) -> Dict[str, Any]:
    """Per-node lane verdicts for the StuckMathAudit request context.

    `available` reports whether the supervisor's `protocol_state.json` was read;
    it is `false` on an absent, unreadable, or malformed file, and the maps are
    then empty. The maps hold the verdict the kernel holds right now, through a
    port of `current_corr_state`, `current_substantiveness_state` and
    `current_sound_assessment` in kernel/src/model.rs:

    * `lean_closure`, keyed by proof node — `open` (`live.open_nodes`), `failed`
      (a `local_closure_failures` summary), `closed` (a `local_closure_records`
      entry), or `unverified`. Every other node kind carries no Lean-closure
      obligation and is absent from the map.
    * `correspondence`, `substantiveness` — `pass`, `fail`, or `?` for the
      kernel's Unknown, which is what a drifted fingerprint under a stored Pass
      reads as.
    * `soundness` — `pass`, `fail`, `?`, or `not_required` where `needs_sound`
      is false, the synthetic VerifierPass the kernel gives a node whose Lean is
      closed.
    * `sound_recorded_fail`, keyed by the nodes carrying a fail verdict on
      record in `sound_assessments` — the `SoundAssessmentStatus` name of that
      verdict, `ReviewerPinnedFail` among them. A node reading `not_required`
      in the `soundness` map appears here when the Soundness lane rejected it
      before its Lean closed.
    """
    verdicts: Dict[str, Any] = {
        "available": False,
        "lean_closure": {},
        "correspondence": {},
        "soundness": {},
        "substantiveness": {},
        "sound_recorded_fail": {},
    }
    try:
        # `json.JSONDecodeError` and the `UnicodeDecodeError` a non-UTF-8 file
        # raises are both `ValueError`.
        state = load_json(runtime_root / "protocol_state.json", default=None)
    except (OSError, ValueError):
        return verdicts
    if not isinstance(state, Mapping):
        return verdicts
    lean_closure: Dict[str, str] = {}
    correspondence: Dict[str, str] = {}
    soundness: Dict[str, str] = {}
    substantiveness: Dict[str, str] = {}
    recorded_fail: Dict[str, str] = {}
    try:
        kernel = _KernelLaneStates(state)
        for node in kernel.present_node_order:
            closure = kernel.lean_closure_state(node)
            if closure is not None:
                lean_closure[node] = closure
            correspondence[node] = kernel.corr_state(node)
            soundness[node] = kernel.sound_state(node)
            substantiveness[node] = kernel.substantiveness_state(node)
            stored = kernel.stored_sound_assessment(node)
            if stored is not None and stored["status"] in _SOUND_FAIL_STATUSES:
                recorded_fail[node] = stored["status"]
    except _MalformedProtocolState:
        return verdicts

    verdicts.update(
        available=True,
        lean_closure=lean_closure,
        correspondence=correspondence,
        soundness=soundness,
        substantiveness=substantiveness,
        sound_recorded_fail=recorded_fail,
    )
    return verdicts


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
    # The context JSON is the audit's read surface (the burst is pointed at it
    # by path); `node_lane_verdicts` rides along there so the audit can see
    # which nodes hold which lane verdicts. `.request.json` stays the verbatim
    # kernel request. Unknown fields deserialize away on the kernel side
    # (`WrapperRequest` is `#[serde(default)]`), so the acceptance check the
    # audit runs over its own artifact reads the enriched file unchanged.
    context_payload = dict(request)
    context_payload["node_lane_verdicts"] = _stuck_math_audit_node_lane_verdicts(
        runtime_root
    )
    save_json(context_json_path, context_payload)
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
    # A malformed payload (unparseable / unknown choice) is surfaced as
    # Malformed and routed by the kernel's normal stutter path
    # (`apply_human_gate_response` re-issues the gate). It is never `fresh`
    # and is never consumed here — leaving the garbage file in place keeps
    # the malformed signal visible to the operator. This preserves the
    # pre-existing malformed-gate behavior.
    if str(output.get("status", "")).lower() == "malformed":
        return output
    # Gate-response freshness (Defect 1). The kernel is the load-bearing
    # guard: it reports `fresh = True` only when the on-disk payload carries
    # a `cycle` stamp matching the in-flight gate's cycle. A stale or
    # un-stamped response (e.g. an approve left over from an earlier gate) is
    # NOT a valid response to THIS gate — treat it exactly like a missing
    # file so the supervisor keeps blocking and polling (see
    # `should_poll_for_human_gate_response` in runtime_cli.rs, which keys off
    # the "missing human gate response file" substring). Do NOT consume the
    # file in this case — a future gate at the stamped cycle may legitimately
    # match it, and deleting it here would silently drop the operator's
    # signal.
    if not bool(kernel_response.get("fresh", False)):
        raise BridgeError(
            "missing human gate response file (stale or unstamped): "
            f"{path}; its cycle stamp does not match the current gate "
            f"(cycle {_request_cycle(request)}). The supervisor will keep "
            "blocking until a matching response is written."
        )
    # Fresh + valid: consume the response so it cannot re-satisfy a later
    # gate. Remove both the kernel-consumed file and the viewer sidecar.
    _consume_human_gate_response(runtime_root, path)
    return output


def _consume_human_gate_response(runtime_root: Path, path: Path) -> None:
    """Delete the consumed gate response file and its viewer sidecar so a
    fresh response cannot be re-read at a later gate (Defect 1)."""
    for target in (path, runtime_root / "human_gate_response.viewer_meta.json"):
        try:
            target.unlink()
        except FileNotFoundError:
            pass
        except Exception:
            # Best-effort consume — never fail the gate over cleanup. The
            # freshness stamp still prevents the now-stale file from
            # re-satisfying a future gate even if the unlink failed.
            pass


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
    # Unconditional check: the marker is only written under the opt-in
    # system_feedback_halt knob, but a marker on disk (however it got
    # there) always refuses new dispatch until an operator deletes it.
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
    if _request_kind(request) != "human_gate":
        try:
            validate_commissioned_operation(request, repo=config.repo_path)
        except ValueError as exc:
            raise BridgeError(str(exc)) from exc

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
        # active_node_prewarm (off by default): nudge the supervisor-side warm
        # active-node `lean --server` to (re-)seed the active node just before
        # the worker burst launches, so the worker's first incremental-check is
        # already warm. Fully gated + best-effort: a no-op unless
        # `active_node_prewarm.enabled` AND the server socket is exported, and
        # it can never raise into dispatch. The worker-side preference (in
        # incremental_check) reads the socket via the env passthrough.
        maybe_prewarm_active_node(
            config_path=bridge_request.config_path, request=request
        )
        # Export the prewarm env into os.environ BEFORE _handle_worker builds the
        # bwrap command line, so sandbox._passthrough_value_envs() forwards the
        # active node + giant-node thresholds (+ server socket) into the burst.
        # No-op unless active_node_prewarm.enabled (and the worker burst hook is
        # itself gated on TRELLIS_INCREMENTAL_PREWARM=1), so it stays inert for a
        # run that has not opted in.
        export_worker_burst_env(
            os.environ,
            config_path=bridge_request.config_path,
            request=request,
        )
        return _handle_worker(
            config=config,
            runtime_root=bridge_request.runtime_root,
            request=request,
        )
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
