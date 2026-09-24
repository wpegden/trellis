"""Per-burst tmux session teardown coverage for codex_headless.run().

The codex backend (the primary provider for live runs) had the same session-
husk leak as script_headless: each burst's tmux session kept its default
idle-bash window 0 after only the burst window was killed. The fix kills the
whole per-burst session at every teardown path, never before the burst window
has been captured.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import List, Tuple

import trellis.burst as burst
from trellis.adapters import ProviderConfig
from trellis.agents import codex_headless
from trellis.agents import tmux_backend


class _FakeProc:
    def __init__(self, returncode: int = 0, stdout: str = "", stderr: str = "") -> None:
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


def _install_tmux_spies(monkeypatch, *, calls, on_send_keys=None,
                        new_window_rc=0, send_keys_rc=0) -> None:
    monkeypatch.setattr(tmux_backend, "_submit_probe_for_burst", lambda *a, **kw: None)
    def fake_tmux_cmd(*args, check: bool = False, timeout: int = 10):
        calls.append(tuple(args))
        verb = args[0] if args else ""
        if verb == "new-window":
            return _FakeProc(returncode=new_window_rc, stdout="@9 %9\n")
        if verb == "send-keys":
            if on_send_keys is not None:
                on_send_keys()
            return _FakeProc(returncode=send_keys_rc)
        return _FakeProc(returncode=0)

    monkeypatch.setattr(burst, "tmux_cmd", fake_tmux_cmd)
    monkeypatch.setattr(burst, "tmux_ensure_session", lambda session: None)
    monkeypatch.setattr(burst, "tmux_kill_window", lambda session, window: None)
    monkeypatch.setattr(burst, "tmux_pane_is_dead", lambda pane_id: False)


def _run(monkeypatch, tmp_path: Path, session_name: str, **kwargs):
    kwargs.setdefault("startup_timeout", 5.0)
    kwargs.setdefault("burst_timeout", 5.0)
    return codex_headless.run(
        ProviderConfig(provider="codex", model="gpt-5.5"),
        "prompt body",
        role="worker",
        session_name=session_name,
        work_dir=tmp_path,
        log_dir=tmp_path / "logs",
        **kwargs,
    )


def test_success_path_kills_session_at_teardown(monkeypatch, tmp_path) -> None:
    calls: List[Tuple[str, ...]] = []
    log_dir = tmp_path / "logs"
    early = {"flag": False}

    def write_markers() -> None:
        if any(c[0] == "kill-session" for c in calls):
            early["flag"] = True
        (log_dir / "worker.started").write_text("now\n")
        (log_dir / "worker.exit").write_text("0\n")

    _install_tmux_spies(monkeypatch, calls=calls, on_send_keys=write_markers)
    result = _run(monkeypatch, tmp_path, "trellis-ns-kind-req1-worker")

    assert result.ok
    assert early["flag"] is False, "must not kill the session while the burst is live"
    ks = [c for c in calls if c[0] == "kill-session"]
    assert ks and ks[-1] == ("kill-session", "-t", "trellis-ns-kind-req1-worker")
    assert not any(c[0] == "kill-window" for c in calls)


def test_startup_timeout_kills_session(monkeypatch, tmp_path) -> None:
    calls: List[Tuple[str, ...]] = []
    _install_tmux_spies(monkeypatch, calls=calls, on_send_keys=None)
    result = _run(monkeypatch, tmp_path, "trellis-ns-kind-req2-worker", startup_timeout=0.01)

    assert not result.ok
    assert ("kill-session", "-t", "trellis-ns-kind-req2-worker") in calls


def test_launch_failure_kills_session(monkeypatch, tmp_path) -> None:
    calls: List[Tuple[str, ...]] = []
    _install_tmux_spies(monkeypatch, calls=calls, send_keys_rc=1)
    result = _run(monkeypatch, tmp_path, "trellis-ns-kind-req3-worker")

    assert not result.ok
    assert ("kill-session", "-t", "trellis-ns-kind-req3-worker") in calls


def test_json_values_in_output_do_not_crash_completed_burst(monkeypatch, tmp_path):
    calls = []
    log_dir = tmp_path / "logs"
    records = [
        "substantiveness:node:BandParentRotation:diagnostic",
        None, True, 17, [],
        {"type": "thread.started", "thread_id": "test-thread"},
        {"type": "turn.completed", "usage": "invalid"},
        {"type": "turn.completed", "usage": {"input_tokens": 5, "output_tokens": 2}},
    ]

    def write_markers():
        (log_dir / "worker.started").write_text("now\n")
        (log_dir / "worker.exit").write_text("0\n")
        (log_dir / "worker-output.log").write_text(
            "\n".join(json.dumps(record) for record in records) + "\n"
        )

    _install_tmux_spies(monkeypatch, calls=calls, on_send_keys=write_markers)
    result = _run(monkeypatch, tmp_path, "trellis-json-values-worker")
    assert result.ok
    assert result.usage["input_tokens"] == 5
    assert result.usage["output_tokens"] == 2


def test_context_overflow_detected_after_non_object_json():
    output = '\n'.join([
        json.dumps("diagnostic"), "null", "[]", "42", "true",
        json.dumps({"type": "error", "message": "context_length_exceeded"}),
    ])
    assert codex_headless._extract_thread_id(output) == ""
    assert codex_headless._detected_context_overflow(output)
