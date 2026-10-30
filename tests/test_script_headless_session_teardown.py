"""Per-burst tmux session teardown coverage for script_headless.run().

Regression guard for the session-husk leak: each burst created a tmux session
whose default idle-bash window 0 was never reaped (only the added burst window
was killed), so ~357 husk sessions accumulated. The fix kills the whole
per-burst session at every teardown path, and never before the burst window has
been captured.
"""

from __future__ import annotations

import types
from pathlib import Path
from typing import List, Tuple

import trellis.burst as burst
from trellis.adapters import ProviderConfig
from trellis.agents import script_headless


class _FakeProc:
    def __init__(self, returncode: int = 0, stdout: str = "", stderr: str = "") -> None:
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


def _install_tmux_spies(
    monkeypatch,
    *,
    calls: List[Tuple[str, ...]],
    on_send_keys=None,
    new_window_rc: int = 0,
    send_keys_rc: int = 0,
) -> None:
    """Patch the burst-module tmux helpers that script_headless imports.

    `calls` accumulates every tmux_cmd invocation as a tuple of its args so the
    test can assert ordering (no kill-session before capture) and that a
    kill-session is issued at teardown.
    """

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
    return script_headless.run(
        ProviderConfig(provider="gemini", model="gemini-test"),
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
    kill_session_seen_before_markers = {"flag": False}

    def write_markers() -> None:
        # Simulate the launcher producing start + exit markers. If any
        # kill-session already happened, that would be killing a live burst.
        if any(c[0] == "kill-session" for c in calls):
            kill_session_seen_before_markers["flag"] = True
        prefix = "worker"
        (log_dir / f"{prefix}.started").write_text("now\n")
        (log_dir / f"{prefix}.exit").write_text("0\n")

    _install_tmux_spies(monkeypatch, calls=calls, on_send_keys=write_markers)

    result = _run(monkeypatch, tmp_path, "trellis-ns-kind-req1-worker")

    assert result.ok
    assert result.exit_code == 0
    # Never tore down the session while the burst was live.
    assert kill_session_seen_before_markers["flag"] is False
    kill_session_calls = [c for c in calls if c[0] == "kill-session"]
    assert kill_session_calls, "teardown must kill the per-burst session"
    assert kill_session_calls[-1] == ("kill-session", "-t", "trellis-ns-kind-req1-worker")
    # The burst window itself is never explicitly killed via kill-window in
    # teardown anymore; the session kill reaps it together with window 0.
    assert not any(c[0] == "kill-window" for c in calls)


def test_startup_timeout_kills_session(monkeypatch, tmp_path) -> None:
    calls: List[Tuple[str, ...]] = []
    # send-keys does NOT write the start marker -> startup timeout path.
    _install_tmux_spies(monkeypatch, calls=calls, on_send_keys=None)

    result = _run(monkeypatch, tmp_path, "trellis-ns-kind-req2-worker", startup_timeout=0.01)

    assert not result.ok
    assert "startup marker" in result.error
    assert ("kill-session", "-t", "trellis-ns-kind-req2-worker") in calls


def test_launch_failure_kills_session(monkeypatch, tmp_path) -> None:
    calls: List[Tuple[str, ...]] = []
    _install_tmux_spies(monkeypatch, calls=calls, send_keys_rc=1)

    result = _run(monkeypatch, tmp_path, "trellis-ns-kind-req3-worker")

    assert not result.ok
    assert "launch agent window" in result.error
    assert ("kill-session", "-t", "trellis-ns-kind-req3-worker") in calls


def test_new_window_failure_leaves_no_dangling_window(monkeypatch, tmp_path) -> None:
    # When new-window itself fails, run() returns early before a window_id
    # exists; there is nothing to tear down and it must not crash.
    calls: List[Tuple[str, ...]] = []
    _install_tmux_spies(monkeypatch, calls=calls, new_window_rc=1)

    result = _run(monkeypatch, tmp_path, "trellis-ns-kind-req4-worker")

    assert not result.ok
    assert "create tmux window" in result.error


def test_tmux_kill_session_helper_is_best_effort(monkeypatch) -> None:
    recorded: List[Tuple[str, ...]] = []

    def fake_tmux_cmd(*args, check: bool = False, timeout: int = 10):
        recorded.append((args, check))
        return _FakeProc(returncode=1)  # simulate "no such session"

    monkeypatch.setattr(burst, "tmux_cmd", fake_tmux_cmd)
    # Must not raise even though the underlying command "failed".
    burst.tmux_kill_session("trellis-gone")
    assert recorded == [(("kill-session", "-t", "trellis-gone"), False)]
