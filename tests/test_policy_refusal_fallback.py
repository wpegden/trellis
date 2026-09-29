"""Provider safety refusals stop without changing models or retrying."""

import json
from types import SimpleNamespace

from trellis import provider_policy
from trellis.adapters import BurstResult, ProviderConfig
from trellis.agents import codex_headless
from trellis.burst import run_with_retry
from trellis.runtime import bridge

REFUSAL = (
    "This content was flagged for possible cybersecurity risk. If this seems wrong, "
    "try rephrasing your request."
)


def _stream(*records) -> str:
    return "\n".join(json.dumps(r) for r in records)


def _refused(model: str) -> BurstResult:
    output = _stream(
        {"type": "thread.started", "thread_id": f"thread-{model}"},
        {"type": "item.completed", "item": {"id": "item_1", "type": "command_execution"}},
        {"type": "error", "message": REFUSAL},
        {"type": "turn.failed", "error": {"message": REFUSAL}},
    )
    return BurstResult(ok=False, exit_code=1, captured_output=output, duration_seconds=0.01,
                       error=f"codex exited 1; {REFUSAL}")


def _ok(model: str) -> BurstResult:
    return BurstResult(ok=True, exit_code=0, captured_output=model, duration_seconds=0.01)


def _ladder_config() -> ProviderConfig:
    return ProviderConfig(provider="codex", model="gpt-5.6-sol", effort="xhigh",
                          fallback_models=["gpt-daybreak-blue-latest", "gpt-5.6-terra"])


def test_refusal_stops_without_trying_next_ladder_model(monkeypatch):
    config = _ladder_config()
    models: list[str] = []
    sleeps: list[float] = []
    monkeypatch.setattr("trellis.burst.time.sleep", lambda d: sleeps.append(d))

    def fake_run() -> BurstResult:
        models.append(config.model)
        return _refused(config.model) if len(models) == 1 else _ok(config.model)

    result = run_with_retry(fake_run, config=config)
    assert not result.ok
    assert models == ["gpt-5.6-sol"]
    assert result.error.startswith(provider_policy.POLICY_REFUSAL_MARKER)
    assert sleeps == []
    assert config.effort == "xhigh"


def test_refusal_marks_result_and_stops_even_with_retry_budget_disabled(monkeypatch):
    config = _ladder_config()
    models: list[str] = []
    sleeps: list[float] = []
    monkeypatch.setattr("trellis.burst.time.sleep", lambda d: sleeps.append(d))

    def fake_run() -> BurstResult:
        models.append(config.model)
        return _refused(config.model)

    result = run_with_retry(fake_run, config=config, max_retries=0, max_post_agent_retries=0)
    assert not result.ok
    assert models == ["gpt-5.6-sol"]
    assert sleeps == []
    assert result.error.startswith(provider_policy.POLICY_REFUSAL_MARKER)
    for model in models:
        assert model in result.error
    assert REFUSAL in result.error


def test_refusal_without_ladder_returns_at_once(monkeypatch):
    config = ProviderConfig(provider="codex", model="gpt-5.6-sol")
    attempts = {"n": 0}
    sleeps: list[float] = []
    monkeypatch.setattr("trellis.burst.time.sleep", lambda d: sleeps.append(d))

    def fake_run() -> BurstResult:
        attempts["n"] += 1
        return _refused(config.model)

    result = run_with_retry(fake_run, config=config)
    assert attempts["n"] == 1
    assert sleeps == []
    assert result.error.startswith(provider_policy.POLICY_REFUSAL_MARKER)


def test_quoted_refusal_text_is_an_ordinary_failure(monkeypatch):
    # An agent that greps an old rollout prints the phrase inside tool output;
    # only structured provider records count as a refusal.
    config = _ladder_config()
    attempts = {"n": 0}
    sleeps: list[float] = []
    monkeypatch.setattr("trellis.burst.time.sleep", lambda d: sleeps.append(d))
    quoted = _stream({"type": "item.completed", "item": {
        "type": "command_execution", "aggregated_output": REFUSAL + " cyber_policy"}})

    def fake_run() -> BurstResult:
        attempts["n"] += 1
        return BurstResult(ok=False, exit_code=1, captured_output=quoted,
                           duration_seconds=0.01, error="codex exited 1")

    result = run_with_retry(fake_run, config=config, base_delay=0.01)
    assert not result.ok
    assert attempts["n"] == 2
    assert len(sleeps) == 1
    assert config.model == "gpt-5.6-sol"
    assert provider_policy.POLICY_REFUSAL_MARKER not in result.error


def test_codex_backend_detects_refusal_from_structured_records_only():
    assert codex_headless._detected_policy_refusal(_refused("m").captured_output)
    quoted = _stream({"type": "agent_message", "text": REFUSAL},
                     {"type": "item.completed", "item": {"type": "command_execution",
                                                         "aggregated_output": "cyber_policy"}})
    assert not codex_headless._detected_policy_refusal(quoted)
    assert provider_policy.is_policy_refusal("", error=f"codex exited 1; {REFUSAL}")
    assert not provider_policy.is_policy_refusal(quoted, error="codex exited 1")


def test_bridge_breaker_halts_on_marked_provider_refusal(tmp_path):
    config = SimpleNamespace(repo_path=tmp_path)
    sentinel = tmp_path / ".trellis-stop-after-checkpoint"

    bridge._maybe_trip_policy_refusal_breaker(
        kind="stuck_math_audit", config=config, errors=[f"codex exited 1; {REFUSAL}"])
    assert not sentinel.exists()

    exhausted = (f"{provider_policy.POLICY_REFUSAL_MARKER}: provider content policy refused "
                 f"this burst on gpt-5.6-sol; operator review required; codex exited 1; {REFUSAL}")
    bridge._maybe_trip_policy_refusal_breaker(
        kind="stuck_math_audit", config=config, errors=[exhausted])
    body = sentinel.read_text()
    assert "policy-refusal" in body and "stuck_math_audit" in body
    assert "gpt-5.6-sol" in body


UNAVAILABLE = (
    "The 'gpt-daybreak-blue-latest' model is not supported when using Codex with a "
    "ChatGPT account."
)


def _unavailable(model: str) -> BurstResult:
    output = _stream(
        {"type": "thread.started", "thread_id": f"thread-{model}"},
        {"type": "error", "status": 400,
         "error": {"type": "invalid_request_error", "message": UNAVAILABLE}},
        {"type": "turn.failed", "error": {"message": UNAVAILABLE}},
    )
    return BurstResult(ok=False, exit_code=1, captured_output=output, duration_seconds=0.01,
                       error=f"codex exited 1; {UNAVAILABLE}")


def test_refusal_does_not_probe_fallback_model_availability(monkeypatch):
    """A refusal ends the burst before any fallback availability probe."""
    config = _ladder_config()
    models: list[str] = []
    sleeps: list[float] = []
    monkeypatch.setattr("trellis.burst.time.sleep", lambda d: sleeps.append(d))

    def fake_run() -> BurstResult:
        models.append(config.model)
        if len(models) == 1:
            return _refused(config.model)
        if len(models) == 2:
            return _unavailable(config.model)
        return _ok(config.model)

    result = run_with_retry(fake_run, config=config)
    assert not result.ok
    assert models == ["gpt-5.6-sol"]
    assert sleeps == []


def test_unused_fallbacks_do_not_appear_in_refusal_diagnostic(monkeypatch):
    config = _ladder_config()
    models: list[str] = []
    monkeypatch.setattr("trellis.burst.time.sleep", lambda d: None)

    def fake_run() -> BurstResult:
        models.append(config.model)
        return _refused(config.model) if len(models) == 1 else _unavailable(config.model)

    result = run_with_retry(fake_run, config=config, max_retries=0, max_post_agent_retries=0)
    assert not result.ok
    assert models == ["gpt-5.6-sol"]
    assert result.error.startswith(provider_policy.POLICY_REFUSAL_MARKER)
    assert "gpt-5.6-sol" in result.error and "gpt-daybreak-blue-latest" not in result.error


def test_unavailable_primary_model_is_an_ordinary_failure(monkeypatch):
    """Without a preceding refusal the ladder is not walked: an unavailable
    primary is a configuration error and surfaces as a plain failure."""
    config = _ladder_config()
    models: list[str] = []
    monkeypatch.setattr("trellis.burst.time.sleep", lambda d: None)

    def fake_run() -> BurstResult:
        models.append(config.model)
        return _unavailable(config.model)

    result = run_with_retry(fake_run, config=config, max_retries=0, max_post_agent_retries=0)
    assert not result.ok
    assert set(models) == {"gpt-5.6-sol"}
    assert not result.error.startswith(provider_policy.POLICY_REFUSAL_MARKER)
