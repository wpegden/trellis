"""Per-burst tmux session teardown coverage for codex_headless.run().

The codex backend (the primary provider for live runs) had the same session-
husk leak as script_headless: each burst's tmux session kept its default
idle-bash window 0 after only the burst window was killed. The fix kills the
whole per-burst session at every teardown path, never before the burst window
has been captured.
"""

from __future__ import annotations

import json
import pytest
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


def _clock_receipts(session, thread, disposition='completed'):
    from trellis.dispatch.candidate import canonical_json, digest
    from trellis.dispatch.publication import atomic
    directory = session.parent / 'worker-execution'
    common = dict(config_sha256=digest((directory/'config.json').read_bytes()))
    atomic(directory/'started.json', canonical_json(dict(**common, started_at_ms=10)))
    atomic(directory/'finished.json', canonical_json(dict(**common,
        thread_id=thread, disposition=disposition, elapsed_ms=20)))
    atomic(directory/'exited.json', canonical_json(dict(**common, returncode=0)))


def test_submission_continuation_uses_exact_thread_despite_fresh_policy(monkeypatch, tmp_path):
    from trellis.agent_wrapper.continuation import record_session, session_binding
    from trellis import codex_timing
    config = ProviderConfig(provider="codex", model="gpt-6.1-sol", effort="xhigh")
    home = tmp_path / 'home'
    session = tmp_path / 'host/session.json'
    record_session(session, session_binding(config, home, 'same-thread'))
    (tmp_path / 'rollout.jsonl').write_text('{}\n')
    monkeypatch.setattr(codex_timing, 'find_rollout_path', lambda *a, **kw: tmp_path / 'rollout.jsonl')
    monkeypatch.setattr(codex_headless, '_decide_worker_context_ab_arm',
                        lambda *a: pytest.fail('correction must not resample fresh policy'))
    monkeypatch.setattr(codex_headless, '_worker_always_fresh_enabled', lambda: True)
    build = codex_headless.build_script
    bindings = []
    def capture(*a, **kw):
        bindings.append((kw['fresh'], kw['resume_thread_id']))
        return build(*a, **kw)
    monkeypatch.setattr(codex_headless, 'build_script', capture)
    log = tmp_path / 'logs'
    def finish():
        _clock_receipts(session, 'same-thread')
        (log / 'worker.started').write_text('now')
        (log / 'worker.exit').write_text('0')
        (log / 'worker-output.log').write_text('\n'.join(json.dumps(x) for x in [
            {'type': 'thread.started', 'thread_id': 'same-thread'}, {'type': 'turn.completed'}]))
    calls = []
    _install_tmux_spies(monkeypatch, calls=calls, on_send_keys=finish)
    result = codex_headless.run(config, 'actual checker failure', role='worker',
        session_name='correction', work_dir=tmp_path, log_dir=log, burst_home=home,
        fresh=True, require_turn_completion=True, exact_resume_thread_id='same-thread',
        session_record=session, burst_timeout=5)
    assert result.ok and result.provider_turn_completed
    assert result.provider_session_id == 'same-thread'
    assert bindings == [(False, 'same-thread')]


def test_transport_resume_uses_new_segment_and_returns_summed_owned_execution(monkeypatch, tmp_path):
    """Host receipt fixture plus mocked tmux; no real provider is launched."""
    import os
    from trellis.agent_wrapper import provider_clock as clock
    from trellis.agent_wrapper.continuation import record_session, session_binding
    from trellis.dispatch.candidate import canonical_json, digest
    from trellis.dispatch.process import identity
    from trellis import codex_timing
    config = ProviderConfig(provider='codex', model='gpt-6-luna', effort='high')
    home, log = tmp_path / 'home', tmp_path / 'logs'
    log.mkdir()
    session = tmp_path / 'host/session.json'
    directory = session.parent / 'worker-execution'
    binding = session_binding(config, home, 'same-thread')
    record_session(session, binding)
    rollout = tmp_path / 'rollout.jsonl'
    rollout.write_text('{}\n')
    monkeypatch.setattr(codex_timing, 'find_rollout_path', lambda *a, **kw: rollout)
    from trellis.chat_history import ensure_chat_file_link
    output = ensure_chat_file_link(tmp_path, log_dir=log, artifact_prefix='worker',
        role='worker', log_filename='worker-output.log', canonical_name='output.log')
    original = clock.prepare(directory, allowance_ms=5000,
        binding=dict(binding, thread_id=''), output=output)
    common = dict(config_sha256=digest((directory / 'config.json').read_bytes()),
                  owner=dict(pid=2147483647, start_ticks=0, boot_id='fixture'))
    for name, values in {
        'started': dict(started_at_ms=10),
        'session': dict(thread_id='same-thread'),
        'finished': dict(disposition='interrupted', elapsed_ms=35, thread_id='same-thread',
            detail='Provider exited without a completed turn', provider_budget='usage_limit_reached'),
        'exited': dict(returncode=1),
    }.items():
        (directory / (name + '.json')).write_bytes(canonical_json(dict(common, **values)))
    process = identity(os.getpid())
    for name, value in {
        'process': process,
        'recipe': dict(payload=dict(worker_turn=dict(execution_record=str(directory)),
            runtime_root=str(session.parent),
            request=dict(worker_submission_policy=dict(max_infrastructure_retries=2)))),
        'provider-pause': dict(ordinal=1, process=process, resume_allowed=True),
        'provider-resume': dict(ordinal=1, process=process),
    }.items():
        (session.parent / (name + '.json')).write_bytes(canonical_json(value))
    # The production path uses this exact retained session filename.
    (session.parent / 'worker-session.json').write_text(session.read_text())
    output.write_text('earlier usage_limit_reached\n')
    def finish():
        assert output.read_text() == 'earlier usage_limit_reached\n'
        segment = directory / 'segments/2'
        remaining = json.loads((segment / 'config.json').read_text())['allowance_ms']
        assert remaining == original['allowance_ms'] - 35
        current = dict(config_sha256=digest((segment / 'config.json').read_bytes()))
        for name, values in {
            'started': dict(started_at_ms=1000),
            'session': dict(thread_id='same-thread'),
            'finished': dict(thread_id='same-thread', disposition='completed', elapsed_ms=20),
            'exited': dict(returncode=0),
        }.items():
            (segment / (name + '.json')).write_bytes(canonical_json(dict(current, **values)))
        with output.open('a') as stream:
            stream.write(json.dumps(dict(type='thread.started', thread_id='same-thread')) + '\n')
            stream.write(json.dumps(dict(type='turn.completed')) + '\n')
    calls = []
    _install_tmux_spies(monkeypatch, calls=calls, on_send_keys=finish)
    result = codex_headless.run(config, 'retained request', role='worker',
        session_name='transport-resume', work_dir=tmp_path, log_dir=log, burst_home=home,
        require_turn_completion=True, exact_resume_thread_id='same-thread',
        session_record=session, execution_record=directory, burst_timeout=5)
    assert result.ok and result.provider_turn_completed
    assert result.provider_session_id == 'same-thread'
    assert result.provider_execution['elapsed_ms'] == 55
    assert result.provider_execution['started_at_ms'] == 10
    assert result.provider_execution['transport_segments'] == 2
    assert 'earlier usage_limit_reached' in result.captured_output


def test_submission_done_marker_does_not_substitute_for_provider_completion(monkeypatch, tmp_path):
    calls = []
    log = tmp_path / 'logs'
    done = tmp_path / 'response.done'
    session = tmp_path / 'host/session.json'
    def finish():
        _clock_receipts(session, 'retained-thread', 'interrupted')
        (log / 'worker.started').write_text('now')
        (log / 'worker.exit').write_text('0')
        (log / 'worker-output.log').write_text(json.dumps({'type':'thread.started','thread_id':'retained-thread'}))
        done.write_text('worker claims done')
    _install_tmux_spies(monkeypatch, calls=calls, on_send_keys=finish)
    result = codex_headless.run(ProviderConfig(provider='codex', model='gpt-6-luna', effort='high'),
        'submit', session_name='first-turn', work_dir=tmp_path, log_dir=log,
        burst_home=tmp_path / 'home', require_turn_completion=True, session_record=session,
        done_file=done, burst_timeout=5)
    assert not result.ok and not result.provider_turn_completed
    assert 'did not complete' in result.error
    assert json.loads(session.read_text())['thread_id'] == 'retained-thread'


def test_submission_missing_context_never_launches_fresh(monkeypatch, tmp_path):
    calls = []
    _install_tmux_spies(monkeypatch, calls=calls)
    with pytest.raises(RuntimeError, match='no matching retained provider context'):
        codex_headless.run(ProviderConfig(provider='codex', model='gpt-6-luna', effort='high'),
            'repair', session_name='missing', work_dir=tmp_path, burst_home=tmp_path/'home',
            require_turn_completion=True, exact_resume_thread_id='lost-thread',
            session_record=tmp_path/'host/session.json', burst_timeout=5)
    assert calls == []


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


@pytest.mark.parametrize("message,reset_context", [
    ('Error running remote compact task: {"detail":"Bad Request"}', True),
    ("Your access token could not be refreshed because your refresh token was already used.", False),
    ("Error running remote compact task: Your access token could not be refreshed because your refresh token was revoked.", False),
    ("context_length_exceeded", True),
    ("This content was flagged for possible cybersecurity risk. If this seems wrong, try rephrasing your request.", True),
])
@pytest.mark.parametrize("exit_code", [0, 1])
def test_reviewer_preserves_error_and_recovers_failed_context(monkeypatch, tmp_path, message, reset_context, exit_code):
    config = ProviderConfig(provider="codex", model="gpt-5.6-sol")
    scope = "theorem_stating:reviewer:review:codex:gpt-5.6-sol:xhigh"
    codex_headless._store_persisted_thread_id(tmp_path, "reviewer", scope, config, "old-review-thread")
    log_dir = tmp_path / "logs"
    calls = []

    def write_markers():
        (log_dir / "reviewer.started").write_text("now\n")
        (log_dir / "reviewer.exit").write_text(f"{exit_code}\n")
        (log_dir / "reviewer-output.log").write_text("\n".join(json.dumps(x) for x in [
            {"type": "thread.started", "thread_id": "old-review-thread"},
            {"type": "error", "message": message},
            {"type": "turn.failed", "error": {"message": message}},
        ]))

    _install_tmux_spies(monkeypatch, calls=calls, on_send_keys=write_markers)
    result = codex_headless.run(config, "review", role="reviewer", session_scope=scope,
        session_name="review-recovery", work_dir=tmp_path, log_dir=log_dir,
        fresh=False, startup_timeout=5, burst_timeout=5)
    assert result.ok == (exit_code == 0)
    if exit_code:
        assert message in result.error
    session_file = codex_headless._session_state_path(tmp_path, "reviewer", scope, config)
    assert session_file.exists() != (reset_context and exit_code != 0)


def test_context_recovery_ignores_errors_quoted_by_tools():
    message = 'Error running remote compact task: {"detail":"Bad Request"}'
    output = json.dumps({"type": "item.completed", "item": {
        "type": "command_execution", "aggregated_output": message,
    }})
    assert not codex_headless._detected_compaction_bad_request(output)
    assert not codex_headless._codex_error_messages(output)


@pytest.mark.parametrize("event", [
    {"type": "error", "error": {"message": "context_length_exceeded"}},
    {"type": "turn.failed", "message": "context_length_exceeded"},
])
def test_nested_provider_errors_retain_context_overflow_recovery(event):
    assert codex_headless._detected_context_overflow(json.dumps(event))


@pytest.mark.parametrize('diagnostic,completed,failed', [
    ('2026-10-05T21:08:40.011225Z ERROR codex_core::tools::router: error=code-mode host exited with status signal: 5 (SIGTRAP) (core dumped)', False, True),
    ('2026-10-05T21:08:40.011225Z ERROR codex_core::tools::router: error=code-mode host closed its stdout', True, False),
    (json.dumps({'type':'agent_message','text':'code-mode host closed its stdout'}), False, False),
    ('ordinary output without a result artifact', False, False),
])
def test_tool_host_failure_keeps_transport_and_completion_distinct(monkeypatch, tmp_path, diagnostic, completed, failed):
    calls, ledger = [], []
    log_dir = tmp_path / 'logs'
    done = tmp_path / 'done'
    monkeypatch.delenv('TRELLIS_AGENT_COMPILER_CAPABILITY', raising=False)
    monkeypatch.setattr(tmux_backend, 'append_cost_ledger', lambda *args, **kwargs: ledger.append(kwargs))
    def finish():
        (log_dir / 'worker.started').write_text('now\n')
        (log_dir / 'worker.exit').write_text('0\n')
        (log_dir / 'worker-output.log').write_text(diagnostic+'\n'+json.dumps({'type':'turn.completed','usage':{'input_tokens':1,'output_tokens':1}})+'\n')
        if completed: done.write_text('done\n')
    _install_tmux_spies(monkeypatch, calls=calls, on_send_keys=finish)
    result = _run(monkeypatch, tmp_path, 'offline-host-failure', done_file=done)
    assert result.ok is not failed
    assert result.exit_code == 0
    assert bool(result.error) is failed
    assert ledger[-1]['ok'] is not failed
    if failed: assert 'Codex tool runtime failed before completion' in result.error
