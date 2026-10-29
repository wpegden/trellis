"""A slow tmux health query must not abort an otherwise live headless burst."""

import json
import subprocess
from types import SimpleNamespace

import pytest

from trellis import burst
from trellis.adapters import ProviderConfig
from trellis.agents import codex_headless, script_headless, tmux_backend


def test_pane_probe_timeout_is_unknown_then_observes_exit(monkeypatch, caplog):
    replies = iter([subprocess.TimeoutExpired("tmux", 30),
                    SimpleNamespace(returncode=0, stdout="1")])

    def query(*args, **kwargs):
        reply = next(replies)
        if isinstance(reply, Exception):
            raise reply
        return reply

    monkeypatch.setattr(burst, "tmux_cmd", query)
    assert not burst.tmux_pane_is_dead("%9")
    assert "timed out" in caplog.text
    assert burst.tmux_pane_is_dead("%9")


@pytest.mark.parametrize("returncode,stdout,expected", [(0, "0", False),
                                                     (0, "1", True),
                                                     (1, "", True)])
def test_pane_probe_preserves_definitive_results(monkeypatch, returncode, stdout, expected):
    monkeypatch.setattr(burst, "tmux_cmd", lambda *a, **kw:
                        SimpleNamespace(returncode=returncode, stdout=stdout))
    assert burst.tmux_pane_is_dead("%9") is expected


def test_session_teardown_timeout_does_not_discard_burst_result(monkeypatch, caplog):
    def timeout(*args, **kwargs):
        raise subprocess.TimeoutExpired("tmux", 30)

    monkeypatch.setattr(burst, "tmux_cmd", timeout)
    burst.tmux_kill_session("fixture")
    assert "fixture" in caplog.text
    assert "timed out" in caplog.text


@pytest.mark.parametrize("backend,provider", [(codex_headless, "codex"),
                                             (script_headless, "gemini")])
@pytest.mark.parametrize("during_startup", [False, True])
@pytest.mark.parametrize("exit_code", [0, 1])
def test_burst_survives_probe_timeout_and_preserves_real_exit(
    monkeypatch, tmp_path, backend, provider, during_startup, exit_code,
):
    logs = tmp_path / "logs"
    clock = [0.0]
    probed = []
    killed = []

    def sleep(seconds):
        clock[0] += seconds
        if probed:
            (logs / "worker.started").write_text("started")
            (logs / "worker.exit").write_text(str(exit_code))
            (logs / "worker-output.log").write_text(json.dumps({
                "type": "turn.failed", "error": {"message": "fixture provider failure"},
            }) if exit_code else "")

    def tmux(*args, **kwargs):
        if args[0] == "new-window":
            return SimpleNamespace(returncode=0, stdout="@9 %9\n", stderr="")
        if args[0] == "send-keys" and not during_startup:
            (logs / "worker.started").write_text("started")
        if args[0] == "display-message":
            probed.append(args)
            clock[0] += 30
            raise subprocess.TimeoutExpired("tmux", 30)
        if args[0] == "kill-session":
            assert (logs / "worker.exit").exists(), "must not kill a live burst"
            killed.append(args)
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(burst, "tmux_cmd", tmux)
    monkeypatch.setattr(burst, "tmux_ensure_session", lambda *a: None)
    monkeypatch.setattr(burst, "tmux_kill_window", lambda *a: None)
    monkeypatch.setattr(backend.time, "monotonic", lambda: clock[0])
    monkeypatch.setattr(backend.time, "sleep", sleep)
    monkeypatch.setattr(tmux_backend, "_submit_probe_for_burst", lambda *a, **kw: None)
    monkeypatch.setattr(tmux_backend, "append_cost_ledger", lambda *a, **kw: None)

    result = backend.run(ProviderConfig(provider=provider, model="fixture"), "fixture",
                         role="worker", session_name="fixture", work_dir=tmp_path,
                         log_dir=logs, startup_timeout=60)
    assert len(probed) == 1
    assert killed
    assert result.exit_code == exit_code
    assert result.ok == (exit_code == 0)
    if exit_code:
        assert "fixture provider failure" in result.captured_output
        if backend is codex_headless:
            assert "fixture provider failure" in result.error
