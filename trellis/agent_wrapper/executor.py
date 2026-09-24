"""Single-request execution and artifact normalization for the shared wrapper."""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
import os
import sys
import time
from pathlib import Path
from typing import Callable, Dict, Optional, Protocol

from trellis.adapters import BurstResult
from trellis.artifacts import artifact_stem, done_marker_path, raw_json_path
from trellis.burst import run_reviewer_burst, run_worker_burst
from trellis.burst_home import cleanup_burst_home
from trellis.checking import validate_json_artifact
from trellis.json_io import append_jsonl, load_json, timestamp_now
from trellis.project_paths import (
    project_config_path,
    project_feedback_log_path,
    project_state_dir_for_repo,
)
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


# Map a wrapper lane kind to the codex base-instructions file stem
# (`codex_base_instructions/<stem>.md`). Lanes whose kind is not listed fall
# back to the lane kind itself, and any stem with no matching file falls back
# to codex's stock prompt (file presence is the only switch). Several verifier
# lane kinds are renamed to the canonical lane vocabulary used for the prompt
# files; `correspondence` and the cleanup-v2 audit lane (kind `reviewer`) pass
# through unchanged.
_LANE_KIND_TO_BASE_INSTRUCTIONS_KEY: Dict[str, str] = {
    "paper-faithfulness": "faithfulness",
    "soundness-node": "soundness",
}


def _base_instructions_key_for_request(request: SingleAgentRequest) -> str:
    """Finest available per-role identifier for selecting a base-instructions
    file, derived from the lane kind (which is distinct per codex lane) and
    falling back to the burst role.
    """
    lane_kind = str(getattr(request.lane, "kind", "") or "").strip()
    if lane_kind:
        return _LANE_KIND_TO_BASE_INSTRUCTIONS_KEY.get(lane_kind, lane_kind)
    return str(request.burst_role or "").strip()


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
    # Append to the unified system-feedback log, and — only when
    # system_feedback halting is enabled (opt-in via top-level
    # `system_feedback_halt` in trellis.config.json, or env
    # TRELLIS_SYSTEM_FEEDBACK_HALT=1) — persist the sticky halt marker.
    _write_system_feedback_halt_marker(
        request,
        artifact_name=artifact_name,
        system_feedback=text,
    )


# Runtime-root-relative filenames, byte-identical to the Rust kernel
# (`kernel/src/runtime_cli_observations.rs`) so a store/log written by
# either process is read by the other.
SYSTEM_FEEDBACK_ACK_STORE_FILENAME = "system_feedback_acks.json"
SYSTEM_FEEDBACK_LOG_FILENAME = "system_feedback_log.jsonl"

# Environment override for the `system_feedback_halt` config knob:
# `1`/`true` forces halting on, `0`/`false` forces it off, taking
# precedence over trellis.config.json in BOTH directions
# (launcher-friendly). Unset/empty defers to the config. Mirrors the Rust
# `SYSTEM_FEEDBACK_HALT_ENV`.
SYSTEM_FEEDBACK_HALT_ENV = "TRELLIS_SYSTEM_FEEDBACK_HALT"

# The exact set of code points for which Rust's `char::is_whitespace()`
# returns true (the Unicode `White_Space=Yes` property). We hand-roll the
# whitespace collapse against THIS set instead of Python's `str.split()`
# because `str.split()` also treats U+001C–U+001F (the file/group/record/
# unit separators) as whitespace while Rust does NOT — a divergence that
# would otherwise let feedback containing those control bytes fingerprint
# differently across the two languages.
_RUST_WHITESPACE = frozenset(
    "".join(chr(cp) for cp in (
        0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x20,  # ASCII controls + space
        0x85, 0xA0,  # NEL, NBSP
        0x1680,  # OGHAM SPACE MARK
        *range(0x2000, 0x200B),  # EN QUAD .. HAIR SPACE (2000-200A)
        0x2028, 0x2029,  # LINE / PARAGRAPH SEPARATOR
        0x202F, 0x205F, 0x3000,  # NNBSP, MMSP, IDEOGRAPHIC SPACE
    ))
)


def _normalize_system_feedback(text: str) -> str:
    """Cycle-STABLE normalization for fingerprinting. Byte-for-byte port of
    Rust `normalize_system_feedback`: (1) each backtick-delimited span
    (through the next backtick, or end-of-string) → ` <id> `; (2) each run
    of ASCII digits → ` <n> `; (3) per-character lowercase; (4) collapse
    Rust-whitespace runs to a single space and trim.
    """
    out: list[str] = []
    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        if c == "`":
            i += 1
            while i < n and text[i] != "`":
                i += 1
            if i < n:  # consume the closing backtick
                i += 1
            out.append(" <id> ")
        elif "0" <= c <= "9":  # ASCII digits only (matches is_ascii_digit)
            i += 1
            while i < n and "0" <= text[i] <= "9":
                i += 1
            out.append(" <n> ")
        else:
            out.append(c.lower())  # per-char lowercase, matches char::to_lowercase
            i += 1
    joined = "".join(out)
    tokens: list[str] = []
    cur: list[str] = []
    for ch in joined:
        if ch in _RUST_WHITESPACE:
            if cur:
                tokens.append("".join(cur))
                cur = []
        else:
            cur.append(ch)
    if cur:
        tokens.append("".join(cur))
    return " ".join(tokens)


def system_feedback_fingerprint(
    request_kind: str,
    burst_role: str,
    system_feedback: str,
) -> str:
    """Stable fingerprint for a `system_feedback` emission's gap-class.

    Byte-for-byte port of Rust `system_feedback_fingerprint`:
    ``sha256("sffp:v1" ␟ request_kind ␟ burst_role ␟ normalize(feedback))``
    as lowercase 64-char hex, where ``␟`` is the ASCII unit separator
    (0x1F). A fingerprint written by either the Python writer or the Rust
    CLI is identical for the same ``(request_kind, burst_role, feedback)``.
    """
    normalized = _normalize_system_feedback(str(system_feedback or ""))
    material = f"sffp:v1\x1f{request_kind}\x1f{burst_role}\x1f{normalized}"
    return hashlib.sha256(material.encode("utf-8")).hexdigest()


def _load_system_feedback_ack_store(runtime_root: Optional[Path]) -> set:
    """Return the set of operator-acknowledged fingerprints. FAIL-CLOSED:
    a missing root, missing file, unreadable file, or unparseable content
    all yield the empty set, so the ack policy can only ever SUPPRESS a
    halt for a fingerprint an operator explicitly and parseably recorded —
    never accidentally widen suppression. Never raises. Reads the same
    ``system_feedback_acks.json`` shape the Rust CLI writes: a top-level
    object with an ``acknowledged`` map keyed by fingerprint.
    """
    if runtime_root is None:
        return set()
    try:
        path = runtime_root / SYSTEM_FEEDBACK_ACK_STORE_FILENAME
        if not path.exists():
            return set()
        data = json.loads(path.read_text())
    except Exception:
        return set()
    if not isinstance(data, dict):
        return set()
    acked = data.get("acknowledged")
    if not isinstance(acked, dict):
        return set()
    return {str(k) for k in acked.keys()}


def _system_feedback_halt_enabled(work_dir: Optional[Path]) -> bool:
    """Resolve whether a non-empty `system_feedback` emission should HALT
    the run (write the sticky halt marker) or log-and-continue. Mirrors
    the Rust `system_feedback_halt_enabled`:

    1. Env ``TRELLIS_SYSTEM_FEEDBACK_HALT`` — ``1``/``true`` => halt,
       ``0``/``false`` => no halt (case-insensitive), beating the config
       in BOTH directions. Any other non-empty value is malformed =>
       loud stderr diagnostic and HALT-ENABLED (conservative: unreadable
       operator intent resolves to the fail-loudly behavior).
    2. Top-level ``system_feedback_halt`` bool in
       ``<work_dir>/trellis.config.json`` (lazy-parsed per call). Missing
       file / absent key => FALSE (the shipped default is
       log-and-continue). Unparseable file or non-bool value => loud
       stderr diagnostic and HALT-ENABLED.

    The marker CHECKS (bridge dispatch gate, kernel run loop) stay
    unconditional: a marker already on disk halts regardless of the knob.
    """
    raw = str(os.environ.get(SYSTEM_FEEDBACK_HALT_ENV, "") or "").strip().lower()
    if raw:
        if raw in ("1", "true"):
            return True
        if raw in ("0", "false"):
            return False
        print(
            f"trellis: malformed {SYSTEM_FEEDBACK_HALT_ENV}={raw!r} (expected 1/0/true/false); "
            "treating system_feedback halting as ENABLED (conservative).",
            file=sys.stderr,
        )
        return True
    if work_dir is None:
        return False
    config_path = project_config_path(Path(work_dir))
    if not config_path.exists():
        return False
    try:
        parsed = json.loads(config_path.read_text())
    except (OSError, ValueError) as err:
        print(
            f"trellis: failed to read/parse {config_path} while resolving system_feedback_halt: "
            f"{err}; treating system_feedback halting as ENABLED (conservative).",
            file=sys.stderr,
        )
        return True
    value = parsed.get("system_feedback_halt") if isinstance(parsed, dict) else None
    if value is None:
        return False
    if isinstance(value, bool):
        return value
    print(
        f"trellis: config {config_path} has non-bool system_feedback_halt {value!r}; "
        "treating system_feedback halting as ENABLED (conservative).",
        file=sys.stderr,
    )
    return True


def _append_system_feedback_log(
    runtime_root: Path,
    *,
    request: SingleAgentRequest,
    artifact_name: str,
    system_feedback: str,
    fingerprint: str,
    acked: bool,
    halted: bool,
) -> None:
    """Append one JSONL record to the unified system-feedback log. Called
    for EVERY non-empty emission — ``acked`` marks operator-acknowledged
    (halt-suppressed) recurrences, ``halted`` marks emissions that
    occurred under halt-enabled mode and left the run halting.
    Best-effort: a failed write is swallowed (the log is a forensic
    layer, not authoritative). Matches the Rust
    `append_system_feedback_log` field set.
    """
    record = {
        "schema_version": 1,
        "kind": "system_feedback",
        "unix_ts": int(time.time()),
        "fingerprint": fingerprint,
        "active_node": str(request.lane.node_name or ""),
        "active_coarse_node": "",
        "cycle": int(request.cycle or 0),
        "request_id": str(request.request_id or ""),
        "request_kind": str(request.kind or ""),
        "burst_role": str(request.burst_role or ""),
        "lane": request.lane.key(),
        "artifact": str(artifact_name or ""),
        "system_feedback": system_feedback,
        "acked": bool(acked),
        "halted": bool(halted),
    }
    try:
        path = runtime_root / SYSTEM_FEEDBACK_LOG_FILENAME
        with path.open("a") as fh:
            fh.write(json.dumps(record) + "\n")
    except OSError:
        return


def _write_system_feedback_halt_marker(
    request: SingleAgentRequest,
    *,
    artifact_name: str,
    system_feedback: str,
) -> None:
    """Record a non-empty `system_feedback` emission: ALWAYS append one
    row to `<runtime_root>/system_feedback_log.jsonl`, and additionally
    persist a halt marker at `<runtime_root>/system_feedback_halt.json`
    when system_feedback halting is enabled (opt-in — see
    `_system_feedback_halt_enabled`). Default (knob absent):
    log-and-continue with a loud stderr notice. Opt-in (knob true): the
    historical fail-loudly behavior — marker mirrors the
    checker-disagreement pattern (commit 18b59ef): sticky across
    restarts, operator clears by deletion, first marker wins so we don't
    clobber the original diagnostic. An operator-acknowledged fingerprint
    log-and-continues in BOTH modes.
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
    # Stable fingerprint for this emission's gap-class. Computed
    # unconditionally so it can be recorded in the halt marker (novel case)
    # AND matched against the ack store (recurrence case). Uses the SAME
    # request_kind/burst_role strings that go into the marker payload, so a
    # fingerprint written here is identical to the one the Rust CLI derives
    # from those marker fields.
    fingerprint = system_feedback_fingerprint(
        str(request.kind or ""),
        str(request.burst_role or ""),
        system_feedback,
    )
    # CONSERVATIVE ack policy — can only fire when an operator has
    # explicitly acknowledged THIS fingerprint. With the default (no ack
    # store on disk / empty store), membership is always False. An acked
    # fingerprint log-and-continues in BOTH halt modes: one unified log
    # row, no marker. Checked BEFORE the marker-exists guard to mirror
    # the Rust ordering (an acked recurrence is logged even if an
    # earlier, unrelated marker is still present).
    if fingerprint in _load_system_feedback_ack_store(runtime_root):
        _append_system_feedback_log(
            runtime_root,
            request=request,
            artifact_name=artifact_name,
            system_feedback=system_feedback,
            fingerprint=fingerprint,
            acked=True,
            halted=False,
        )
        return
    halt_enabled = _system_feedback_halt_enabled(request.work_dir)
    # One durable stream regardless of mode: the unified log row lands
    # before any marker bookkeeping, so halt-mode re-emissions (marker
    # already present) are recorded too.
    _append_system_feedback_log(
        runtime_root,
        request=request,
        artifact_name=artifact_name,
        system_feedback=system_feedback,
        fingerprint=fingerprint,
        acked=False,
        halted=halt_enabled,
    )
    if not halt_enabled:
        print(
            "trellis: SYSTEM_FEEDBACK EMITTED (log-and-continue): an agent burst returned a "
            f"non-empty system_feedback string on request_id={request.request_id} "
            f"(kind={request.kind}, role={request.burst_role}, "
            f"node={request.lane.node_name or ''}, cycle={request.cycle}). The full text and "
            f"stable fingerprint {fingerprint} were appended to "
            f"{runtime_root / SYSTEM_FEEDBACK_LOG_FILENAME}; the run CONTINUES. system_feedback "
            "usually signals a design gap or harness bug worth operator review; to make future "
            'emissions halt the run instead, set top-level "system_feedback_halt": true in '
            f"trellis.config.json (or {SYSTEM_FEEDBACK_HALT_ENV}=1). "
            f"Feedback text: {system_feedback}",
            file=sys.stderr,
        )
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
        "fingerprint": fingerprint,
        "clear_instructions": (
            "The trellis supervisor is HALTED because an agent burst "
            f"returned a non-empty `system_feedback` string on "
            f"request_id={request.request_id} (kind={request.kind}, "
            f"node={request.lane.node_name or ''}, cycle={request.cycle}). "
            "Every system_feedback emission is treated as a design-gap "
            "signal that requires human inspection — the supervisor will "
            "not dispatch new bursts until you review the `system_feedback` "
            f"field above and then DELETE this file to resume: rm {marker_path}. "
            "If this is a KNOWN design gap you want the supervisor to "
            "log-and-continue on future recurrences (instead of halting each "
            f"cycle), acknowledge its fingerprint `{fingerprint}` by piping a "
            "JSON request to the runtime CLI on stdin: "
            '{\"action\":\"ack_system_feedback\",\"root\":\"<runtime_root>\",'
            f'\"fingerprint\":\"{fingerprint}\",\"reason\":\"<why>\"}} '
            f"(records it in `{SYSTEM_FEEDBACK_ACK_STORE_FILENAME}` and resumes "
            "this halt). Acks are PER-FINGERPRINT; any NOVEL feedback still halts. "
            "Halting on system_feedback is OPT-IN: this run enabled it via the "
            "top-level `system_feedback_halt` knob in trellis.config.json (or "
            f"{SYSTEM_FEEDBACK_HALT_ENV}=1); with the knob off (the default) "
            f"emissions are appended to `{SYSTEM_FEEDBACK_LOG_FILENAME}` and the "
            "run continues."
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

    base_instructions_key = _base_instructions_key_for_request(request)

    burst_result: BurstResult
    try:
        if request.burst_role in {"worker", "source_adaptation_worker"}:
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
                base_instructions_key=base_instructions_key,
                sandbox_role=request.burst_role,
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
                base_instructions_key=base_instructions_key,
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
