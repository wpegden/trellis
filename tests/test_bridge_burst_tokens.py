"""Tests for the bridge's per-burst HMAC token plumbing + dispatch log.

Phases 2 + 3 of the bwrap-only migration plan
(SANDBOX_BWRAP_ONLY_MIGRATION_PLAN_2026-06-03.md §3):

- Phase 2: ``_mint_burst_token`` + ``_register_burst_token`` +
  ``_atomic_write_burst_tokens`` together produce a mode-0o600
  ``burst-tokens.json`` file whose ``tokens`` list the checker server
  reloads on every accept.
- Phase 3: ``_append_burst_dispatch_log`` emits append-only JSON-Lines
  attribution records at ``<runtime>/checker-state/burst-dispatch.jsonl``.

Both helpers are bridge-internal and run on the supervisor side, so
they can be exercised without a live tmux/agent stack. The live
``handle_bridge_request`` integration test lives in
``tests/test_runtime_bridge.py``.
"""

from __future__ import annotations

import json
import os
import stat
from pathlib import Path

import pytest

from trellis.runtime import bridge as bridge_module


def test_mint_burst_token_is_unique_and_url_safe() -> None:
    tokens = {bridge_module._mint_burst_token() for _ in range(64)}
    # 64 mints should collide with vanishingly small probability; if this
    # fails the entropy source is broken.
    assert len(tokens) == 64
    for tok in tokens:
        assert tok and isinstance(tok, str)
        # URL-safe alphabet: A-Za-z0-9_-
        assert all(c.isalnum() or c in "-_" for c in tok), tok


def test_register_burst_token_writes_atomic_0o600_file(tmp_path: Path) -> None:
    runtime_root = tmp_path / "runtime"
    runtime_root.mkdir()
    bridge_module._register_burst_token(
        runtime_root,
        token="tok-1",
        burst_id="worker-c1-r2",
        kind="worker",
        request_id=2,
        cycle=1,
    )
    path = bridge_module._burst_tokens_path(runtime_root)
    assert path.exists()
    mode = stat.S_IMODE(path.stat().st_mode)
    assert mode == 0o600, oct(mode)
    payload = json.loads(path.read_text())
    assert payload["tokens"] == ["tok-1"]
    assert len(payload["entries"]) == 1
    entry = payload["entries"][0]
    assert entry["token"] == "tok-1"
    assert entry["burst_id"] == "worker-c1-r2"
    assert entry["kind"] == "worker"
    assert entry["request_id"] == 2
    assert entry["cycle"] == 1


def test_register_burst_token_dedupes_repeated_token(tmp_path: Path) -> None:
    runtime_root = tmp_path / "runtime"
    runtime_root.mkdir()
    for _ in range(3):
        bridge_module._register_burst_token(
            runtime_root,
            token="dup",
            burst_id="b",
            kind="worker",
            request_id=1,
            cycle=1,
        )
    payload = json.loads(bridge_module._burst_tokens_path(runtime_root).read_text())
    assert payload["tokens"] == ["dup"]
    assert len(payload["entries"]) == 1


def test_register_burst_token_gcs_expired_entries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    runtime_root = tmp_path / "runtime"
    runtime_root.mkdir()
    # Seed an entry whose ts is older than the TTL.
    stale_ts = 1.0  # epoch start, well before "now"
    seed = {
        "tokens": ["stale"],
        "entries": [
            {
                "token": "stale",
                "ts": stale_ts,
                "burst_id": "old",
                "kind": "worker",
                "request_id": 0,
                "cycle": 0,
            }
        ],
    }
    bridge_module._atomic_write_burst_tokens(
        bridge_module._burst_tokens_path(runtime_root), seed
    )
    bridge_module._register_burst_token(
        runtime_root,
        token="fresh",
        burst_id="new",
        kind="worker",
        request_id=1,
        cycle=1,
    )
    payload = json.loads(bridge_module._burst_tokens_path(runtime_root).read_text())
    assert payload["tokens"] == ["fresh"], payload
    assert [e["token"] for e in payload["entries"]] == ["fresh"]


def test_register_burst_token_tolerates_corrupted_file(tmp_path: Path) -> None:
    runtime_root = tmp_path / "runtime"
    runtime_root.mkdir()
    path = bridge_module._burst_tokens_path(runtime_root)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{not valid json")
    # Must not raise; corrupted file is treated as empty and overwritten.
    bridge_module._register_burst_token(
        runtime_root,
        token="recovered",
        burst_id="b",
        kind="worker",
        request_id=1,
        cycle=1,
    )
    payload = json.loads(path.read_text())
    assert payload["tokens"] == ["recovered"]


def test_append_burst_dispatch_log_writes_jsonl_record(tmp_path: Path) -> None:
    runtime_root = tmp_path / "runtime"
    runtime_root.mkdir()
    bridge_module._append_burst_dispatch_log(
        runtime_root,
        burst_id="worker-c5-r9",
        kind="worker",
        request_id=9,
        cycle=5,
        bridge_pid=12345,
    )
    bridge_module._append_burst_dispatch_log(
        runtime_root,
        burst_id="paper-c5-r10",
        kind="paper",
        request_id=10,
        cycle=5,
        bridge_pid=12346,
    )
    log_path = bridge_module._burst_dispatch_log_path(runtime_root)
    lines = [
        json.loads(line)
        for line in log_path.read_text().splitlines()
        if line.strip()
    ]
    assert len(lines) == 2
    assert lines[0]["burst_id"] == "worker-c5-r9"
    assert lines[0]["kind"] == "worker"
    assert lines[0]["request_id"] == 9
    assert lines[0]["cycle"] == 5
    assert lines[0]["bridge_pid"] == 12345
    assert isinstance(lines[0]["ts_ns"], int)
    assert lines[1]["burst_id"] == "paper-c5-r10"


def test_append_burst_dispatch_log_passes_extra_fields(tmp_path: Path) -> None:
    runtime_root = tmp_path / "runtime"
    runtime_root.mkdir()
    bridge_module._append_burst_dispatch_log(
        runtime_root,
        burst_id="b",
        kind="worker",
        request_id=1,
        cycle=1,
        bridge_pid=os.getpid(),
        extra={"bwrap_pid": 99999, "session_name": "trellis-foo"},
    )
    record = json.loads(
        bridge_module._burst_dispatch_log_path(runtime_root).read_text().splitlines()[0]
    )
    assert record["bwrap_pid"] == 99999
    assert record["session_name"] == "trellis-foo"
