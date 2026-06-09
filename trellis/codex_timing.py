"""On-demand codex burst timing decomposition from rollout files.

Replaces the previous in-process tracker that wrote a `timing` field on
every cost-ledger row. Now the rollout file under
`~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-*-<sessionId>.jsonl`
is the authoritative source — read it on demand, compute, return.

# Rollout schema (codex CLI 0.125.x)

Each line is a JSON object with `timestamp` (ISO 8601 with ms, UTC) and
`type` (outer envelope) plus a `payload` object whose internal `type`
discriminates the event. The shapes we care about:

  - `session_meta` — header line with `payload.id` (session id, matches
    the rollout filename suffix).
  - `event_msg` payload `type=task_started` — turn boundary; carries the
    `turn_id`. One per-cycle of the codex agent loop.
  - `event_msg` payload `type=task_complete` — turn-end marker for the
    same `turn_id`. Carries `duration_ms`, `time_to_first_token_ms`.
  - `response_item` payload `type=function_call` — a tool invocation
    (e.g. `name=exec_command`, `name=write_stdin`). Has a `call_id`.
  - `response_item` payload `type=function_call_output` — paired output
    for that tool, same `call_id`.
  - `response_item` payload `type=custom_tool_call` / `custom_tool_call_output`
    — used for `apply_patch` (file changes); same call_id pairing.
  - `event_msg` payload `type=agent_message` — atomic model message
    emission (no started/completed pair).

# Burst -> rollout slice

A single rollout file may contain MANY turns (the codex thread is reused
via `codex exec resume <thread>`). The cost-ledger row's `session_id`
matches `payload.id` of the rollout's `session_meta` line, but each row
covers exactly ONE turn. We slice by turn boundaries: pick the
`task_started` event whose timestamp is closest to (and not later than)
`row.ts_start`, then take all events through its matching `task_complete`
(same `turn_id`).

# Decomposition

  duration = tool_exec + file_change + llm

  - `tool_exec_seconds` = sum over function_call/function_call_output
    pairs (excluding `apply_patch`) of (output_ts - call_ts).
  - `file_change_seconds` = same, but for `apply_patch` (custom tool calls
    or function calls named `apply_patch`).
  - `llm_seconds` = max(0, duration - tool_exec - file_change). Atomic
    `agent_message` events contribute nothing directly: the LLM time is
    the residual that the model's API roundtrips and the codex CLI
    overhead jointly account for.

  - `item_count` = count of completed/atomic items in the slice
    (function_call_output + custom_tool_call_output + agent_message).

# Reads

Post-bwrap-only, bursts run as the operator, so rollouts written inside a
burst are operator-owned (hard-linked back into the operator's
`~/.codex/sessions/`). The viewer and supervisor read them directly; no
cross-user sudo hop is involved. The sessions root is configurable via
`TRELLIS_CODEX_SESSIONS_ROOT` (e.g. for tests).

# Limitations

  - Bursts launched with `--ephemeral` (e.g. fresh corr/sound reviewer
    bursts) DO NOT write a rollout file. `find_rollout_path` returns
    None for those sessions; `compute_timing_for_session` raises a
    clean `RolloutNotFoundError`. CLI prints a one-line note and skips.
  - The rollout's outer `timestamp` is a UTC ISO 8601 string; we parse
    it with the stdlib (`datetime.fromisoformat`) and convert to a Unix
    epoch float for arithmetic.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable, Optional

# Default rollout location. Post-bwrap-only, bursts run as the operator and
# rollouts are operator-owned; override via TRELLIS_CODEX_SESSIONS_ROOT.
CODEX_WORKER_HOME = Path("${TRELLIS_BURST_USER_HOME:-/home/<sandbox-user>}")
DEFAULT_SESSIONS_ROOT = CODEX_WORKER_HOME / ".codex" / "sessions"


class RolloutNotFoundError(LookupError):
    """No rollout file matched the given session_id."""


class RolloutReadError(RuntimeError):
    """The rollout file exists but couldn't be read or parsed."""


# ---------------------------------------------------------------------------
# File location & cross-user read
# ---------------------------------------------------------------------------


def _sessions_root() -> Path:
    """Allow tests / operators to override via env var."""
    env = os.environ.get("TRELLIS_CODEX_SESSIONS_ROOT")
    if env:
        return Path(env)
    return DEFAULT_SESSIONS_ROOT


_ROLLOUT_INDEX_CACHE: dict[Path, dict[str, Path]] = {}


def _build_rollout_index(root: Path) -> dict[str, Path]:
    """Return a dict {session_id_suffix: rollout_path} for all rollouts under
    `root`. Cached per-root for the life of this process.

    Uses a direct rglob when readable, else falls back to a single sudo find.
    """
    if root in _ROLLOUT_INDEX_CACHE:
        return _ROLLOUT_INDEX_CACHE[root]
    paths: list[Path] = []
    try:
        if root.is_dir():
            paths = list(root.rglob("rollout-*.jsonl"))
    except (PermissionError, OSError):
        paths = []
    # Phase 4 bwrap-only migration: rollouts are supervisor-owned (or
    # hard-linked from the per-burst fake-home back into the
    # supervisor's `~/.codex/sessions/`). Direct `rglob` above is the
    # primary path; this fallback is retained as a defensive sweep
    # using a plain `find` if rglob hit any transient OSError.
    if not paths:
        try:
            proc = subprocess.run(
                ["find", str(root), "-name", "rollout-*.jsonl"],
                capture_output=True, text=True, timeout=10,
            )
        except (FileNotFoundError, subprocess.TimeoutExpired):
            proc = None
        if proc is not None and proc.returncode == 0:
            paths = [Path(p) for p in proc.stdout.splitlines() if p.strip()]
    index: dict[str, Path] = {}
    for p in paths:
        # Filename pattern: rollout-<isoTs>-<sessionId>.jsonl. Index on the
        # session id, which is the last hyphen-separated chunk of the stem.
        stem = p.stem  # rollout-2026-05-01T01-33-04-019de206-b85d-74b3-834d-74206c28b719
        if not stem.startswith("rollout-"):
            continue
        # session_id is a 36-char dashed UUID at the end. Take the last
        # 36 characters of the stem.
        if len(stem) < 36:
            continue
        sid = stem[-36:]
        index[sid] = p
    _ROLLOUT_INDEX_CACHE[root] = index
    return index


def find_rollout_path(session_id: str, *, sessions_root: Optional[Path] = None) -> Optional[Path]:
    """Return the rollout .jsonl file for `session_id`, or None.

    Codex names rollouts `rollout-<isoTs>-<sessionId>.jsonl` under
    `~/.codex/sessions/<YYYY>/<MM>/<DD>/`. We index ALL
    rollouts under the sessions root once (cached process-lifetime),
    then look up by filename suffix (`-<session_id>.jsonl`).

    Post-bwrap-only the tree is operator-owned, so the direct `rglob`
    is the primary path; a plain `find` is retained as a defensive
    fallback if `rglob` hit a transient error. Subsequent calls hit
    the cache.
    """
    sid = (session_id or "").strip()
    if not sid:
        return None
    root = sessions_root or _sessions_root()
    index = _build_rollout_index(root)
    direct = index.get(sid)
    if direct is not None:
        return direct
    # Fall back to suffix match (e.g. partial session id, or a non-UUID
    # synthetic id used in tests).
    suffix = f"-{sid}.jsonl"
    for path in index.values():
        if path.name.endswith(suffix):
            return path
    return None


def _read_rollout_text(path: Path) -> str:
    """Read the rollout file (UTF-8).

    Phase 4 bwrap-only migration: rollouts written inside a burst land
    in the per-burst fake-home and are hard-linked back to the
    supervisor's `~/.codex/sessions/`, so the supervisor user can read them
    directly. Raises RolloutReadError on failure.
    """
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except FileNotFoundError as exc:
        raise RolloutReadError(f"rollout vanished: {path}") from exc
    except PermissionError as exc:
        raise RolloutReadError(f"rollout permission denied: {path}: {exc}") from exc
    except OSError as exc:
        raise RolloutReadError(f"rollout read failed: {path}: {exc}") from exc


def _iter_rollout_events(text: str) -> Iterable[dict]:
    """Yield parsed JSON event dicts from rollout text. Skips malformed lines."""
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except (json.JSONDecodeError, ValueError):
            continue
        if isinstance(rec, dict):
            yield rec


# ---------------------------------------------------------------------------
# Timestamp parsing
# ---------------------------------------------------------------------------


def _parse_ts(ts: Any) -> Optional[float]:
    """Parse the rollout's outer `timestamp` (ISO 8601 UTC) to epoch seconds."""
    if not isinstance(ts, str) or not ts:
        return None
    try:
        # Codex emits "2026-05-01T05:33:06.994Z" — replace Z for fromisoformat.
        s = ts[:-1] + "+00:00" if ts.endswith("Z") else ts
        return datetime.fromisoformat(s).timestamp()
    except (ValueError, TypeError):
        return None


# ---------------------------------------------------------------------------
# Slice extraction
# ---------------------------------------------------------------------------


def _slice_events_for_burst(
    events: list[tuple[float, dict]],
    *,
    ts_start: Optional[float],
    duration_seconds: Optional[float],
) -> list[tuple[float, dict]]:
    """Restrict events to a single burst's turn.

    Strategy:
      1. If no ts_start hint, return all events (single-turn rollout case).
      2. Find the `task_started` event whose timestamp is closest to
         ts_start (and not later than ts_start + 60s slack); take its turn_id.
      3. Take all events from that task_started through the matching
         task_complete (or end of file if absent).
      4. If no task_started exists, fall back to a time-window filter
         [ts_start - slack, ts_start + duration + slack] to catch single-
         event rollouts and ephemeral fragments.
    """
    if not events:
        return []
    if ts_start is None:
        return events

    # Find best task_started.
    best_idx = None
    best_dt = None
    for i, (ts, rec) in enumerate(events):
        payload = rec.get("payload") or {}
        if not isinstance(payload, dict):
            continue
        if payload.get("type") != "task_started":
            continue
        # Pick the latest task_started at or before ts_start, falling back
        # to the closest one if none precedes (clock skew).
        dt = ts - ts_start
        if dt <= 30:  # allow 30s slack on either side
            score = abs(dt)
            if best_dt is None or score < best_dt:
                best_idx = i
                best_dt = score

    if best_idx is None:
        slack = 30.0
        end = (ts_start + (duration_seconds or 0)) + slack
        start_lo = ts_start - slack
        return [(t, r) for (t, r) in events if start_lo <= t <= end]

    started_ts, started_rec = events[best_idx]
    started_payload = started_rec.get("payload") or {}
    turn_id = started_payload.get("turn_id") if isinstance(started_payload, dict) else None

    # Walk forward until we see the matching task_complete (same turn_id).
    end_idx = len(events)
    for j in range(best_idx + 1, len(events)):
        _, rec_j = events[j]
        payload_j = rec_j.get("payload") or {}
        if not isinstance(payload_j, dict):
            continue
        if payload_j.get("type") == "task_started":
            # Hit the next turn before seeing task_complete; stop here.
            end_idx = j
            break
        if payload_j.get("type") == "task_complete":
            if not turn_id or payload_j.get("turn_id") == turn_id:
                end_idx = j + 1
                break
    return events[best_idx:end_idx]


# ---------------------------------------------------------------------------
# Decomposition
# ---------------------------------------------------------------------------


def _decompose(
    slice_events: list[tuple[float, dict]],
    *,
    duration_seconds: Optional[float],
) -> dict:
    """Bucket the events and return the timing dict.

    Pairs `function_call`/`function_call_output` and
    `custom_tool_call`/`custom_tool_call_output` by `call_id`. Tools named
    `apply_patch` (in either flavor) go to file_change; everything else
    is tool_exec. Atomic agent_message events count toward item_count
    only.
    """
    pending: dict[str, tuple[float, str, str]] = {}  # call_id -> (ts, kind, name)
    tool_exec = 0.0
    file_change = 0.0
    item_count = 0

    first_ts: Optional[float] = None
    last_ts: Optional[float] = None

    for ts, rec in slice_events:
        if first_ts is None:
            first_ts = ts
        last_ts = ts
        payload = rec.get("payload") or {}
        if not isinstance(payload, dict):
            continue
        ptype = payload.get("type")
        if ptype == "function_call":
            cid = str(payload.get("call_id") or "")
            name = str(payload.get("name") or "")
            if cid:
                pending[cid] = (ts, "function", name)
        elif ptype == "function_call_output":
            cid = str(payload.get("call_id") or "")
            item_count += 1
            started = pending.pop(cid, None) if cid else None
            if started is None:
                continue
            start_ts, _, name = started
            dur = max(0.0, ts - start_ts)
            if name == "apply_patch":
                file_change += dur
            else:
                tool_exec += dur
        elif ptype == "custom_tool_call":
            cid = str(payload.get("call_id") or "")
            name = str(payload.get("name") or "")
            if cid:
                pending[cid] = (ts, "custom", name)
        elif ptype == "custom_tool_call_output":
            cid = str(payload.get("call_id") or "")
            item_count += 1
            started = pending.pop(cid, None) if cid else None
            if started is None:
                continue
            start_ts, _, name = started
            dur = max(0.0, ts - start_ts)
            # Custom tool calls are typically apply_patch; treat by name.
            if name == "apply_patch":
                file_change += dur
            else:
                tool_exec += dur
        elif ptype == "agent_message":
            item_count += 1

    # If no explicit duration provided, derive from slice span.
    if duration_seconds is None and first_ts is not None and last_ts is not None:
        duration_seconds = max(0.0, last_ts - first_ts)
    duration_seconds = float(duration_seconds or 0.0)
    llm = max(0.0, duration_seconds - tool_exec - file_change)

    return {
        "tool_exec_seconds": round(tool_exec, 3),
        "file_change_seconds": round(file_change, 3),
        "llm_seconds": round(llm, 3),
        "item_count": item_count,
        "duration_seconds": round(duration_seconds, 3),
    }


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------


def compute_timing_from_text(
    text: str,
    *,
    ts_start: Optional[float] = None,
    duration_seconds: Optional[float] = None,
) -> dict:
    """Compute timing from rollout file contents (no I/O).

    Useful for tests and when the caller has already loaded the rollout.
    """
    events: list[tuple[float, dict]] = []
    for rec in _iter_rollout_events(text):
        ts = _parse_ts(rec.get("timestamp"))
        if ts is None:
            continue
        events.append((ts, rec))
    sliced = _slice_events_for_burst(
        events, ts_start=ts_start, duration_seconds=duration_seconds,
    )
    return _decompose(sliced, duration_seconds=duration_seconds)


def compute_timing_for_session(
    session_id: str,
    *,
    duration_seconds: Optional[float] = None,
    ts_start: Optional[float] = None,
    sessions_root: Optional[Path] = None,
) -> dict:
    """Compute the per-burst timing dict for `session_id`.

    Locates the rollout via :func:`find_rollout_path`, reads it, and
    decomposes the slice. If both `ts_start` and `duration_seconds` are
    given the slice is bounded; otherwise the whole rollout is used.

    Raises:
      RolloutNotFoundError — no rollout file for this session.
      RolloutReadError — rollout exists but couldn't be read or parsed.
    """
    path = find_rollout_path(session_id, sessions_root=sessions_root)
    if path is None:
        raise RolloutNotFoundError(
            f"no rollout file for session_id={session_id} under {sessions_root or _sessions_root()}"
        )
    text = _read_rollout_text(path)
    out = compute_timing_from_text(
        text, ts_start=ts_start, duration_seconds=duration_seconds,
    )
    out["rollout_path"] = str(path)
    return out


def compute_timing_for_ledger_row(row: dict, *, sessions_root: Optional[Path] = None) -> dict:
    """Convenience wrapper: extract session_id + ts_start + duration from a
    cost-ledger row, then compute timing.
    """
    sid = str(row.get("session_id") or "").strip()
    if not sid:
        raise RolloutNotFoundError("ledger row has no session_id")
    duration = row.get("duration_seconds")
    try:
        duration = float(duration) if duration is not None else None
    except (TypeError, ValueError):
        duration = None
    ts_start = row.get("ts_start")
    try:
        ts_start = float(ts_start) if ts_start is not None else None
    except (TypeError, ValueError):
        ts_start = None
    return compute_timing_for_session(
        sid,
        duration_seconds=duration,
        ts_start=ts_start,
        sessions_root=sessions_root,
    )


# ---------------------------------------------------------------------------
# β model: predict 5h-quota burn from LLM seconds
# ---------------------------------------------------------------------------
#
# Cost reporting flips from per-burst quota deltas (which were per-row noisy
# integers) to a model-derived prediction:
#
#     model_burn_5h_pct = β(phase, lane) · llm_seconds
#
# β is calibrated empirically per `(phase, lane)` from accumulated
# (llm_seconds, observed_5h_pct_delta) pairs — see
# scripts/fit_codex_burn_beta.py for the methodology and the half-vs-half
# stability analysis (fit the first and second halves of a run's cycles
# separately and check the constants agree) that informs the current
# constants.
#
# Initial calibration uses the same constants across phases — there is not
# yet enough per-phase signal to justify per-phase calibration. The data
# structure is keyed by phase up front so per-phase β values can land here
# as evidence accumulates without rewiring callers.
#
# Lane classes:
#   - worker:   role=worker (one β class)
#   - verifier: role=reviewer with sub ∈ {paper, sound, corr} (panel verifiers)
#   - review:   role=reviewer with sub=review (the substantive-review lane)

# `_default` is the catch-all for any phase not explicitly listed below.
# Per-phase entries override it; add them as calibration data accumulates.
#
# Calibration history:
#   v1: per-lane β fits via squared-hinge loss
#       on (llm_seconds, observed_5h_pct_delta) pairs. Per-lane RATIOS
#       are anchored here (verifier ~0.42× worker, review ~0.31× worker)
#       — those ratios match the cached-context-discount hypothesis.
#       Absolute scale gets calibrated independently against the
#       5h-to-weekly conversion factor in the viewer (currently 1/5;
#       see PROVIDER_5H_TO_WEEKLY in viewer/server.js).
BETA_BY_PHASE_LANE: dict[str, dict[str, float]] = {
    "_default": {
        "worker": 0.00240,    # ~417 s/pct
        "verifier": 0.00100,  # ~1000 s/pct
        "review": 0.00110,    # ~909 s/pct  (bumped from 0.00075 after MoM
                              #  on 555 rows showed 0.00139 ±0.00078; 0.00110
                              #  splits the difference, still well within CI)
    },
    # proof_formalization: only the verifier fit is committed here. A
    # ~14h45m proof_formalization window (weekly meter delta 19% from
    # 100→81)
    # gave a hinge-loss fit (weight=k+1) of β_verifier ≈ 0.00396 on
    # n=48 rows with 11 k≥1 observations; that survives a restricted
    # cut to → 08:00 EDT (n=40, β=0.00382) and the bootstrap puts it
    # below _default with only 4.5% probability — so we're confident
    # the proof-phase verifier truly burns more per LLM-second than
    # _default. Worker fit on the same window came in at 0.00275
    # (full) / 0.00231 (restricted) — direction-flips depending on
    # window cut, so worker is held at _default until more data
    # accumulates. Review was data-degenerate (72/80 buckets at k=0,
    # hinge loss satisfied at β=0); also held at _default.
    #
    # The lookup uses the phase dict whole when the phase is present,
    # so worker/review must be listed explicitly with their kept
    # _default values (otherwise beta_for_scope returns None for
    # proof-phase rows that don't match a key here).
    "proof_formalization": {
        "worker": 0.00240,    # default kept — fit unstable across cuts
        "verifier": 0.00396,  # ~253 s/pct (fit; was 0.00100, ~4x higher
                              #  — proof-phase verifiers reason densely
                              #  with little caching across substantive
                              #  Lean changes)
        "review": 0.00110,    # default kept — fit data-degenerate
    },
}

# Sub-tokens (parts[2] in scope) that classify reviewer-side calls.
_VERIFIER_SUBS = frozenset({"paper", "sound", "corr"})
_REVIEW_SUBS = frozenset({"review"})


def phase_for_scope(scope: str) -> str:
    """Extract the phase token from a cost-ledger scope.

    Scopes look like ``<phase>:<role>:[<sub>:]<id>:<v>:<provider>:<model>:<effort>``.
    Returns the leading phase token (e.g. ``theorem_stating``) or
    ``"_default"`` when the scope is empty/malformed.
    """
    if not scope:
        return "_default"
    parts = scope.split(":")
    leading = parts[0].strip() if parts else ""
    return leading or "_default"


def lane_for_scope(scope: str) -> Optional[str]:
    """Classify a cost-ledger scope into a β-model lane.

    Returns one of ``"worker"``, ``"verifier"``, ``"review"``, or ``None``
    when the scope can't be classified (unknown role/sub, malformed).
    """
    if not scope:
        return None
    parts = scope.split(":")
    if len(parts) < 2:
        return None
    role = parts[1].strip()
    if role == "worker":
        return "worker"
    if role == "reviewer" and len(parts) >= 3:
        sub = parts[2].strip()
        if sub in _VERIFIER_SUBS:
            return "verifier"
        if sub in _REVIEW_SUBS:
            return "review"
    return None


def beta_for_scope(scope: str) -> Optional[float]:
    """Look up β for a scope, applying per-phase override if calibrated.

    Returns ``None`` when the scope's lane can't be classified or no β is
    configured for it (the caller should treat this as "no model
    prediction available" and skip cost attribution for that row).
    """
    lane = lane_for_scope(scope)
    if lane is None:
        return None
    phase = phase_for_scope(scope)
    by_lane = BETA_BY_PHASE_LANE.get(phase) or BETA_BY_PHASE_LANE["_default"]
    return by_lane.get(lane)


def model_burn_5h_pct_for_row(
    row: dict, *, sessions_root: Optional[Path] = None
) -> Optional[float]:
    """Predicted 5h_pct burn for a single cost-ledger row, from β · llm_seconds.

    Returns ``None`` when:
      - provider isn't ``codex`` (other providers don't have rollouts),
      - lane can't be classified from the scope,
      - the rollout file is missing or unreadable,
      - llm_seconds is zero/missing.

    Caller should treat ``None`` as "this row contributes 0 to model
    burn" — no fallback to quota-delta is provided here on purpose; the
    cost-rollup endpoint reports rollout coverage separately so the
    operator can see what fraction of rows had usable rollouts.
    """
    if (row.get("provider") or "").strip().lower() != "codex":
        return None
    beta = beta_for_scope(row.get("scope", ""))
    if beta is None:
        return None
    try:
        timing = compute_timing_for_ledger_row(row, sessions_root=sessions_root)
    except (RolloutNotFoundError, RolloutReadError):
        return None
    if not timing:
        return None
    llm = timing.get("llm_seconds")
    if llm is None or llm <= 0:
        return None
    return float(beta) * float(llm)


# ---------------------------------------------------------------------------
# Ledger I/O
# ---------------------------------------------------------------------------


def _default_ledger_path() -> Path:
    """Resolve the default cost-ledger path.

    Honors ``TRELLIS_PROJECT_ROOT`` (project-aware), else falls back to the
    live example-run runtime which is the canonical default on this host.
    """
    env_root = os.environ.get("TRELLIS_PROJECT_ROOT")
    if env_root:
        return Path(env_root) / ".trellis" / "logs" / "cost-ledger.jsonl"
    return Path("${TRELLIS_ROOT:-/path/to/trellis}/math/example-run/.trellis/logs/cost-ledger.jsonl")


def _read_ledger(path: Path) -> list[dict]:
    if not path.is_file():
        return []
    rows: list[dict] = []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            obj = json.loads(line)
        except (json.JSONDecodeError, ValueError):
            continue
        if isinstance(obj, dict):
            rows.append(obj)
    return rows


def _row_burst_name(row: dict) -> str:
    """Best-effort burst label for matching `--burst <name>`.

    The cost-ledger doesn't store an explicit burst name, but the chat
    artifact dir does (e.g. `worker_199_result`). We match on the ROLE +
    SCOPE + a kind tag so users can paste the panel-shown burst label.
    """
    scope = str(row.get("scope") or "")
    role = str(row.get("role") or "")
    return f"{role}:{scope}" if scope else role


def _row_matches_burst(row: dict, name: str) -> bool:
    name = name.strip()
    if not name:
        return False
    if str(row.get("session_id") or "") == name:
        return True
    label = _row_burst_name(row)
    if name in label:
        return True
    # Tolerate raw scope string as the label.
    return name in str(row.get("scope") or "")


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def _fmt_float(x: Any) -> str:
    if x is None:
        return "—"
    try:
        return f"{float(x):.2f}"
    except (TypeError, ValueError):
        return str(x)


def _fmt_int(x: Any) -> str:
    if x is None:
        return "—"
    try:
        return str(int(x))
    except (TypeError, ValueError):
        return str(x)


def _print_table(headers: list[str], rows: list[list[str]], stream=None) -> None:
    stream = stream or sys.stdout
    if not rows:
        print("(no rows)", file=stream)
        return
    widths = [len(h) for h in headers]
    for r in rows:
        for i, cell in enumerate(r):
            if i >= len(widths):
                continue
            widths[i] = max(widths[i], len(cell))
    fmt = "  ".join(f"{{:<{w}}}" for w in widths)
    print(fmt.format(*headers), file=stream)
    print(fmt.format(*("-" * w for w in widths)), file=stream)
    for r in rows:
        print(fmt.format(*r[: len(widths)]), file=stream)


def _resolve_target_row(args: argparse.Namespace, ledger_rows: list[dict]) -> Optional[dict]:
    if args.session_id:
        for r in reversed(ledger_rows):
            if str(r.get("session_id") or "") == args.session_id:
                return r
        # Synthesize a minimal row so we can still compute (no ts_start
        # bound: whole-rollout fallback).
        return {"session_id": args.session_id}
    if args.burst:
        for r in reversed(ledger_rows):
            if _row_matches_burst(r, args.burst):
                return r
        return None
    if args.row is not None:
        # 1-indexed from the END (so --row 1 == latest).
        if args.row < 1 or args.row > len(ledger_rows):
            return None
        return ledger_rows[-args.row]
    return None


def _row_summary_str(row: dict) -> str:
    parts = []
    sid = str(row.get("session_id") or "")
    if sid:
        parts.append(f"session={sid}")
    parts.append(f"role={row.get('role') or '?'}")
    scope = row.get("scope")
    if scope:
        parts.append(f"scope={scope}")
    parts.append(f"duration={_fmt_float(row.get('duration_seconds'))}s")
    ts = row.get("ts")
    if ts:
        try:
            iso = datetime.fromtimestamp(float(ts), tz=timezone.utc).isoformat()
        except (TypeError, ValueError):
            iso = str(ts)
        parts.append(f"ts={iso}")
    return " ".join(parts)


def _cmd_show(args: argparse.Namespace) -> int:
    ledger_path = Path(args.ledger) if args.ledger else _default_ledger_path()
    rows = _read_ledger(ledger_path)
    target = _resolve_target_row(args, rows)
    if target is None:
        print("error: no matching row (try --row 1, --burst NAME, or pass a session_id)", file=sys.stderr)
        return 2

    sid = str(target.get("session_id") or "").strip()
    if not sid:
        print("error: target row has no session_id", file=sys.stderr)
        return 2

    try:
        timing = compute_timing_for_ledger_row(target)
    except RolloutNotFoundError as exc:
        msg = f"rollout-not-found: {exc}"
        if args.json:
            print(json.dumps({"error": "rollout_not_found", "message": str(exc)}))
        else:
            print(msg, file=sys.stderr)
        return 3
    except RolloutReadError as exc:
        if args.json:
            print(json.dumps({"error": "rollout_read_error", "message": str(exc)}))
        else:
            print(f"rollout-read-error: {exc}", file=sys.stderr)
        return 4

    if args.json:
        out = dict(timing)
        out["session_id"] = sid
        if "scope" in target:
            out["scope"] = target.get("scope")
        if "role" in target:
            out["role"] = target.get("role")
        print(json.dumps(out))
        return 0

    print(_row_summary_str(target))
    print()
    rows_out = [
        ["tool_exec_seconds", _fmt_float(timing["tool_exec_seconds"])],
        ["file_change_seconds", _fmt_float(timing["file_change_seconds"])],
        ["llm_seconds", _fmt_float(timing["llm_seconds"])],
        ["item_count", _fmt_int(timing["item_count"])],
        ["duration_seconds", _fmt_float(timing["duration_seconds"])],
        ["rollout_path", str(timing.get("rollout_path") or "")],
    ]
    _print_table(["field", "value"], rows_out)
    return 0


def _filter_since(rows: list[dict], since: Optional[str]) -> list[dict]:
    if not since:
        return rows
    try:
        cutoff = datetime.fromisoformat(since).replace(tzinfo=timezone.utc).timestamp()
    except ValueError:
        # Try date-only.
        cutoff = datetime.strptime(since, "%Y-%m-%d").replace(tzinfo=timezone.utc).timestamp()
    return [r for r in rows if float(r.get("ts") or 0) >= cutoff]


def _cmd_list(args: argparse.Namespace) -> int:
    ledger_path = Path(args.ledger) if args.ledger else _default_ledger_path()
    rows = _read_ledger(ledger_path)
    rows = _filter_since(rows, args.since)
    rows = [r for r in rows if (r.get("provider") or "") == "codex"] if args.codex_only else rows
    if args.limit:
        rows = rows[-args.limit:]

    headers = ["ts", "provider", "role", "scope", "dur_s", "tool_s", "file_s", "llm_s", "items"]
    out_rows: list[list[str]] = []
    json_out: list[dict] = []
    for r in rows:
        sid = str(r.get("session_id") or "")
        ts_iso = ""
        try:
            ts_iso = datetime.fromtimestamp(float(r.get("ts") or 0), tz=timezone.utc).isoformat(timespec="seconds")
        except (TypeError, ValueError):
            ts_iso = str(r.get("ts") or "")
        scope = str(r.get("scope") or "")
        try:
            t = compute_timing_for_ledger_row(r)
            tool_s = _fmt_float(t["tool_exec_seconds"])
            file_s = _fmt_float(t["file_change_seconds"])
            llm_s = _fmt_float(t["llm_seconds"])
            items = _fmt_int(t["item_count"])
            err = None
        except RolloutNotFoundError:
            tool_s = file_s = llm_s = items = "—"
            t = None
            err = "no_rollout"
        except RolloutReadError as exc:
            tool_s = file_s = llm_s = items = "—"
            t = None
            err = f"read_err:{exc}"
        out_rows.append([
            ts_iso,
            str(r.get("provider") or ""),
            str(r.get("role") or ""),
            scope[:50],
            _fmt_float(r.get("duration_seconds")),
            tool_s, file_s, llm_s, items,
        ])
        if args.json:
            entry = {
                "ts": r.get("ts"),
                "provider": r.get("provider"),
                "role": r.get("role"),
                "scope": r.get("scope"),
                "session_id": sid,
                "duration_seconds": r.get("duration_seconds"),
                "timing": t,
                "error": err,
            }
            json_out.append(entry)

    if args.json:
        print(json.dumps(json_out))
        return 0
    _print_table(headers, out_rows)
    return 0


def _aggregate(rows: list[dict], by: str) -> list[dict]:
    """Group rows by `by` ∈ {provider, role, scope} and aggregate timing.

    Caches rollout file contents by path for the lifetime of this call —
    multiple bursts share one rollout (multi-turn threads), so deduping
    saves a sudo-cat round-trip per row.
    """
    buckets: dict[str, dict] = {}
    events_cache: dict[Path, list[tuple[float, dict]]] = {}
    rollout_path_cache: dict[str, Optional[Path]] = {}

    def _cached_timing(row: dict) -> Optional[dict]:
        sid = str(row.get("session_id") or "").strip()
        if not sid:
            return None
        if sid in rollout_path_cache:
            path = rollout_path_cache[sid]
        else:
            path = find_rollout_path(sid)
            rollout_path_cache[sid] = path
        if path is None:
            return None
        events = events_cache.get(path)
        if events is None:
            try:
                text = _read_rollout_text(path)
            except RolloutReadError:
                return None
            events = []
            for rec in _iter_rollout_events(text):
                ts = _parse_ts(rec.get("timestamp"))
                if ts is None:
                    continue
                events.append((ts, rec))
            events_cache[path] = events
        try:
            duration = float(row.get("duration_seconds") or 0) or None
        except (TypeError, ValueError):
            duration = None
        try:
            ts_start = float(row.get("ts_start") or 0) or None
        except (TypeError, ValueError):
            ts_start = None
        sliced = _slice_events_for_burst(
            events, ts_start=ts_start, duration_seconds=duration,
        )
        return _decompose(sliced, duration_seconds=duration)

    for r in rows:
        if by == "provider":
            key = str(r.get("provider") or "?")
        elif by == "role":
            key = str(r.get("role") or "?")
        elif by == "scope":
            key = str(r.get("scope") or "?")
        else:
            key = "all"
        if key not in buckets:
            buckets[key] = {
                "key": key,
                "bursts": 0,
                "with_rollout": 0,
                "duration_seconds": 0.0,
                "tool_exec_seconds": 0.0,
                "file_change_seconds": 0.0,
                "llm_seconds": 0.0,
                "item_count": 0,
            }
        b = buckets[key]
        b["bursts"] += 1
        try:
            b["duration_seconds"] += float(r.get("duration_seconds") or 0)
        except (TypeError, ValueError):
            pass
        t = _cached_timing(r)
        if t is None:
            continue
        b["with_rollout"] += 1
        b["tool_exec_seconds"] += t["tool_exec_seconds"]
        b["file_change_seconds"] += t["file_change_seconds"]
        b["llm_seconds"] += t["llm_seconds"]
        b["item_count"] += t["item_count"]
    # Round once at the end.
    out = []
    for b in buckets.values():
        out.append({
            "key": b["key"],
            "bursts": b["bursts"],
            "with_rollout": b["with_rollout"],
            "duration_seconds": round(b["duration_seconds"], 3),
            "tool_exec_seconds": round(b["tool_exec_seconds"], 3),
            "file_change_seconds": round(b["file_change_seconds"], 3),
            "llm_seconds": round(b["llm_seconds"], 3),
            "item_count": b["item_count"],
        })
    out.sort(key=lambda d: -d["duration_seconds"])
    return out


def _cmd_summary(args: argparse.Namespace) -> int:
    ledger_path = Path(args.ledger) if args.ledger else _default_ledger_path()
    rows = _read_ledger(ledger_path)
    rows = _filter_since(rows, args.since)
    if args.codex_only:
        rows = [r for r in rows if (r.get("provider") or "") == "codex"]
    agg = _aggregate(rows, args.by)
    if args.json:
        print(json.dumps(agg))
        return 0
    headers = [args.by, "bursts", "rollouts", "dur_s", "tool_s", "file_s", "llm_s", "items"]
    out = []
    for b in agg:
        out.append([
            b["key"],
            str(b["bursts"]),
            f"{b['with_rollout']}/{b['bursts']}",
            _fmt_float(b["duration_seconds"]),
            _fmt_float(b["tool_exec_seconds"]),
            _fmt_float(b["file_change_seconds"]),
            _fmt_float(b["llm_seconds"]),
            _fmt_int(b["item_count"]),
        ])
    _print_table(headers, out)
    return 0


def _cmd_aggregate(args: argparse.Namespace) -> int:
    """Bulk endpoint for the viewer: returns aggregated JSON.

    Equivalent to summary --json but always emits JSON and is the canonical
    name when invoked by the viewer.
    """
    args.json = True
    return _cmd_summary(args)


def _category_for_row(row: dict) -> str:
    """Mirror viewer/server.js's ``categoryFor`` for the cost-rollup output.

    The cost ledger's ``role`` field is just ``worker``/``reviewer`` — but
    reviewer covers four sub-classes (paper/corr/sound verifiers + the
    review lane). Split them out so the per-(provider, category) breakdown
    matches what the viewer shows today.
    """
    role = str(row.get("role") or "?").strip()
    if role == "worker":
        return "worker"
    scope = str(row.get("scope") or "")
    for k in ("paper", "corr", "sound", "review"):
        if f":{k}:" in scope:
            return k
    return role


def _row_duration_seconds(row: dict) -> Optional[float]:
    try:
        d = float(row.get("duration_seconds") or 0)
    except (TypeError, ValueError):
        return None
    return d if d > 0 else None


def _row_llm_seconds_from_rollout(
    row: dict,
    *,
    events_cache: dict,
    rollout_path_cache: dict,
) -> Optional[float]:
    """Read llm_seconds from the rollout file for this row, or None."""
    sid = str(row.get("session_id") or "").strip()
    if not sid:
        return None
    if sid in rollout_path_cache:
        path_ = rollout_path_cache[sid]
    else:
        path_ = find_rollout_path(sid)
        rollout_path_cache[sid] = path_
    if path_ is None:
        return None
    events = events_cache.get(path_)
    if events is None:
        try:
            text = _read_rollout_text(path_)
        except RolloutReadError:
            return None
        events = []
        for rec in _iter_rollout_events(text):
            ts = _parse_ts(rec.get("timestamp"))
            if ts is None:
                continue
            events.append((ts, rec))
        events_cache[path_] = events
    duration = _row_duration_seconds(row)
    try:
        ts_start = float(row.get("ts_start") or 0) or None
    except (TypeError, ValueError):
        ts_start = None
    sliced = _slice_events_for_burst(
        events, ts_start=ts_start, duration_seconds=duration,
    )
    timing = _decompose(sliced, duration_seconds=duration)
    if not timing:
        return None
    llm = timing.get("llm_seconds")
    if llm is None or llm <= 0:
        return None
    return float(llm)


def _cmd_cost_rollup(args: argparse.Namespace) -> int:
    """Emit per-provider and per-(provider, category) model-burn aggregates.

    For each codex row, computes ``model_burn_5h_pct = β(phase, lane) ·
    llm_seconds``. ``llm_seconds`` comes from one of three sources, in
    priority order:

      1. **rollout** — read from the codex rollout file (the canonical
         signal). All cost-ledger rows after the ``--ephemeral``-drop
         (commit f3ec017, 2026-05-01 16:39 UTC) have rollouts.
      2. **backfill** — for rows whose rollout is missing (legacy
         pre-2026-05-01 verifier bursts that were written with
         ``--ephemeral``), estimate ``llm_seconds`` from ``duration_seconds
         × ratio_lane``, where ``ratio_lane`` is the empirical
         ``Σllm / Σduration`` over rollout-bearing rows in the same
         lane (verifier rows pool across paper+sound+corr because the
         workload is uniform within that class). Falls back to the
         provider-wide ratio if a lane has no rollout-bearing rows of
         its own.
      3. **none** — when no ratio is available (provider has zero
         rollouts) the row is skipped from cost attribution.

    The output reports counts at each level (``with_rollout``,
    ``backfilled``, ``n``) so the operator can tell what fraction of
    each bucket relied on backfill estimates.

    Output JSON shape::

        {
          "betas": {"_default": {"worker": ..., "verifier": ..., "review": ...}},
          "ratios": {"codex": {"worker": 0.97, "verifier": 0.92, ...}},
          "by_provider": [
            {"provider": "codex", "n": ..., "with_rollout": ...,
             "backfilled": ..., "model_burn_5h_pct": ...,
             "model_burn_5h_pct_rollout_only": ...}
          ],
          "by_provider_category": [...]
        }

    Caches rollout file reads by path within one invocation — multi-turn
    threads share one rollout, so dedup saves a sudo-cat per row.
    """
    ledger_path = Path(args.ledger) if args.ledger else _default_ledger_path()
    rows = _read_ledger(ledger_path)
    rows = _filter_since(rows, args.since)

    events_cache: dict[Path, list[tuple[float, dict]]] = {}
    rollout_path_cache: dict[str, Optional[Path]] = {}

    # Pass 1: read rollouts when available, collect per-row state and
    # accumulate per-lane (Σllm, Σduration) over rollout-bearing rows so
    # we can compute backfill ratios.
    #
    # Lane-class for backfill: verifier rows (paper, sound, corr) pool
    # together because they share the workload shape (heavy cached
    # context, short fresh user payload). Worker and review get their
    # own ratio. Provider-wide ratio is the final fallback.
    enriched: list[dict] = []
    lane_totals: dict[tuple[str, str], dict] = {}  # (provider, lane) → {llm, dur}
    provider_totals: dict[str, dict] = {}

    for row in rows:
        prov = (row.get("provider") or "?").strip() or "?"
        scope = row.get("scope") or ""
        lane = lane_for_scope(scope)
        beta = beta_for_scope(scope) if lane else None
        duration = _row_duration_seconds(row)
        llm = None
        if (row.get("provider") or "").strip().lower() == "codex" and beta is not None:
            llm = _row_llm_seconds_from_rollout(
                row,
                events_cache=events_cache,
                rollout_path_cache=rollout_path_cache,
            )
        enriched.append({
            "row": row,
            "provider": prov,
            "category": _category_for_row(row),
            "lane": lane,
            "beta": beta,
            "duration_s": duration,
            "llm_s_rollout": llm,
        })
        if llm is not None and duration is not None and lane is not None:
            key = (prov, lane)
            lt = lane_totals.setdefault(key, {"llm": 0.0, "dur": 0.0})
            lt["llm"] += llm
            lt["dur"] += duration
            pt = provider_totals.setdefault(prov, {"llm": 0.0, "dur": 0.0})
            pt["llm"] += llm
            pt["dur"] += duration

    # Compute lane and provider ratios (Σllm / Σduration). None when no
    # rollout-bearing rows exist.
    lane_ratio: dict[tuple[str, str], float] = {}
    for key, t in lane_totals.items():
        if t["dur"] > 0:
            lane_ratio[key] = t["llm"] / t["dur"]
    provider_ratio: dict[str, float] = {}
    for prov, t in provider_totals.items():
        if t["dur"] > 0:
            provider_ratio[prov] = t["llm"] / t["dur"]

    def _resolve_ratio(prov: str, lane: Optional[str]) -> Optional[float]:
        # Per-(provider, lane) → fall back to provider-wide if absent.
        if lane is not None and (prov, lane) in lane_ratio:
            return lane_ratio[(prov, lane)]
        return provider_ratio.get(prov)

    # Pass 2: bucketize. For each row, derive llm_seconds from rollout
    # when available, else backfill = duration × ratio. Track which
    # source each row used.
    by_provider: dict[str, dict] = {}
    by_provider_category: dict[tuple[str, str], dict] = {}

    def _bucket(d: dict, key: tuple) -> dict:
        if key not in d:
            d[key] = {
                "n": 0, "with_rollout": 0, "backfilled": 0,
                "model_burn_5h_pct": 0.0,
                "model_burn_5h_pct_rollout_only": 0.0,
                "estimated_llm_seconds_total": 0.0,
            }
        return d[key]

    for e in enriched:
        prov = e["provider"]
        cat = e["category"]
        bp = _bucket(by_provider, (prov,))
        bp.setdefault("provider", prov)
        bp["n"] += 1
        bpc = _bucket(by_provider_category, (prov, cat))
        bpc.setdefault("provider", prov)
        bpc.setdefault("category", cat)
        bpc["n"] += 1

        beta = e["beta"]
        if beta is None:
            continue

        llm = e["llm_s_rollout"]
        backfilled = False
        if llm is None:
            duration = e["duration_s"]
            ratio = _resolve_ratio(prov, e["lane"])
            if duration is not None and ratio is not None:
                llm = duration * ratio
                backfilled = True

        if llm is None:
            continue

        burn = float(beta) * float(llm)
        if backfilled:
            bp["backfilled"] += 1
            bpc["backfilled"] += 1
        else:
            bp["with_rollout"] += 1
            bpc["with_rollout"] += 1
            bp["model_burn_5h_pct_rollout_only"] += burn
            bpc["model_burn_5h_pct_rollout_only"] += burn
        bp["model_burn_5h_pct"] += burn
        bpc["model_burn_5h_pct"] += burn
        bp["estimated_llm_seconds_total"] += llm
        bpc["estimated_llm_seconds_total"] += llm

    def _round(b):
        out = dict(b)
        for k in ("model_burn_5h_pct", "model_burn_5h_pct_rollout_only",
                  "estimated_llm_seconds_total"):
            if k in out:
                out[k] = round(out[k], 3)
        return out

    # Surface ratios used for backfill so the operator can sanity-check them.
    ratios_out: dict[str, dict] = {}
    for (prov, lane), r in lane_ratio.items():
        ratios_out.setdefault(prov, {})[lane] = round(r, 4)
    for prov, r in provider_ratio.items():
        ratios_out.setdefault(prov, {})["_provider"] = round(r, 4)

    result = {
        "betas": {p: dict(v) for p, v in BETA_BY_PHASE_LANE.items()},
        "ratios": ratios_out,
        "by_provider": [_round(v) for v in by_provider.values()],
        "by_provider_category": [_round(v) for v in by_provider_category.values()],
    }
    print(json.dumps(result))
    return 0


def _cmd_strip_timing(args: argparse.Namespace) -> int:
    """One-shot: drop the `timing` key from every row of a cost-ledger.

    Atomic: write to a sibling .tmp file, fsync, rename. Refuses to run
    without --in-place to make it explicit; otherwise writes to the path
    in `--out`.
    """
    src = Path(args.ledger) if args.ledger else _default_ledger_path()
    if not src.is_file():
        print(f"error: ledger not found: {src}", file=sys.stderr)
        return 2
    if not args.in_place and not args.out:
        print("error: must pass --in-place or --out PATH", file=sys.stderr)
        return 2

    src_text = src.read_text(encoding="utf-8", errors="replace")
    out_lines: list[str] = []
    stripped = 0
    kept = 0
    for line in src_text.splitlines():
        if not line.strip():
            out_lines.append(line)
            continue
        try:
            obj = json.loads(line)
        except (json.JSONDecodeError, ValueError):
            out_lines.append(line)
            continue
        if isinstance(obj, dict) and "timing" in obj:
            obj.pop("timing", None)
            stripped += 1
        else:
            kept += 1
        out_lines.append(json.dumps(obj, ensure_ascii=False))
    new_text = "\n".join(out_lines) + ("\n" if src_text.endswith("\n") else "")

    if args.in_place:
        tmp = src.with_suffix(src.suffix + ".tmp.strip")
        tmp.write_text(new_text, encoding="utf-8")
        os.replace(tmp, src)
        target = src
    else:
        target = Path(args.out)
        target.write_text(new_text, encoding="utf-8")
    print(f"stripped timing from {stripped} rows; left {kept} rows untouched -> {target}")
    return 0


def _build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="trellis.codex_timing",
        description=(
            "On-demand codex burst timing decomposition from rollout files. "
            "Reads cost-ledger.jsonl + ~/.codex/sessions/.../rollout-*.jsonl"
        ),
    )
    sub = p.add_subparsers(dest="cmd", required=True)

    show = sub.add_parser("show", help="Print timing for a single burst.")
    show.add_argument("session_id", nargs="?", help="codex session id (UUID).")
    show.add_argument("--burst", help="Match ledger row by burst label / scope substring.")
    show.add_argument("--row", type=int, help="1-indexed from the latest ledger row (1 = latest).")
    show.add_argument("--ledger", help="Override cost-ledger.jsonl path.")
    show.add_argument("--json", action="store_true", help="Emit JSON instead of a table.")
    show.set_defaults(func=_cmd_show)

    lst = sub.add_parser("list", help="List recent bursts with their timing.")
    lst.add_argument("--since", help="ISO date or datetime; only rows ts >= this.")
    lst.add_argument("--limit", type=int, default=20, help="Max rows (default 20).")
    lst.add_argument("--ledger", help="Override cost-ledger.jsonl path.")
    lst.add_argument("--codex-only", action="store_true", help="Limit to provider=codex rows.")
    lst.add_argument("--json", action="store_true", help="Emit JSON instead of a table.")
    lst.set_defaults(func=_cmd_list)

    summ = sub.add_parser("summary", help="Aggregate timing rollups.")
    summ.add_argument("--since", help="ISO date or datetime; only rows ts >= this.")
    summ.add_argument("--by", choices=("provider", "role", "scope"), default="provider")
    summ.add_argument("--ledger", help="Override cost-ledger.jsonl path.")
    summ.add_argument("--codex-only", action="store_true", help="Limit to provider=codex rows.")
    summ.add_argument("--json", action="store_true", help="Emit JSON instead of a table.")
    summ.set_defaults(func=_cmd_summary)

    agg = sub.add_parser(
        "aggregate",
        help="JSON-only aggregation endpoint (used by the viewer).",
    )
    agg.add_argument("--since")
    agg.add_argument("--by", choices=("provider", "role", "scope"), default="provider")
    agg.add_argument("--ledger")
    agg.add_argument("--codex-only", action="store_true")
    agg.set_defaults(func=_cmd_aggregate, json=True)

    cost = sub.add_parser(
        "cost-rollup",
        help=(
            "JSON cost-rollup using the β model: per-(provider, category) "
            "and per-provider sums of model-predicted 5h_pct burn. "
            "Used by the viewer in place of per-burst quota-delta accounting."
        ),
    )
    cost.add_argument("--since")
    cost.add_argument("--ledger")
    cost.set_defaults(func=_cmd_cost_rollup)

    strip = sub.add_parser(
        "strip-timing-field",
        help="One-shot: drop the `timing` key from every row of a cost-ledger.",
    )
    strip.add_argument("--ledger", help="Path to cost-ledger.jsonl.")
    strip.add_argument("--in-place", action="store_true", help="Rewrite the ledger atomically.")
    strip.add_argument("--out", help="Write to this path instead of in-place.")
    strip.set_defaults(func=_cmd_strip_timing)

    return p


def main(argv: Optional[list[str]] = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)
    func = getattr(args, "func", None)
    if func is None:
        parser.print_help()
        return 2
    try:
        return int(func(args) or 0)
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
