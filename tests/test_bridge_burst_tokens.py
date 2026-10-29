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


@pytest.mark.parametrize('fails', [False, True])
def test_maintenance_cleanup_preserves_supervisor_runtime_stamp(tmp_path, fails):
    from trellis.remote_resume import maintenance_token
    bridge_module._register_burst_token(tmp_path, token='supervisor', burst_id='supervisor',
        kind='supervisor', request_id=0, cycle=0)
    path = bridge_module._burst_tokens_path(tmp_path)
    try:
        with maintenance_token(tmp_path) as token:
            assert token in json.loads(path.read_text())['tokens']
            if fails:
                raise RuntimeError('maintenance failed')
    except RuntimeError as exc:
        assert fails and str(exc) == 'maintenance failed'
    registry = json.loads(path.read_text())
    assert registry['runtime_root'] == str(tmp_path.resolve())
    assert registry['tokens'] == ['supervisor']
    assert [entry['token'] for entry in registry['entries']] == ['supervisor']


def test_mint_burst_token_is_unique_and_url_safe() -> None:
    tokens = {bridge_module._mint_burst_token() for _ in range(64)}
    # 64 mints should collide with vanishingly small probability; if this
    # fails the entropy source is broken.
    assert len(tokens) == 64
    for tok in tokens:
        assert tok and isinstance(tok, str)
        # URL-safe alphabet: A-Za-z0-9_-
        assert all(c.isalnum() or c in "-_" for c in tok), tok


@pytest.mark.parametrize('operation', ['register', 'remove', 'write'])
def test_token_writers_reject_foreign_runtime_without_changing_registry(tmp_path, operation):
    path = bridge_module._burst_tokens_path(tmp_path)
    path.parent.mkdir(parents=True)
    foreign = dict(runtime_root=str(tmp_path / 'other'), tokens=['foreign'], entries=[])
    path.write_text(json.dumps(foreign))
    before = path.read_bytes()
    with pytest.raises(ValueError, match='does not belong to this runtime'):
        if operation == 'register':
            bridge_module._register_burst_token(tmp_path, token='new', burst_id='new',
                kind='worker', request_id=1, cycle=1)
        elif operation == 'remove':
            bridge_module._unregister_burst_token(tmp_path, token='foreign')
        else:
            bridge_module._atomic_write_burst_tokens(path, foreign)
    assert path.read_bytes() == before


def test_registration_does_not_adopt_unstamped_credentials(tmp_path):
    path = bridge_module._burst_tokens_path(tmp_path)
    path.parent.mkdir(parents=True)
    path.write_text(json.dumps(dict(tokens=['legacy'], entries=[
        dict(token='legacy', kind='supervisor', ts=1)])))
    bridge_module._register_burst_token(tmp_path, token='current', burst_id='new',
        kind='supervisor', request_id=1, cycle=1)
    registry = json.loads(path.read_text())
    assert registry['runtime_root'] == str(tmp_path.resolve())
    assert registry['tokens'] == ['current']


@pytest.mark.parametrize('first_operation', ['register', 'remove'])
def test_token_updates_serialize_read_modify_write(tmp_path, monkeypatch, first_operation):
    import threading
    from concurrent.futures import ThreadPoolExecutor
    def register(token):
        bridge_module._register_burst_token(tmp_path, token=token, burst_id=token,
            kind='worker', request_id=1, cycle=1)
    register('maintenance')
    loaded, release, second_started, second_loaded = (threading.Event() for _ in range(4))
    load = bridge_module._load_burst_tokens_file
    local = threading.local()
    def blocked_load(path):
        data = load(path)
        if local.first:
            loaded.set()
            assert release.wait(5)
        else:
            second_loaded.set()
        return data
    monkeypatch.setattr(bridge_module, '_load_burst_tokens_file', blocked_load)
    def update(first):
        local.first = first
        if not first:
            second_started.set()
        if (first_operation == 'register') == first:
            register('new-worker')
        else:
            bridge_module._unregister_burst_token(tmp_path, token='maintenance')
    with ThreadPoolExecutor(max_workers=2) as pool:
        first = pool.submit(update, True)
        try:
            assert loaded.wait(5)
            second = pool.submit(update, False)
            assert second_started.wait(5)
            assert not second_loaded.wait(.05)
        finally:
            release.set()
        first.result(timeout=5)
        second.result(timeout=5)
    registry = json.loads(bridge_module._burst_tokens_path(tmp_path).read_text())
    assert registry['tokens'] == ['new-worker']
    assert [entry['token'] for entry in registry['entries']] == ['new-worker']


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
        "runtime_root": str(runtime_root.resolve()),
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


def test_register_burst_token_refreshes_supervisor_entry_without_expiry(tmp_path: Path) -> None:
    runtime_root = tmp_path / "runtime"
    runtime_root.mkdir()
    seed = {
        "runtime_root": str(runtime_root.resolve()),
        "tokens": ["long-lived-supervisor"],
        "entries": [
            {
                "token": "long-lived-supervisor",
                "ts": 1.0,
                "burst_id": "supervisor",
                "kind": "supervisor",
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
        token="fresh-worker",
        burst_id="worker-c1-r1",
        kind="worker",
        request_id=1,
        cycle=1,
    )

    payload = json.loads(bridge_module._burst_tokens_path(runtime_root).read_text())
    assert payload["tokens"] == ["long-lived-supervisor", "fresh-worker"]
    assert payload["entries"][0]["ts"] > 1.0


def test_registering_new_supervisor_retires_prior_supervisor_token(tmp_path: Path) -> None:
    runtime_root = tmp_path / "runtime"
    runtime_root.mkdir()
    bridge_module._register_burst_token(
        runtime_root,
        token="old-supervisor",
        burst_id="supervisor-old",
        kind="supervisor",
        request_id=0,
        cycle=0,
    )
    bridge_module._register_burst_token(
        runtime_root,
        token="active-worker",
        burst_id="worker-c1-r1",
        kind="worker",
        request_id=1,
        cycle=1,
    )

    bridge_module._register_burst_token(
        runtime_root,
        token="new-supervisor",
        burst_id="supervisor-new",
        kind="supervisor",
        request_id=0,
        cycle=0,
    )

    payload = json.loads(bridge_module._burst_tokens_path(runtime_root).read_text())
    assert payload["tokens"] == ["active-worker", "new-supervisor"]
    assert [entry["kind"] for entry in payload["entries"]] == [
        "worker",
        "supervisor",
    ]


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
