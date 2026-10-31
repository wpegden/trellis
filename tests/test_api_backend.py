from __future__ import annotations

import copy
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import shutil
import subprocess
import threading
import time

import pytest

from trellis.adapters import ProviderConfig
from trellis.agents import api_backend as api
from trellis.burst import run_worker_burst, run_reviewer_burst
from trellis.config import SandboxConfig, _parse_provider_config, _parse_verifier_agents
from trellis.provider_budget import budget_exhaustion_detail


@pytest.fixture(autouse=True)
def isolated_credentials(monkeypatch, tmp_path):
    monkeypatch.setenv("TRELLIS_API_STATE_DIR", str(tmp_path / "api-state"))
    monkeypatch.setenv("TRELLIS_PROVIDERS_KEYS_FILE", str(tmp_path / "absent.env"))
    for provider, (_, _, name) in api.PROVIDERS.items():
        monkeypatch.setenv(name, "test-secret")
        monkeypatch.delenv(provider.upper() + "_BASE_URL", raising=False)


def response(content="done", calls=None, reason=None):
    message = {"role": "assistant", "content": content, "reasoning_content": "preserve exactly"}
    if calls:
        message["tool_calls"] = calls
    return {"choices": [{"message": message, "finish_reason": reason or ("tool_calls" if calls else "stop")}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120,
                      "prompt_cache_hit_tokens": 40, "completion_tokens_details": {"reasoning_tokens": 5}}}


def shell(command, ident="call-1"):
    return {"id": ident, "type": "function", "function": {
        "name": "shell", "arguments": json.dumps({"command": command})}}


def run(tmp_path, provider="deepseek", extra_args=None, **kwargs):
    return api.run(ProviderConfig(provider, extra_args=extra_args or []), "perform the task",
                   session_name="test", work_dir=tmp_path, **kwargs)


@pytest.mark.parametrize("provider", ["deepseek", "kimi"])
def test_real_http_tool_loop_preserves_reasoning_and_usage(provider, tmp_path):
    requests = []
    replies = [response(None, [shell("printf value > result.txt; cat result.txt")]), response()]

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            assert self.path == "/v1/chat/completions"
            assert self.headers["Authorization"] == "Bearer test-secret"
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            self.send_response(200)
            self.end_headers()
            self.wfile.write(json.dumps(replies[len(requests) - 1]).encode())

        def log_message(self, *args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        result = run(tmp_path, provider, ["--base-url", f"http://127.0.0.1:{server.server_port}/v1"])
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
    assert result.ok and result.provider_turn_completed
    assert (tmp_path / "result.txt").read_text() == "value"
    history = requests[1]["messages"]
    assert history[2] == replies[0]["choices"][0]["message"]
    assert history[3]["tool_call_id"] == "call-1"
    assert json.loads(history[3]["content"])["output"] == "value"
    assert result.usage["input_tokens"] == 200
    assert result.usage["total_tokens"] == 240
    assert result.usage["cached_input_tokens"] == 80
    assert result.usage["reasoning_tokens"] == 10
    assert "test-secret" not in result.transcript_path.read_text()
    from trellis.agents.tmux_backend import default_cost_ledger_path
    ledger = json.loads(default_cost_ledger_path(tmp_path).read_text().splitlines()[-1])
    assert ledger["usage"] == result.usage and ledger["cost_usd"] is None


def test_credentials_are_literal_and_environment_wins(monkeypatch, tmp_path):
    path = tmp_path / "keys.env"
    path.write_text("export KIMI_API_KEY='literal-$(touch should-not-exist)' # comment\nKIMI_BASE_URL=https://api.moonshot.cn/v1\n")
    monkeypatch.setenv("TRELLIS_PROVIDERS_KEYS_FILE", str(path))
    assert api.credentials("kimi") == ("test-secret", "https://api.moonshot.cn/v1")
    monkeypatch.delenv("KIMI_API_KEY")
    assert api.credentials("kimi")[0] == "literal-$(touch should-not-exist)"
    assert not (tmp_path / "should-not-exist").exists()


def test_missing_credentials_fail_without_request(monkeypatch, tmp_path):
    monkeypatch.delenv("DEEPSEEK_API_KEY")
    monkeypatch.setattr(api, "_completion", lambda *a, **k: pytest.fail("unexpected request"))
    result = run(tmp_path)
    assert not result.ok and "Missing DEEPSEEK_API_KEY" in result.error


@pytest.mark.parametrize("extra", [["--max-turns", "0"], ["--max-tokens", "-1"],
                                    ["--tool-timeout", "nan"], ["--unknown"], ["--max-turns"]])
def test_invalid_options_are_burst_failures(tmp_path, extra):
    result = run(tmp_path, extra_args=extra)
    assert not result.ok and result.error


@pytest.mark.parametrize("finish", ["length", "content_filter", None])
def test_truncated_or_filtered_response_never_executes_tools(monkeypatch, tmp_path, finish):
    reply = response(None, [shell("touch must-not-run")])
    reply["choices"][0]["finish_reason"] = finish
    monkeypatch.setattr(api, "_completion", lambda *a, **k: reply)
    result = run(tmp_path)
    assert not result.ok
    assert not (tmp_path / "must-not-run").exists()
    assert result.usage["total_tokens"] == 120


def test_malformed_tools_are_returned_to_model(monkeypatch, tmp_path):
    invalid = shell("touch must-not-run")
    invalid["function"]["arguments"] = "not-json"
    calls = []

    def complete(url, key, payload, **kwargs):
        calls.append(copy.deepcopy(payload))
        return response(None, [invalid]) if len(calls) == 1 else response()

    monkeypatch.setattr(api, "_completion", complete)
    result = run(tmp_path)
    assert result.ok
    assert "Invalid tool call" in calls[1]["messages"][-1]["content"]
    assert not (tmp_path / "must-not-run").exists()


def test_multiple_calls_and_no_reexecution_on_http_retry(monkeypatch, tmp_path):
    replies = [response(None, [shell("printf x >> result", "one"), shell("printf y >> result", "two")]), response()]
    monkeypatch.setattr(api, "_completion", lambda *a, **k: replies.pop(0))
    assert run(tmp_path).ok
    assert (tmp_path / "result").read_text() == "xy"


def test_fresh_done_marker_stops_additional_tools(monkeypatch, tmp_path):
    reply = response(None, [shell("printf done > receipt"), shell("touch must-not-run", "two")])
    monkeypatch.setattr(api, "_completion", lambda *a, **k: reply)
    result = run(tmp_path, done_file=tmp_path / "receipt")
    assert result.ok and not result.provider_turn_completed
    assert not (tmp_path / "must-not-run").exists()


def test_stale_marker_does_not_mask_failure(monkeypatch, tmp_path):
    (tmp_path / "receipt").write_text("stale")
    monkeypatch.setattr(api, "_completion", lambda *a, **k: response(None, [shell("true")]))
    result = run(tmp_path, extra_args=["--max-turns", "1"], done_file=tmp_path / "receipt")
    assert not result.ok and "max-turns" in result.error


@pytest.mark.parametrize("provider", ["deepseek", "kimi"])
def test_long_submission_uses_execution_allowance_not_implicit_turn_cap(monkeypatch, tmp_path, provider):
    # Goldberg task 118 performed 186 commands and 33 edits in submission 1.
    # Exercise a sequential exchange for each operation, preserving all results.
    operation_count = 219
    commands = []

    def complete(url, key, payload, **kwargs):
        history = payload["messages"]
        assert len(history) == 2 + 2 * len(commands)
        if commands:
            assert json.loads(history[-1]["content"])["output"] == commands[-1]
        if len(commands) == operation_count:
            return response()
        return response(None, [shell(f"operation-{len(commands)}", f"call-{len(commands)}")])

    def execute(command, **kwargs):
        commands.append(command)
        return {"exit_code": 0, "output": command}

    monkeypatch.setattr(api, "_completion", complete)
    monkeypatch.setattr(api, "_shell", execute)
    result = run(tmp_path, provider)
    assert result.ok and result.provider_turn_completed
    assert len(commands) == operation_count
    assert result.usage["requests"] == operation_count + 1


def test_uncapped_loop_stops_at_execution_deadline(monkeypatch, tmp_path):
    elapsed = 0
    commands = []

    def execute(command, **kwargs):
        nonlocal elapsed
        commands.append(command)
        elapsed += 1
        return {"exit_code": 0, "output": "done"}

    monkeypatch.setattr(api.time, "monotonic", lambda: elapsed)
    monkeypatch.setattr(api, "_completion", lambda *a, **k: response(None, [shell("true")]))
    monkeypatch.setattr(api, "_shell", execute)
    result = run(tmp_path, burst_timeout=3)
    assert not result.ok and not result.provider_turn_completed
    assert "timeout" in result.error
    assert len(commands) == result.usage["requests"] == 3


def test_shell_timeout_kills_descendants_and_bounds_output(tmp_path):
    result = api._shell("sleep 1; touch too-late", work_dir=tmp_path, role="worker", sandbox=None,
                        burst_home=None, timeout=0.05, max_output=100)
    assert result["timed_out"] and result["exit_code"] != 0
    result = api._shell("printf %01000d 0", work_dir=tmp_path, role="worker", sandbox=None,
                        burst_home=None, timeout=5, max_output=100)
    assert len(result["output"]) < 150 and "truncated" in result["output"]
    time.sleep(1.1)
    assert not (tmp_path / "too-late").exists()


def test_shell_has_no_provider_credentials(tmp_path):
    result = api._shell('printf "%s/%s" "${KIMI_API_KEY-unset}" "${DEEPSEEK_API_KEY-unset}"',
                        work_dir=tmp_path, role="worker", sandbox=None,
                        burst_home=None, timeout=5, max_output=100)
    assert result["output"] == "unset/unset"


def test_sandbox_launch_failure_never_falls_back(monkeypatch, tmp_path):
    monkeypatch.setattr(api, "_completion", lambda *a, **k: response(None, [shell("touch unsafe")]))
    def fail(*args, **kwargs):
        raise RuntimeError("bwrap unavailable")
    monkeypatch.setattr(api, "wrap_command", fail)
    result = run(tmp_path, sandbox=SandboxConfig())
    assert not result.ok and "bwrap unavailable" in result.error
    assert not (tmp_path / "unsafe").exists()


@pytest.mark.parametrize("provider", ["deepseek", "kimi"])
@pytest.mark.parametrize("dispatch", [run_worker_burst, run_reviewer_burst])
def test_dispatch_and_config_accept_new_providers(monkeypatch, tmp_path, provider, dispatch):
    config = _parse_provider_config({"provider": provider}, "worker")
    assert _parse_verifier_agents([{"provider": provider}])[0].provider == provider
    monkeypatch.setattr(api, "_completion", lambda *a, **k: response())
    result = dispatch(config, "test", work_dir=tmp_path, session_name="test", max_rate_limit_retries=0)
    assert result.ok and result.usage["provider"] == provider


@pytest.mark.parametrize("code,body,retries", [(401, "invalid key test-secret", 1),
    (402, "Insufficient Balance", 1), (429, "insufficient_balance", 1),
    (429, "Too Many Requests", 3), (503, "Unavailable", 3), (400, "bad request", 1)])
def test_http_errors_retry_only_transient_and_redact(monkeypatch, tmp_path, code, body, retries):
    import io
    import urllib.error
    attempts = []
    class Opener:
        def open(self, *a, **k):
            attempts.append(1)
            raise urllib.error.HTTPError("https://test", code, "error", {}, io.BytesIO(body.encode()))
    monkeypatch.setattr(api.urllib.request, "build_opener", lambda *a: Opener())
    monkeypatch.setattr(api.time, "sleep", lambda delay: None)
    result = run(tmp_path)
    assert not result.ok and len(attempts) == retries
    assert "test-secret" not in result.error + result.captured_output
    if code == 402 or "insufficient" in body:
        assert budget_exhaustion_detail(error=result.error)


def test_expired_burst_does_not_call_provider(monkeypatch, tmp_path):
    monkeypatch.setattr(api.urllib.request, "build_opener", lambda *a: pytest.fail("network call after deadline"))
    result = run(tmp_path, burst_timeout=0)
    assert not result.ok and "timeout" in result.error


def test_actual_bwrap_enforces_worker_scope(tmp_path):
    if not shutil.which("bwrap"):
        pytest.skip("bwrap unavailable")
    probe = subprocess.run(["bwrap", "--ro-bind", "/", "/", "--unshare-pid", "/bin/true"], capture_output=True)
    if probe.returncode:
        pytest.skip("bwrap namespaces unavailable")
    repo = tmp_path / "repo"
    repo.mkdir()
    subprocess.run(["git", "init", "-q", str(repo)], check=True)
    (repo / "Tablet").mkdir()
    (repo / "protected.txt").write_text("protected")
    home = tmp_path / "burst-home"
    home.mkdir()
    result = api._shell('printf ok > Tablet/result; if touch root-write 2>/dev/null; then exit 99; fi; cat protected.txt',
                        work_dir=repo, role="worker", sandbox=SandboxConfig(), burst_home=home,
                        timeout=30, max_output=1000)
    assert result["exit_code"] == 0, result
    assert (repo / "Tablet/result").read_text() == "ok"
    assert not (repo / "root-write").exists()


def test_http_retry_preserves_tool_history_without_reexecution(monkeypatch, tmp_path):
    import io
    import urllib.error
    requests = []
    replies = [response(None, [shell("printf x >> result")]), None, response()]
    class Opener:
        def open(self, req, **kwargs):
            requests.append(json.loads(req.data))
            reply = replies.pop(0)
            if reply is None:
                raise urllib.error.HTTPError(req.full_url, 429, "rate limit", {}, io.BytesIO(b"Too Many Requests"))
            return io.BytesIO(json.dumps(reply).encode())
    monkeypatch.setattr(api.urllib.request, "build_opener", lambda *a: Opener())
    monkeypatch.setattr(api.time, "sleep", lambda delay: None)
    assert run(tmp_path).ok
    assert requests[1] == requests[2]
    assert (tmp_path / "result").read_text() == "x"


def test_worker_failure_after_tools_is_not_replayed(monkeypatch, tmp_path):
    requests = []
    def complete(*args, **kwargs):
        requests.append(1)
        if len(requests) == 1:
            return response(None, [shell("printf x >> result")])
        raise api.BackendError("Provider HTTP 429: Too Many Requests")
    monkeypatch.setattr(api, "_completion", complete)
    original_sleep = time.sleep
    def no_retry_sleep(delay):
        # subprocess.wait also uses the shared time module for millisecond polls.
        assert delay < 1, "worker replayed after mutation"
        original_sleep(delay)
    monkeypatch.setattr("trellis.burst.time.sleep", no_retry_sleep)
    result = run_worker_burst(ProviderConfig("deepseek"), "test", work_dir=tmp_path,
                              session_name="test", max_rate_limit_retries=2)
    assert not result.ok and len(requests) == 2
    assert (tmp_path / "result").read_text() == "x"


def test_new_templates_select_only_their_provider():
    root = Path(__file__).resolve().parents[1]
    def bindings(value):
        if isinstance(value, dict):
            if "provider" in value and "model" in value:
                yield value
            for child in value.values():
                yield from bindings(child)
        elif isinstance(value, list):
            for child in value:
                yield from bindings(child)
    for provider in api.PROVIDERS:
        data = json.loads((root / f"examples/trellis.{provider}.config.json").read_text())
        specs = list(bindings(data))
        assert len(specs) >= 8
        assert all(spec["provider"] == provider for spec in specs)
        assert all(spec["model"] == api.PROVIDERS[provider][1] for spec in specs)


def test_context_resumes_and_fresh_resets(monkeypatch, tmp_path):
    histories = []
    def complete(url, key, payload, **kwargs):
        histories.append(copy.deepcopy(payload['messages']))
        return response('remembered')
    monkeypatch.setattr(api, '_completion', complete)
    first = run(tmp_path)
    second = run(tmp_path)
    third = run(tmp_path, fresh=True)
    assert first.provider_session_id == second.provider_session_id != third.provider_session_id
    assert len(histories[1]) == 4 and histories[1][2]['reasoning_content'] == 'preserve exactly'
    assert len(histories[2]) == 2


def test_verifiers_do_not_reuse_reviewer_context(monkeypatch, tmp_path):
    monkeypatch.setattr(api, '_completion', lambda *a, **k: response())
    first = run(tmp_path, role='reviewer', session_scope='lean:phase:review')
    verify = run(tmp_path, role='reviewer', session_scope='lean:phase:correspondence')
    again = run(tmp_path, role='reviewer', session_scope='lean:phase:review')
    assert first.provider_session_id == again.provider_session_id != verify.provider_session_id


def test_pending_tool_on_resume_is_not_executed_twice(monkeypatch, tmp_path):
    monkeypatch.setattr(api, '_completion', lambda *a, **k: response())
    first = run(tmp_path)
    from trellis.agents.api_sessions import checkpoint_path
    path = checkpoint_path(first.provider_session_id)
    state = json.loads(path.read_text())
    state['messages'].append(response(None, [shell('touch must-not-run')])['choices'][0]['message'])
    path.write_text(json.dumps(state))
    histories = []
    def complete(url, key, payload, **kwargs):
        histories.append(copy.deepcopy(payload['messages']))
        return response()
    monkeypatch.setattr(api, '_completion', complete)
    assert run(tmp_path).ok
    assert 'may have run' in histories[0][-2]['content']
    assert not (tmp_path / 'must-not-run').exists()


@pytest.mark.parametrize('provider', ['deepseek', 'kimi'])
def test_owned_clock_and_same_conversation_correction(provider, monkeypatch, tmp_path):
    requests = []
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            requests.append(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
            self.send_response(200)
            self.end_headers()
            self.wfile.write(json.dumps(response('submission completed')).encode())
        def log_message(self, *args):
            pass
    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    config = ProviderConfig(provider, api.PROVIDERS[provider][1], 'max',
        extra_args=['--base-url', f'http://127.0.0.1:{server.server_port}/v1'])
    home = tmp_path / 'home'
    home.mkdir()
    record = tmp_path / 'runtime/session.json'
    try:
        first = api.run(config, 'first submission', session_name='test', work_dir=tmp_path,
            burst_home=home, session_record=record, execution_record=tmp_path/'turn1/clock',
            require_turn_completion=True, burst_timeout=10)
        assert first.ok, first.error + first.captured_output
        second = api.run(config, 'correct the checker error', session_name='test', work_dir=tmp_path,
            burst_home=home, session_record=record, execution_record=tmp_path/'turn2/clock',
            require_turn_completion=True, exact_resume_thread_id=first.provider_session_id, burst_timeout=10)
        assert second.ok, second.error + second.captured_output
        assert first.provider_session_id == second.provider_session_id
        assert first.provider_execution['disposition'] == second.provider_execution['disposition'] == 'completed'
        assert requests[1]['messages'][1]['content'] == 'first submission'
        assert requests[1]['messages'][2]['reasoning_content'] == 'preserve exactly'
        assert requests[1]['messages'][3]['content'] == 'correct the checker error'
        from trellis.agents.api_sessions import checkpoint_path
        checkpoint_path(first.provider_session_id).unlink()
        with pytest.raises(RuntimeError, match='unavailable'):
            api.run(config, 'cannot replace', session_name='test', work_dir=tmp_path,
                burst_home=home, session_record=record, execution_record=tmp_path/'turn3/clock',
                require_turn_completion=True, exact_resume_thread_id=first.provider_session_id, burst_timeout=10)
        assert len(requests) == 2
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


@pytest.mark.parametrize('overflow', [False, True])
def test_compaction_archives_history_and_keeps_thread_and_tool_pairs(overflow, monkeypatch, tmp_path):
    from trellis.agents.api_sessions import conversation
    from trellis.agents.api_context import size
    config = ProviderConfig('deepseek', 'deepseek-v4-pro', 'max', extra_args=[
        '--context-tokens', '6000' if not overflow else '1000000', '--max-tokens', '256'])
    with conversation(config=config, work_dir=tmp_path, home=None, role='worker', scope='',
                      fresh=True, endpoint='https://api.deepseek.com/chat/completions') as (state, save):
        state['messages'] = [{'role': 'system', 'content': api.SYSTEM_PROMPT},
            {'role': 'user', 'content': 'original task: preserve important invariant'}]
        for i in range(8):
            state['messages'] += [response(None, [shell('true', str(i))])['choices'][0]['message'],
                {'role': 'tool', 'tool_call_id': str(i), 'content': 'old output ' * 200}]
        original = copy.deepcopy(state['messages'])
        thread_id = state['binding']['thread_id']
        save()
    requests = []
    failed = False
    def complete(url, key, payload, **kwargs):
        nonlocal failed
        requests.append(copy.deepcopy(payload))
        if overflow and not failed and 'tools' in payload:
            failed = True
            raise api.BackendError('Provider HTTP 400: context_length_exceeded')
        return response('Preserved invariant; files inspected. Continue with current task.' if 'tools' not in payload else 'done')
    monkeypatch.setattr(api, '_completion', complete)
    result = api.run(config, 'continue the task', session_name='test', work_dir=tmp_path)
    assert result.ok, result.error
    assert result.provider_session_id == thread_id
    assert any('tools' not in request for request in requests)
    history = requests[-1]['messages']
    assert size(history) < size(original)
    assert any('original task' in m.get('content', '') for m in history)
    pending = set()
    for message in history:
        if message['role'] == 'assistant':
            pending.update(call['id'] for call in message.get('tool_calls') or [])
        if message['role'] == 'tool':
            assert message['tool_call_id'] in pending
            pending.remove(message['tool_call_id'])
    assert not pending
    archives = list((tmp_path/'api-state').glob('*.archive-*.json'))
    assert len(archives) == 1
    assert json.loads(archives[0].read_text())['messages'][:-1] == original
    assert result.usage['requests'] == len(requests) - int(overflow)


@pytest.mark.parametrize('mode', ['deadline', 'disconnect', 'quota'])
def test_owned_clock_interruption_retains_context(mode, tmp_path):
    from trellis.agents.api_sessions import checkpoint_path
    calls = []
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            calls.append(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
            if len(calls) == 1:
                if mode == 'deadline':
                    time.sleep(2)
                elif mode == 'quota':
                    self.send_response(402)
                    self.end_headers()
                    self.wfile.write(b'{"error":"insufficient balance"}')
                return  # disconnect intentionally, no HTTP replay
            self.send_response(200)
            self.end_headers()
            self.wfile.write(json.dumps(response('recovered')).encode())
        def log_message(self, *args):
            pass
    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    config = ProviderConfig('kimi', 'kimi-k3', 'max', extra_args=['--base-url', f'http://127.0.0.1:{server.server_port}/v1'])
    home = tmp_path / 'home'; home.mkdir()
    record = tmp_path / 'runtime/session.json'
    try:
        failed = api.run(config, 'remember this interrupted task', session_name='test', work_dir=tmp_path,
            burst_home=home, session_record=record, execution_record=tmp_path/'turn1/clock',
            require_turn_completion=True, burst_timeout=0.5 if mode == 'deadline' else 10)
        assert not failed.ok and not failed.provider_turn_completed
        assert failed.provider_execution['disposition'] == ('expired' if mode == 'deadline' else 'interrupted')
        assert bool(failed.provider_execution.get('provider_budget')) == (mode == 'quota')
        assert checkpoint_path(failed.provider_session_id).is_file()
        if mode == 'deadline':
            return  # exhausted clock must never authorize a new turn
        resumed = api.run(config, 'continue', session_name='test', work_dir=tmp_path,
            burst_home=home, session_record=record, execution_record=tmp_path/'transport/clock',
            exact_resume_thread_id=failed.provider_session_id, require_turn_completion=True, burst_timeout=5)
        assert resumed.ok, resumed.error
        assert resumed.provider_session_id == failed.provider_session_id
        assert calls[1]['messages'][1]['content'] == 'remember this interrupted task'
    finally:
        server.shutdown(); server.server_close(); thread.join()


@pytest.mark.parametrize('provider', ['deepseek', 'kimi'])
@pytest.mark.parametrize('role,key', [('worker','worker'), ('source_adaptation_worker','source_adaptation_worker'),
    ('reviewer','reviewer'), ('reviewer','correspondence'), ('reviewer','soundness'), ('reviewer','faithfulness'),
    ('reviewer','substantiveness'), ('stuck_math_audit','stuck_math_audit'),
    ('source_adaptation_reviewer','source_adaptation_reviewer')])
def test_all_role_routes_retain_sandbox_and_instruction_selection(provider, role, key, monkeypatch, tmp_path):
    requests = []
    def complete(url, secret, payload, **kwargs):
        requests.append(copy.deepcopy(payload))
        return response()
    monkeypatch.setattr(api, '_completion', complete)
    config = ProviderConfig(provider)
    common = dict(session_name='roles', work_dir=tmp_path, base_instructions_key=key,
                  max_rate_limit_retries=0)
    if role in {'worker', 'source_adaptation_worker'}:
        result = run_worker_burst(config, 'Perform this role task', sandbox_role=role, **common)
    else:
        result = run_reviewer_burst(config, 'Perform this role task', role=role, **common)
    assert result.ok, result.error
    assert requests[0]['messages'][0]['content'] == api.role_instructions(key)
    assert requests[0]['model'] == api.PROVIDERS[provider][1]
    assert requests[0]['reasoning_effort'] == 'max'
