from __future__ import annotations

import sys
import types

from trellis.adapters import BurstResult, ProviderConfig
from trellis.burst import run_reviewer_burst, run_with_retry, run_worker_burst


def _ok_result(tag: str) -> BurstResult:
    return BurstResult(
        ok=True,
        exit_code=0,
        captured_output=tag,
        duration_seconds=0.01,
    )


def test_run_worker_burst_routes_gemini_to_tmux(
    monkeypatch, tmp_path
) -> None:
    calls: list[str] = []
    prep_calls: list[str] = []

    def fake_script_run(*args, **kwargs):
        calls.append("script")
        return _ok_result("script")

    def fake_tmux_run(*args, **kwargs):
        calls.append("tmux")
        return _ok_result("tmux")

    monkeypatch.setitem(
        sys.modules,
        "trellis.agents.script_headless",
        types.SimpleNamespace(run=fake_script_run),
    )
    monkeypatch.setitem(
        sys.modules,
        "trellis.agents.tmux_backend",
        types.SimpleNamespace(run=fake_tmux_run),
    )
    monkeypatch.setattr(
        "trellis.burst._maybe_prepare_gemini_auth",
        lambda *args, **kwargs: prep_calls.append("prepare"),
    )

    result = run_worker_burst(
        ProviderConfig(provider="gemini", model="gemini-3.1-pro-preview"),
        "prompt",
        session_name="worker-session",
        work_dir=tmp_path,
        done_file=tmp_path / "worker.done",
        max_rate_limit_retries=0,
    )

    assert result.ok
    assert prep_calls == ["prepare"]
    assert calls == ["tmux"]


def test_run_reviewer_burst_routes_gemini_to_tmux(
    monkeypatch, tmp_path
) -> None:
    calls: list[str] = []
    prep_calls: list[str] = []

    def fake_script_run(*args, **kwargs):
        calls.append("script")
        return _ok_result("script")

    def fake_tmux_run(*args, **kwargs):
        calls.append("tmux")
        return _ok_result("tmux")

    monkeypatch.setitem(
        sys.modules,
        "trellis.agents.script_headless",
        types.SimpleNamespace(run=fake_script_run),
    )
    monkeypatch.setitem(
        sys.modules,
        "trellis.agents.tmux_backend",
        types.SimpleNamespace(run=fake_tmux_run),
    )
    monkeypatch.setattr(
        "trellis.burst._maybe_prepare_gemini_auth",
        lambda *args, **kwargs: prep_calls.append("prepare"),
    )

    result = run_reviewer_burst(
        ProviderConfig(provider="gemini", model="gemini-3.1-pro-preview"),
        "prompt",
        session_name="reviewer-session",
        work_dir=tmp_path,
        done_file=tmp_path / "review.done",
        max_rate_limit_retries=0,
    )

    assert result.ok
    assert prep_calls == ["prepare"]
    assert calls == ["tmux"]


def test_run_worker_burst_keeps_claude_on_tmux(monkeypatch, tmp_path) -> None:
    calls: list[str] = []

    def fake_script_run(*args, **kwargs):
        calls.append("script")
        return _ok_result("script")

    def fake_tmux_run(*args, **kwargs):
        calls.append("tmux")
        return _ok_result("tmux")

    monkeypatch.setitem(
        sys.modules,
        "trellis.agents.script_headless",
        types.SimpleNamespace(run=fake_script_run),
    )
    monkeypatch.setitem(
        sys.modules,
        "trellis.agents.tmux_backend",
        types.SimpleNamespace(run=fake_tmux_run),
    )

    result = run_worker_burst(
        ProviderConfig(provider="claude", model="claude-opus-4-6", effort="max"),
        "prompt",
        session_name="worker-session",
        work_dir=tmp_path,
        done_file=tmp_path / "worker.done",
        max_rate_limit_retries=0,
    )

    assert result.ok
    assert calls == ["tmux"]


def test_run_worker_burst_passes_startup_timeout_to_tmux(
    monkeypatch, tmp_path
) -> None:
    captured: dict[str, object] = {}

    def fake_tmux_run(*args, **kwargs):
        captured.update(kwargs)
        return _ok_result("tmux")

    monkeypatch.setitem(
        sys.modules,
        "trellis.agents.tmux_backend",
        types.SimpleNamespace(run=fake_tmux_run),
    )

    result = run_worker_burst(
        ProviderConfig(provider="gemini", model="gemini-3.1-pro-preview"),
        "prompt",
        session_name="worker-session",
        work_dir=tmp_path,
        done_file=tmp_path / "worker.done",
        startup_timeout_seconds=123.0,
        max_rate_limit_retries=0,
    )

    assert result.ok
    assert captured["startup_timeout"] == 123.0


def test_run_reviewer_burst_passes_startup_timeout_to_tmux(
    monkeypatch, tmp_path
) -> None:
    captured: dict[str, object] = {}

    def fake_tmux_run(*args, **kwargs):
        captured.update(kwargs)
        return _ok_result("tmux")

    monkeypatch.setitem(
        sys.modules,
        "trellis.agents.tmux_backend",
        types.SimpleNamespace(run=fake_tmux_run),
    )

    result = run_reviewer_burst(
        ProviderConfig(provider="claude", model="claude-opus-4-6", effort="max"),
        "prompt",
        session_name="reviewer-session",
        work_dir=tmp_path,
        done_file=tmp_path / "review.done",
        startup_timeout_seconds=234.0,
        max_rate_limit_retries=0,
    )

    assert result.ok
    assert captured["startup_timeout"] == 234.0


def test_run_with_retry_retries_gemini_send_fail_without_fallback_models(
    monkeypatch,
) -> None:
    attempts = {"count": 0}
    sleeps: list[float] = []

    def fake_run() -> BurstResult:
        attempts["count"] += 1
        if attempts["count"] == 1:
            return BurstResult(
                ok=False,
                exit_code=None,
                captured_output="429 RESOURCE_EXHAUSTED MODEL_CAPACITY_EXHAUSTED",
                duration_seconds=0.01,
                error="Failed to send message",
            )
        return _ok_result("retried")

    monkeypatch.setattr("trellis.burst.time.sleep", lambda delay: sleeps.append(delay))

    result = run_with_retry(
        fake_run,
        max_retries=1,
        rate_limit_delay=0.01,
        config=ProviderConfig(provider="gemini", model="gemini-3.1-pro-preview"),
    )

    assert result.ok
    assert attempts["count"] == 2
    assert sleeps == [0.01]


def test_run_with_retry_retries_gemini_send_fail_from_bare_error_without_fallback_models(
    monkeypatch,
) -> None:
    attempts = {"count": 0}
    sleeps: list[float] = []

    def fake_run() -> BurstResult:
        attempts["count"] += 1
        if attempts["count"] == 1:
            return BurstResult(
                ok=False,
                exit_code=None,
                captured_output="",
                duration_seconds=0.01,
                error="Failed to send message",
            )
        return _ok_result("retried")

    monkeypatch.setattr("trellis.burst.time.sleep", lambda delay: sleeps.append(delay))

    result = run_with_retry(
        fake_run,
        max_retries=1,
        rate_limit_delay=0.01,
        config=ProviderConfig(provider="gemini", model="gemini-3.1-pro-preview"),
    )

    assert result.ok
    assert attempts["count"] == 2
    assert sleeps == [0.01]


def test_run_with_retry_switches_gemini_model_on_capacity_exhaustion(
    monkeypatch,
) -> None:
    attempts = {"count": 0}

    class FakeAvailability:
        def __init__(self) -> None:
            self.marked: list[tuple[str, str]] = []

        def mark_unavailable(self, model: str, reason: str) -> None:
            self.marked.append((model, reason))

        def pick_available(self, candidates):
            assert candidates == ["gemini-3.1-pro", "gemini-3.1-flash"]
            return "gemini-3.1-flash"

        def status(self):
            return {}

    availability = FakeAvailability()

    def fake_run() -> BurstResult:
        attempts["count"] += 1
        if attempts["count"] == 1:
            return BurstResult(
                ok=False,
                exit_code=None,
                captured_output=(
                    '{"error":{"code":429,"status":"RESOURCE_EXHAUSTED",'
                    '"details":[{"reason":"MODEL_CAPACITY_EXHAUSTED",'
                    '"model":"gemini-3.1-pro-preview"}]}}'
                ),
                duration_seconds=0.01,
                error="Failed to send message",
            )
        return _ok_result("fallback")

    monkeypatch.setattr("trellis.burst.time.sleep", lambda *_args, **_kwargs: None)
    monkeypatch.setattr(
        "trellis.model_availability.get_availability",
        lambda: availability,
    )

    # No outer switch_model call any more — the tmux backend handles the
    # in-session model swap from its own `active_fallbacks` list on a
    # MODEL_CAPACITY_EXHAUSTED event (`run_gemini_burst`). The outer retry
    # wrapper just updates `config.model` so the next attempt picks it up.

    config = ProviderConfig(
        provider="gemini",
        model="gemini-3.1-pro-preview",
        fallback_models=["gemini-3.1-pro", "gemini-3.1-flash"],
    )
    result = run_with_retry(
        fake_run,
        max_retries=1,
        config=config,
        port=3298,
    )

    assert result.ok
    assert attempts["count"] == 2
    assert availability.marked == [
        ("gemini-3.1-pro-preview", "429 capacity exhausted")
    ]
    assert config.model == "gemini-3.1-flash"
