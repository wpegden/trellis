from __future__ import annotations

import io
import json
from types import SimpleNamespace

import pytest

from trellis.adapters import BurstResult, ProviderConfig
from trellis.agent_wrapper.executor import execute_agent_request
from trellis.agent_wrapper.protocol import AgentLane, SingleAgentRequest
from trellis.burst import run_with_retry
from trellis.provider_budget import ProviderBudgetExhausted, budget_exhaustion_detail


@pytest.mark.parametrize("output", [
    '{"type":"turn.failed","error":{"message":"You have hit your usage limit. Try again tomorrow.","type":"usage_limit_reached"}}',
    '{"type":"error","message":"insufficient_quota"}',
    '{"type":"result","is_error":true,"result":"Credit balance is too low"}',
    "You've hit your limit · resets 3pm (America/New_York)",
    "You have reached your usage limit. Your allowance resets tomorrow.",
    "API Error: daily quota exhausted; retry tomorrow",
    "insufficient_quota",
    "You exceeded your current quota, please check your plan and billing details.",
])
def test_provider_allowance_messages(output):
    assert budget_exhaustion_detail(output=output)


@pytest.mark.parametrize("output", [
    "429 RESOURCE_EXHAUSTED: please retry in 30 seconds",
    "MODEL_CAPACITY_EXHAUSTED: No capacity available for model foo",
    "rate_limit_error: too many tokens per minute",
    "context_length_exceeded",
    "budget_exhausted: wall-clock attempt deadline",
    "max_tokens reached",
    "authentication failed",
    '{"type":"item.completed","item":{"type":"agent_message","text":"Test usage_limit_reached handling"}}',
    '{"type":"result","is_error":false,"result":"Daily quota exhausted is handled now"}',
])
def test_other_failures_and_successful_messages_are_not_budget_exhaustion(output):
    assert not budget_exhaustion_detail(output=output)


def test_allowance_failure_does_not_spend_short_retry_budget(monkeypatch):
    result = BurstResult(False, 1, "You've hit your limit; resets tomorrow", 1.0)
    attempts = []
    monkeypatch.setattr("trellis.burst.time.sleep", lambda _: pytest.fail("must not sleep"))
    assert run_with_retry(lambda: attempts.append(1) or result) is result
    assert len(attempts) == 1


def test_transient_throttle_still_retries(monkeypatch):
    results = [BurstResult(False, 1, "429 too many requests", 1.0), BurstResult(True, 0, "", 1.0)]
    sleeps = []
    monkeypatch.setattr("trellis.burst.time.sleep", sleeps.append)
    assert run_with_retry(lambda: results.pop(0)).ok
    assert len(sleeps) == 1


@pytest.mark.parametrize("role", ["worker", "reviewer", "correspondence", "soundness", "paper-faithfulness", "stuck_math_audit"])
def test_all_agent_roles_raise_an_operational_pause(tmp_path, role):
    request = SingleAgentRequest(
        request_id="42", cycle=17, kind=role, burst_role=role,
        provider=ProviderConfig("codex", "test-model"), prompt="test",
        work_dir=tmp_path, state_dir=tmp_path / "state", session_name="test",
        lane=AgentLane(role), timeout_seconds=1,
    )
    runner = lambda *args, **kwargs: BurstResult(False, 1, "usage_limit_reached: resets tomorrow", 1.0)
    with pytest.raises(ProviderBudgetExhausted) as exc:
        execute_agent_request(request, worker_runner=runner, reviewer_runner=runner)
    assert exc.value.pause["provider"] == "codex"
    assert exc.value.pause["model"] == "test-model"
    assert exc.value.pause["role"] == role
    assert "resets tomorrow" in exc.value.pause["reason"]


def test_bridge_cli_uses_operational_envelope(monkeypatch):
    from trellis.runtime import bridge_cli

    monkeypatch.setattr(bridge_cli, "_read_request", lambda: {})
    monkeypatch.setattr(bridge_cli.BridgeCliRequest, "from_dict", lambda _: SimpleNamespace())
    def exhausted(_):
        raise ProviderBudgetExhausted(provider="claude", model=None, role="reviewer", detail="You've hit your limit")
    monkeypatch.setattr(bridge_cli, "handle_bridge_request", exhausted)
    output = io.StringIO()
    monkeypatch.setattr(bridge_cli.sys, "stdout", output)
    assert bridge_cli.main() == 3
    result = json.loads(output.getvalue())
    assert result["pause"]["kind"] == "provider_budget"
    assert "traceback" not in result


def test_interactive_allowance_failure_stops_after_idle_grace(monkeypatch):
    from trellis.agents import tmux_backend as tmb

    clock = [0.0]
    monkeypatch.setattr(tmb.time, "monotonic", lambda: clock[0])
    monkeypatch.setattr(tmb.time, "monotonic_ns", lambda: int(clock[0] * 1e9))
    monkeypatch.setattr(tmb.time, "sleep", lambda dt: clock.__setitem__(0, clock[0] + dt))
    monkeypatch.setattr(tmb, "pane_dead", lambda _: False)
    monkeypatch.setattr(tmb, "capture", lambda _: "You've hit your limit · resets 3pm\n> ")
    handle = SimpleNamespace(session="fake", provider="claude")
    assert tmb.wait_until_idle(handle, total_timeout=7200, min_stable_seconds=5400,
        apparent_stall_seconds=0, api_error_idle_seconds=60) == (False, "provider_budget_exhausted")
    assert 60 <= clock[0] < 65
