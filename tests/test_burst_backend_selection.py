from __future__ import annotations

import json
import sys
import types

import pytest

from trellis.adapters import BurstResult, ProviderConfig
from trellis.burst import is_rate_limited, run_reviewer_burst, run_with_retry, run_worker_burst


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


@pytest.mark.parametrize(
    ("activity", "terminal_error", "backend_error", "rate_limited"),
    [
        (False, "Selected model is at capacity. Please try a different model.", "", True),
        (True, "Selected model is at capacity. Please try a different model.", "", True),
        (True, "Permission denied", "", False),
        (False, "Permission denied", "", False),
        (False, "", "429 Too Many Requests", True),
        (True, "", "429 Too Many Requests", True),
    ],
)
def test_run_with_retry_uses_provider_errors_and_respects_agent_activity(
    monkeypatch, activity, terminal_error, backend_error, rate_limited,
) -> None:
    records = [{"type": "thread.started", "thread_id": "429"}, {"type": "turn.started"}]
    if activity:
        records.extend([
            {"type": "item.started", "item": {"type": "command_execution"}},
            {"type": "item.completed", "item": {
                "type": "command_execution",
                "aggregated_output": "inspect-ir-1789030429280601891 17642900107990496220#u64\n429 rate limit",
            }},
        ])
    if terminal_error:
        records.append({"type": "turn.failed", "error": {"message": terminal_error}})
    output = "\n".join(json.dumps(record) for record in records)
    assert is_rate_limited(output + "\n" + backend_error) is rate_limited
    assert not is_rate_limited("inspect-ir-1789030429280601891 17642900107990496220#u64")
    assert is_rate_limited("429")
    assert is_rate_limited(json.dumps({"type": "result", "is_error": True, "errors": ["rate limit"]}))
    assert is_rate_limited(json.dumps({"message": "RESOURCE_EXHAUSTED", "code": 429}))

    for post_agent_budget in (0, 1):
        attempts = []
        sleeps = []
        config = ProviderConfig(provider="codex", model="gpt-5.6-terra", effort="high")
        failure = BurstResult(ok=False, exit_code=1, captured_output=output,
                              duration_seconds=512, error=backend_error)

        def fake_run():
            attempts.append(config.model)
            return failure if len(attempts) == 1 else _ok_result("retried")

        monkeypatch.setattr("trellis.burst.time.sleep", sleeps.append)
        result = run_with_retry(fake_run, max_retries=1,
                                max_post_agent_retries=post_agent_budget,
                                base_delay=3, rate_limit_delay=7, config=config)
        pre_agent_retry = rate_limited and not activity
        retry = pre_agent_retry or post_agent_budget > 0
        assert result.ok is retry
        assert attempts == ["gpt-5.6-terra"] * (2 if retry else 1)
        assert sleeps == ([7 if pre_agent_retry else 3] if retry else [])
        if not retry:
            assert result is failure
