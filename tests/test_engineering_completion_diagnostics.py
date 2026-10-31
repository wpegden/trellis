"""Completion disagreement stays unknown, with only finite public evidence."""
from contextlib import contextmanager
import hashlib
import itertools
import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from trellis.adapters import BurstResult, ProviderConfig
from trellis.agents import codex_headless as headless, engineering_exec
from trellis import git_trust, pv_execution
from trellis.agent_wrapper import executor
from trellis.agent_wrapper.protocol import AgentLane, SingleAgentRequest


@pytest.fixture
def owned(tmp_path, monkeypatch):
    clock = tmp_path / 'clock'; clock.mkdir()
    raw = b'{"version":1}'
    (clock / 'config.json').write_bytes(raw)
    identity = hashlib.sha256(raw).hexdigest()
    records = dict(started=dict(config_sha256=identity),
        finished=dict(config_sha256=identity, disposition='completed', elapsed_ms=12,
                      thread_id='12345678-1234-1234-1234-123456789abc',
                      detail='PRIVATE_DETAIL', provider_budget='PRIVATE_BUDGET'),
        exited=dict(config_sha256=identity, returncode=0))
    for name, value in records.items(): (clock / (name + '.json')).write_text(json.dumps(value))
    monkeypatch.setattr(engineering_exec, 'verify', lambda *args: {})
    monkeypatch.setattr(headless, 'os', SimpleNamespace(killpg=lambda *args: None))
    counter = itertools.count()
    monkeypatch.setattr(headless, 'time', SimpleNamespace(monotonic=lambda: next(counter), sleep=lambda _: None))
    def call(*, outer=0, terminal=(0, 'launcher_exited'), cancel=False, clock_directory=None):
        @contextmanager
        def process(*args, **kwargs):
            if cancel: raise KeyboardInterrupt('synthetic original cancellation')
            yield SimpleNamespace(pid=123, poll=lambda: outer, wait=lambda: outer)
        monkeypatch.setattr(pv_execution, 'owned_process', process)
        monkeypatch.setattr(headless, 'engineering_launch_status', lambda _: terminal)
        completion = {}
        command = ['/usr/bin/python3', '-I', '-S', 'engineering_exec.py', '--binding',
                   str(tmp_path / 'binding.json'), '--binding-sha256', 'a' * 64, '--', '/bin/true']
        result = headless._run_engineering_owned(tmp_path / 'launcher', command, work_dir=tmp_path,
            environment={}, start_file=tmp_path / 'started', exit_file=tmp_path / 'exit',
            startup_timeout=1, burst_timeout=1, clock_record=clock_directory or clock, completion=completion)
        return result, completion
    return clock, call


def test_agreeing_completion_keeps_success_and_excludes_private_fields(owned):
    _, call = owned
    ok, completion = call()
    assert ok and completion['clock_finished'] == 'valid' and completion['launcher_receipt'] == 'valid'
    assert completion['clock_elapsed_ms'] == 12
    assert 'PRIVATE' not in json.dumps(completion)


@pytest.mark.parametrize('outer,terminal,key,value', [
    (-15, (0, 'launcher_exited'), 'outer_returncode', -15),
    (7, (0, 'launcher_exited'), 'outer_returncode', 7),
    (0, None, 'launcher_receipt', 'missing'),
    (0, (125, 'execution_unknown'), 'launcher_receipt', 'invalid'),
    (0, (125, 'guard_refused'), 'launcher_code', 'guard_refused'),
    (0, (125, 'spawn_refused'), 'launcher_code', 'spawn_refused'),
    (0, (1, 'launcher_exited'), 'launcher_returncode', 1),
])
def test_each_outer_recorder_disagreement_is_unknown(owned, outer, terminal, key, value):
    _, call = owned
    ok, completion = call(outer=outer, terminal=terminal)
    assert not ok and completion[key] == value


@pytest.mark.parametrize('change,expected', [
    ('missing', 'missing'), ('json', 'invalid'), ('identity', 'invalid'),
    ('disposition', 'invalid'), ('configuration', 'invalid'),
])
def test_missing_and_invalid_clock_receipts_remain_distinct(owned, change, expected):
    clock, call = owned
    path = clock / 'finished.json'; original = json.loads(path.read_bytes())
    if change == 'missing': path.unlink()
    elif change == 'json': path.write_text('{')
    elif change in ('identity', 'disposition'):
        field = {'identity': 'config_sha256', 'disposition': 'disposition'}[change]
        original[field] = 'PRIVATE_INVALID_VALUE'; path.write_text(json.dumps(original))
    else: (clock / 'config.json').unlink()
    if change in ('json', 'identity'):
        with pytest.raises(ValueError) as caught:
            call()
        assert caught.value.engineering_completion['clock_finished'] == 'invalid'
        assert 'PRIVATE' not in json.dumps(caught.value.engineering_completion)
        return
    ok, completion = call()
    assert not ok and completion['clock_finished'] == expected
    assert 'PRIVATE' not in json.dumps(completion)


@pytest.mark.parametrize('disposition,code', [('expired', 0), ('interrupted', 0), ('completed', 2)])
def test_clock_disagreement_cannot_become_success(owned, disposition, code):
    clock, call = owned
    for name, field, value in [('finished', 'disposition', disposition), ('exited', 'returncode', code)]:
        path = clock / (name + '.json'); data = json.loads(path.read_bytes()); data[field] = value
        path.write_text(json.dumps(data))
    assert call()[0] is False


def test_optional_diagnostic_fields_do_not_change_existing_completion_decision(owned):
    clock, call = owned
    path = clock / 'finished.json'; record = json.loads(path.read_bytes())
    record.update(elapsed_ms='PRIVATE_OPTIONAL', owner='PRIVATE_OWNER')
    path.write_text(json.dumps(record))
    ok, completion = call()
    assert ok and 'clock_elapsed_ms' not in completion and 'PRIVATE' not in json.dumps(completion)


def test_core_completion_comparison_is_unchanged_by_diagnostic_types(owned):
    clock, call = owned
    path = clock / 'exited.json'; record = json.loads(path.read_bytes()); record['returncode'] = False
    path.write_text(json.dumps(record))
    ok, completion = call()
    assert ok and completion['clock_exited'] == 'invalid' and 'clock_returncode' not in completion


def request(tmp_path):
    view = tmp_path / 'delivery/view'; view.mkdir(parents=True)
    return SingleAgentRequest(request_id='engineering-fixture', cycle=1, kind='engineering', burst_role='engineering',
        provider=ProviderConfig(provider='codex'), prompt='Synthetic', work_dir=view, state_dir=view / '.trellis',
        session_name='fixture', lane=AgentLane(kind='engineering'), timeout_seconds=1)


def test_first_failure_retains_sanitized_completion_without_reclassification(tmp_path):
    single = request(tmp_path)
    result = BurstResult(False, None, 'PRIVATE_OUTPUT', .1, error='PRIVATE_ERROR', provider_execution=dict(completion=dict(
        outer_returncode=-15, launcher_receipt='missing', clock_finished='invalid', detail='PRIVATE_DETAIL',
        clock_elapsed_ms=True, environment='PRIVATE_ENV')))
    failure = executor.engineering_execution_failure(single, result)
    raw = failure.evidence.read_bytes(); record = json.loads(raw)
    assert record['completion'] == dict(outer_returncode=-15, launcher_receipt='missing', clock_finished='invalid')
    assert record['code'] == 'execution_unknown' and 'PRIVATE' not in raw.decode()
    git_trust.validate_engineering_receipt(record, record['identity'], {'execution_unknown'})
    result.provider_execution['completion']['outer_returncode'] = 0
    executor.engineering_execution_failure(single, result)
    assert failure.evidence.read_bytes() == raw
    forged = dict(record, completion=dict(record['completion'], error='PRIVATE'))
    with pytest.raises(ValueError): git_trust.validate_engineering_receipt(forged, record['identity'], {'execution_unknown'})
    with pytest.raises(ValueError):
        git_trust.validate_engineering_receipt(dict(code='launcher_exited', identity='a' * 64, uid=1, euid=1, exit_code=-15), 'a' * 64, {'launcher_exited'})


def test_failed_failure_publication_still_returns_unknown(tmp_path, monkeypatch):
    single = request(tmp_path)
    monkeypatch.setattr(git_trust, '_write_result', lambda *args, **kwargs: (_ for _ in ()).throw(OSError('synthetic refusal')))
    failure = executor.engineering_execution_failure(single)
    assert str(failure) == 'engineering_execution_unknown' and not failure.evidence.exists()


def test_original_cancellation_is_not_replaced_by_diagnostics(owned):
    _, call = owned
    with pytest.raises(KeyboardInterrupt, match='original cancellation'):
        call(cancel=True)


def test_existing_summed_clock_segment_completion_is_unchanged(owned, tmp_path):
    from test_provider_clock import _transport, _run_segment, _resume
    from trellis.agent_wrapper import provider_clock
    _, call = owned
    directory, original, process, _ = _transport(tmp_path)
    assert _run_segment(directory, original) != 0
    resumed = _resume(directory, original, process)
    assert _run_segment(Path(resumed['segment_directory']), resumed, thread='synthetic-thread', budget=False, completed=True) == 0
    ok, completion = call(clock_directory=directory)
    assert ok and completion['clock_elapsed_ms'] == provider_clock.read(directory, 'finished')['elapsed_ms']


@pytest.mark.parametrize('publication_fails', [False, True])
def test_runner_exception_publishes_safe_frame_and_preserves_original_error(tmp_path, monkeypatch, publication_fails):
    from dataclasses import replace
    from trellis.agent_wrapper.protocol import ArtifactSpec
    from trellis.config import SandboxConfig
    from trellis.agents import engineering_context
    single = replace(request(tmp_path), fresh=True, burst_home=tmp_path / 'home',
        artifact=ArtifactSpec('result.json', 'preparation-worker-result', phase='engineering'),
        sandbox=SandboxConfig(enabled=True, backend='bwrap'))
    monkeypatch.setattr(engineering_context, 'validate', lambda *args, **kwargs: None)
    monkeypatch.setattr(executor, 'tmux_kill_session', lambda *args: pytest.fail('engineering used tmux cleanup'))
    error = ValueError('synthetic original clock read failure')
    error.engineering_completion = dict(outer_returncode=0, clock_finished='invalid', error='PRIVATE_ERROR')
    def runner(*args, **kwargs): raise error
    if publication_fails:
        monkeypatch.setattr(git_trust, '_write_result', lambda *args, **kwargs: (_ for _ in ()).throw(OSError('synthetic refusal')))
    with pytest.raises(ValueError) as caught:
        executor.execute_agent_request(single, reviewer_runner=runner,
            port_resolver=SimpleNamespace(resolve=lambda *args: None))
    assert caught.value is error
    path = single.work_dir.parent / 'execution-failure.json'
    if publication_fails: assert not path.exists()
    else:
        record = json.loads(path.read_bytes())
        assert record['completion'] == dict(outer_returncode=0, clock_finished='invalid')
        assert record['code'] == 'execution_unknown' and 'PRIVATE' not in path.read_text()
