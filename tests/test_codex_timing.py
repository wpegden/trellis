"""Tests for trellis.codex_timing — on-demand codex burst timing."""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from trellis.codex_timing import (
    RolloutNotFoundError,
    _aggregate,
    _filter_since,
    _parse_ts,
    compute_timing_for_session,
    compute_timing_from_text,
    find_rollout_path,
)


# ---------------------------------------------------------------------------
# Synthetic rollout helpers
# ---------------------------------------------------------------------------


def _ev(ts: datetime, outer: str, payload: dict) -> str:
    rec = {"timestamp": ts.strftime("%Y-%m-%dT%H:%M:%S.") + f"{ts.microsecond // 1000:03d}Z",
           "type": outer, "payload": payload}
    return json.dumps(rec)


def _rollout_synthetic(*, with_apply_patch: bool = True, n_turns: int = 1) -> str:
    """Build a synthetic rollout with deterministic timings."""
    base = datetime(2026, 5, 1, 12, 0, 0, tzinfo=timezone.utc)
    lines = [
        _ev(base, "session_meta",
            {"id": "sid-test-12345"}),
    ]
    cursor = base
    for turn_n in range(n_turns):
        turn_id = f"turn-{turn_n}"
        cursor += timedelta(seconds=1)
        lines.append(_ev(cursor, "event_msg",
                          {"type": "task_started", "turn_id": turn_id}))
        # Tool exec: function_call -> ... 5s ... -> function_call_output
        cursor += timedelta(seconds=2)
        lines.append(_ev(cursor, "response_item",
                          {"type": "function_call", "name": "exec_command",
                           "call_id": f"call-A-{turn_n}", "arguments": "ls"}))
        cursor += timedelta(seconds=5)
        lines.append(_ev(cursor, "response_item",
                          {"type": "function_call_output",
                           "call_id": f"call-A-{turn_n}", "output": "ok"}))
        # Atomic agent_message (3s gap, contributes to llm via residual).
        cursor += timedelta(seconds=3)
        lines.append(_ev(cursor, "event_msg",
                          {"type": "agent_message", "message": "hi"}))
        if with_apply_patch:
            # File change: custom_tool_call apply_patch lasting 2s.
            cursor += timedelta(seconds=4)
            lines.append(_ev(cursor, "response_item",
                              {"type": "custom_tool_call", "name": "apply_patch",
                               "call_id": f"call-B-{turn_n}", "input": "*** Begin Patch"}))
            cursor += timedelta(seconds=2)
            lines.append(_ev(cursor, "response_item",
                              {"type": "custom_tool_call_output",
                               "call_id": f"call-B-{turn_n}", "output": "ok"}))
        cursor += timedelta(seconds=1)
        lines.append(_ev(cursor, "event_msg",
                          {"type": "task_complete", "turn_id": turn_id,
                           "duration_ms": int((cursor - base).total_seconds() * 1000)}))
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------
# Unit tests
# ---------------------------------------------------------------------------


def test_parse_ts_basic() -> None:
    ts = _parse_ts("2026-05-01T05:33:06.994Z")
    assert ts is not None
    assert abs(ts - 1777613586.994) < 1e-3


def test_parse_ts_invalid() -> None:
    assert _parse_ts(None) is None
    assert _parse_ts("") is None
    assert _parse_ts("not a date") is None


def test_decompose_pairs_function_call() -> None:
    text = _rollout_synthetic(with_apply_patch=True)
    out = compute_timing_from_text(text, duration_seconds=18.0)
    # tool_exec = 5s (exec_command bracket), file_change = 2s (apply_patch),
    # llm = 18 - 5 - 2 = 11s
    assert out["tool_exec_seconds"] == 5.0
    assert out["file_change_seconds"] == 2.0
    assert out["llm_seconds"] == 11.0
    # item_count: function_call_output (1) + custom_tool_call_output (1) +
    # agent_message (1) = 3
    assert out["item_count"] == 3


def test_decompose_no_apply_patch() -> None:
    text = _rollout_synthetic(with_apply_patch=False)
    out = compute_timing_from_text(text, duration_seconds=12.0)
    assert out["tool_exec_seconds"] == 5.0
    assert out["file_change_seconds"] == 0.0
    assert out["llm_seconds"] == pytest.approx(7.0, abs=0.01)


def test_llm_residual_floored_at_zero() -> None:
    """Residual is clamped to zero when buckets exceed declared duration."""
    text = _rollout_synthetic(with_apply_patch=True)
    # Pretend the burst was very short — residual should clamp.
    out = compute_timing_from_text(text, duration_seconds=1.0)
    assert out["tool_exec_seconds"] == 5.0
    assert out["file_change_seconds"] == 2.0
    assert out["llm_seconds"] == 0.0


def test_decompose_derives_duration_from_span() -> None:
    text = _rollout_synthetic(with_apply_patch=True)
    out = compute_timing_from_text(text, duration_seconds=None)
    # Duration should be derived from first->last event timestamp; ~18s.
    assert 17.5 <= out["duration_seconds"] <= 18.5
    # tool + file = 7s; llm = duration - 7
    assert out["llm_seconds"] == pytest.approx(out["duration_seconds"] - 7.0, abs=0.5)


def test_slice_to_single_turn() -> None:
    """Multi-turn rollout: slicing by ts_start picks the right turn."""
    text = _rollout_synthetic(n_turns=3, with_apply_patch=False)
    # The 2nd task_started lives at base + 1s + (one turn ~ 11s) + 1s ~ 13s
    # Each turn span: 1s gap + 2 + 5 + 3 + 1 = 12s; turn 0 starts at +1s,
    # turn 1 starts at turn0_end + 1s. Compute precisely:
    # turn 0: started=+1s, complete=+12s. turn 1: started=+13s, complete=+24s.
    base = datetime(2026, 5, 1, 12, 0, 0, tzinfo=timezone.utc).timestamp()
    out = compute_timing_from_text(
        text,
        ts_start=base + 13.0,  # near turn-1 task_started
        duration_seconds=11.0,
    )
    # Only one turn's tool_exec (5s) should count.
    assert out["tool_exec_seconds"] == 5.0


def test_aggregate_groups_by_key() -> None:
    rows = [
        {"provider": "codex", "role": "worker", "scope": "x",
         "session_id": "sid-test-12345", "duration_seconds": 18.0},
        {"provider": "codex", "role": "reviewer", "scope": "y",
         "session_id": "missing-sid", "duration_seconds": 50.0},
    ]
    # The aggregator depends on find_rollout_path, which won't find anything
    # for a synthetic test — assert that the bucket counts are sensible.
    agg = _aggregate(rows, "provider")
    assert len(agg) == 1
    assert agg[0]["bursts"] == 2
    assert agg[0]["with_rollout"] == 0  # no rollouts present
    assert agg[0]["duration_seconds"] == 68.0


def test_filter_since_iso_date() -> None:
    rows = [
        {"ts": 1000},
        {"ts": 1777612800},  # 2026-05-01T00:00:00Z
        {"ts": 1777699200},  # 2026-05-02T00:00:00Z
    ]
    out = _filter_since(rows, "2026-05-01")
    assert {r["ts"] for r in out} == {1777612800, 1777699200}


def test_find_rollout_path_uses_sessions_root(tmp_path: Path) -> None:
    """The locator honors a custom sessions root, glob-matches by session id suffix."""
    root = tmp_path / "sessions" / "2026" / "05" / "01"
    root.mkdir(parents=True)
    target = root / "rollout-2026-05-01T12-00-00-019xx111-test.jsonl"
    target.write_text("{}\n", encoding="utf-8")
    found = find_rollout_path("019xx111-test", sessions_root=tmp_path / "sessions")
    assert found == target


def test_find_rollout_path_returns_none_when_missing(tmp_path: Path) -> None:
    found = find_rollout_path("nope-no-such", sessions_root=tmp_path)
    assert found is None


def test_compute_timing_for_session_raises_when_missing(tmp_path: Path) -> None:
    with pytest.raises(RolloutNotFoundError):
        compute_timing_for_session(
            "no-such-session",
            sessions_root=tmp_path / "empty",
        )


def test_compute_timing_for_session_via_synthetic_root(tmp_path: Path) -> None:
    root = tmp_path / "sessions" / "2026" / "05" / "01"
    root.mkdir(parents=True)
    target = root / "rollout-2026-05-01T12-00-00-sid-test-12345.jsonl"
    target.write_text(_rollout_synthetic(), encoding="utf-8")
    out = compute_timing_for_session(
        "sid-test-12345",
        duration_seconds=18.0,
        sessions_root=tmp_path / "sessions",
    )
    assert out["tool_exec_seconds"] == 5.0
    assert out["file_change_seconds"] == 2.0
    assert out["llm_seconds"] == 11.0
    assert out["rollout_path"] == str(target)


# ---------------------------------------------------------------------------
# Integration test against the live codex sessions tree (best-effort).
# ---------------------------------------------------------------------------


def _live_ledger_session_id() -> str | None:
    """Pick the most recent worker session_id from the live example-run ledger.

    Worker rows are NOT ephemeral, so they almost always have a rollout.
    Returns None when the ledger isn't present (CI / dev box).
    """
    p = Path("${TRELLIS_ROOT:-/path/to/trellis}/math/example-run/.trellis/logs/cost-ledger.jsonl")
    if not p.is_file():
        return None
    rows = []
    for line in p.read_text(encoding="utf-8", errors="replace").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(obj, dict) and obj.get("provider") == "codex" and obj.get("role") == "worker":
            sid = obj.get("session_id")
            if sid:
                rows.append(obj)
    return str(rows[-1].get("session_id")) if rows else None


def test_integration_live_rollout() -> None:
    sid = _live_ledger_session_id()
    if not sid:
        pytest.skip("no live codex worker rollout available")
    try:
        out = compute_timing_for_session(sid)
    except RolloutNotFoundError:
        pytest.skip(f"rollout vanished or ephemeral for {sid}")
    # Sanity: the buckets should sum (approximately) to duration.
    duration = out.get("duration_seconds", 0.0)
    bucket_sum = (
        out.get("tool_exec_seconds", 0.0)
        + out.get("file_change_seconds", 0.0)
        + out.get("llm_seconds", 0.0)
    )
    assert duration >= 0
    assert bucket_sum <= duration + 1e-3
    assert out["item_count"] >= 0
