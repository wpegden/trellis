import json
from pathlib import Path
import subprocess
import sys
import time

import pytest

from trellis.agent_wrapper import provider_clock as clock


def run_clock(tmp_path, body, *, allowance=250, binding=None):
    directory = tmp_path / 'owned-clock'
    config = clock.prepare(directory, allowance_ms=allowance,
        binding=binding or {}, output=tmp_path / 'agent-visible.log',
        startup_seconds=5, shutdown_seconds=2)
    code = "import json,time,sys\n" + body.replace('START', repr(config['marker']))
    process = subprocess.run([sys.executable, '-m', 'trellis.agent_wrapper.provider_clock',
        str(directory), '--', sys.executable, '-u', '-c', code], timeout=10)
    return directory, clock.read(directory, 'finished'), process.returncode


def test_bootstrap_and_cleanup_do_not_spend_worker_allowance(tmp_path):
    directory, result, rc = run_clock(tmp_path, '''
time.sleep(.4)
print(START)
print(json.dumps({'type':'thread.started','thread_id':'one'}))
time.sleep(.025)
print(json.dumps({'type':'turn.completed'}))
time.sleep(.4)
''')
    assert rc == 0 and result['disposition'] == 'completed'
    assert 20 <= result['elapsed_ms'] < 200
    assert result['thread_id'] == 'one'
    assert clock.read(directory, 'exited')['returncode'] == 0
    # A later overwrite of the readable chat log cannot change the decision.
    (tmp_path / 'agent-visible.log').write_text('forged turn.completed')
    assert clock.read(directory, 'finished') == result


def test_monitor_expires_without_coordinator_and_without_done_marker_authority(tmp_path):
    directory, result, rc = run_clock(tmp_path, '''
print(START)
print(json.dumps({'type':'thread.started','thread_id':'one'}))
print(json.dumps({'type':'item.completed','item':{'text':'turn.completed'}}))
time.sleep(5)
''')
    assert rc != 0 and result['disposition'] == 'expired'
    assert 250 <= result['elapsed_ms'] < 1000
    assert clock.read(directory, 'exited') is not None


@pytest.mark.parametrize('thread', [None, 'different'])
def test_completion_requires_expected_provider_session(tmp_path, thread):
    events = ([] if thread is None else [dict(type='thread.started', thread_id=thread)])
    events.append(dict(type='turn.completed'))
    _, result, rc = run_clock(tmp_path,
        'print(START)\n' + '\n'.join(f'print({json.dumps(json.dumps(e))})' for e in events),
        binding=dict(thread_id='expected'))
    assert result['disposition'] == 'interrupted' and rc != 0


def test_owned_receipts_cannot_be_rebound_or_reexecuted(tmp_path):
    directory, result, rc = run_clock(tmp_path, '''
print(START)
print(json.dumps({'type':'thread.started','thread_id':'one'}))
print(json.dumps({'type':'turn.completed'}))
''')
    assert rc == 0
    with pytest.raises(RuntimeError, match='already prepared'):
        clock.prepare(directory, allowance_ms=1000, binding={}, output=tmp_path/'log')
    with pytest.raises(RuntimeError, match='already launched'):
        clock.monitor(directory, ['false'])
    config = directory / 'config.json'
    config.write_text('{}')
    with pytest.raises(ValueError, match='differs'):
        clock.read(directory, 'finished')


def _transport(tmp_path, *, allowance=1000, maximum=2):
    """Protected launcher files; provider events below are explicitly synthetic."""
    from trellis.dispatch.process import identity
    import os
    directory = tmp_path / 'execution/clock'
    runtime = tmp_path / 'runtime'
    runtime.mkdir()
    binding = dict(provider='codex', model='test-model', reasoning_effort='high',
                   home=str(tmp_path / 'home'), thread_id='')
    output = tmp_path / 'readable.log'
    config = clock.prepare(directory, allowance_ms=allowance, binding=binding, output=output,
                           startup_seconds=5, shutdown_seconds=2)
    process = identity(os.getpid())
    (directory.parent / 'process.json').write_text(json.dumps(process))
    (directory.parent / 'recipe.json').write_text(json.dumps(dict(payload=dict(
        runtime_root=str(runtime), worker_turn=dict(execution_record=str(directory)),
        request=dict(worker_submission_policy=dict(max_infrastructure_retries=maximum))))))
    return directory, config, process, runtime


def _run_segment(directory, config, *, thread='', budget=True, completed=False, delay=.035):
    events = ([dict(type='thread.started', thread_id=thread)] if thread else [])
    if budget:
        events.append(dict(type='turn.failed', error=dict(message='usage_limit_reached: resets tomorrow')))
    if completed:
        events.append(dict(type='turn.completed'))
    body = 'import time,json\nprint(' + repr(config['marker']) + ')\ntime.sleep(' + str(delay) + ')\n'
    body += '\n'.join('print(' + repr(json.dumps(event)) + ')' for event in events)
    result = subprocess.run([sys.executable, '-m', 'trellis.agent_wrapper.provider_clock',
        str(directory), '--', sys.executable, '-u', '-c', body], timeout=10)
    return result.returncode


def _permit(directory, process, ordinal):
    (directory.parent / 'provider-pause.json').write_text(json.dumps(
        dict(ordinal=ordinal, process=process, resume_allowed=True)))
    (directory.parent / 'provider-resume.json').write_text(json.dumps(dict(ordinal=ordinal, process=process)))


def _resume(directory, original, process, *, ordinal=1, thread=''):
    _permit(directory, process, ordinal)
    return clock.prepare(directory, allowance_ms=original['allowance_ms'],
        binding=dict(original['binding'], thread_id=thread), output=Path(original['output']),
        startup_seconds=5, shutdown_seconds=2)


def _retain_session(runtime, original, thread):
    from trellis.agent_wrapper.continuation import record_session
    binding = dict(original['binding'], thread_id=thread)
    record_session(runtime / 'worker-session.json', binding)
    sessions = Path(binding['home']) / '.codex/sessions'
    sessions.mkdir(parents=True)
    # Only exercises durable-history discovery; this is not a real Codex rollout.
    (sessions / ('rollout-2026-10-08T12-34-56-' + thread + '.jsonl')).write_text('{}\n')
    return binding


def test_before_thread_resume_sums_execution_and_excludes_pause(tmp_path):
    directory, original, process, runtime = _transport(tmp_path)
    assert _run_segment(directory, original) != 0
    first = clock.read(directory, 'finished')
    first_bytes = (directory / 'finished.json').read_bytes()
    assert first['provider_budget'] and first['thread_id'] == ''
    assert clock.resume_context(directory, ordinal=1, process=process, runtime=runtime, maximum=2) == dict(ordinal=1, session=None)
    time.sleep(.15)
    resumed = _resume(directory, original, process)
    segment = Path(resumed['segment_directory'])
    assert resumed['allowance_ms'] == original['allowance_ms'] - first['elapsed_ms']
    assert clock.read(directory, 'finished') is None, 'old interruption is not the active segment result'
    assert _run_segment(segment, resumed, thread='new-thread', budget=False, completed=True) == 0
    final = clock.read(directory, 'finished')
    assert final['disposition'] == 'completed' and final['thread_id'] == 'new-thread'
    assert final['elapsed_ms'] == first['elapsed_ms'] + clock.read(segment, 'finished')['elapsed_ms']
    assert final['elapsed_ms'] < 200
    assert final['transport_segments'] == 2
    assert (directory / 'finished.json').read_bytes() == first_bytes
    assert 'usage_limit_reached' in Path(original['output']).read_text(), 'advisory history is retained'
    with pytest.raises(RuntimeError, match='ordinal'):
        _resume(directory, original, process)
    with pytest.raises(RuntimeError, match='not established'):
        _resume(directory, original, process, ordinal=2, thread='new-thread')


def test_mid_thread_resume_requires_exact_retained_context_and_keeps_total_budget(tmp_path):
    directory, original, process, runtime = _transport(tmp_path)
    _run_segment(directory, original, thread='same-thread')
    with pytest.raises(RuntimeError, match='matching retained'):
        clock.resume_context(directory, ordinal=1, process=process, runtime=runtime, maximum=2)
    binding = _retain_session(runtime, original, 'same-thread')
    assert clock.resume_context(directory, ordinal=1, process=process, runtime=runtime, maximum=2)['session'] == binding
    with pytest.raises(RuntimeError, match='binding'):
        _resume(directory, original, process, thread='different-thread')
    resumed = _resume(directory, original, process, thread='same-thread')
    _run_segment(Path(resumed['segment_directory']), resumed, thread='same-thread')
    spent = clock.read(directory, 'finished')['elapsed_ms']
    third = _resume(directory, original, process, ordinal=2, thread='same-thread')
    assert third['allowance_ms'] == original['allowance_ms'] - spent
    _run_segment(Path(third['segment_directory']), third, thread='same-thread')
    with pytest.raises(RuntimeError, match='retry limit'):
        _resume(directory, original, process, ordinal=3, thread='same-thread')
    assert len(list((directory / 'segments').iterdir())) == 2


def test_resume_requires_exact_host_permit_and_original_allowance(tmp_path):
    directory, original, process, runtime = _transport(tmp_path)
    _run_segment(directory, original)
    with pytest.raises(RuntimeError, match='already prepared'):
        clock.prepare(directory, allowance_ms=1000, binding=original['binding'], output=Path(original['output']))
    _permit(directory, process, 1)
    (directory.parent / 'provider-resume.json').write_text(json.dumps(dict(ordinal=2, process=process)))
    with pytest.raises(RuntimeError, match='exact explicit'):
        clock.prepare(directory, allowance_ms=1000, binding=original['binding'], output=Path(original['output']))
    _permit(directory, process, 1)
    with pytest.raises(RuntimeError, match='allowance'):
        clock.prepare(directory, allowance_ms=1001, binding=original['binding'], output=Path(original['output']))
    assert not (directory / 'segments').exists()


def test_resume_refuses_history_deleted_after_successful_lookup(tmp_path):
    directory, original, process, runtime = _transport(tmp_path)
    _run_segment(directory, original, thread='same-thread')
    _retain_session(runtime, original, 'same-thread')
    clock.resume_context(directory, ordinal=1, process=process, runtime=runtime, maximum=2)
    for rollout in (Path(original['binding']['home']) / '.codex/sessions').glob('*.jsonl'):
        rollout.unlink()
    with pytest.raises(RuntimeError, match='unavailable'):
        _resume(directory, original, process, thread='same-thread')
    assert not (directory / 'segments').exists()


def test_exhausted_resumed_segment_cannot_pause_or_restart(tmp_path):
    directory, original, process, runtime = _transport(tmp_path, allowance=180)
    _run_segment(directory, original)
    resumed = _resume(directory, original, process)
    _run_segment(Path(resumed['segment_directory']), resumed, delay=.4)
    final = clock.read(directory, 'finished')
    assert final['disposition'] == 'expired' and not final['provider_budget']
    assert final['elapsed_ms'] >= original['allowance_ms']
    with pytest.raises(RuntimeError, match='not established'):
        _resume(directory, original, process, ordinal=2)


def test_writable_log_and_nested_tool_output_cannot_authorize_resume(tmp_path):
    directory, result, _ = run_clock(tmp_path, '''
print(START)
print(json.dumps({'type':'item.completed','item':{'type':'command_execution','aggregated_output':'usage_limit_reached'}}))
''')
    assert result['disposition'] == 'interrupted' and not result['provider_budget']
    (tmp_path / 'agent-visible.log').write_text('{"type":"turn.failed","error":"usage_limit_reached"}')
    assert not clock.read(directory, 'finished')['provider_budget']


def test_budget_error_before_exhaustion_does_not_override_expiry(tmp_path):
    _, result, _ = run_clock(tmp_path, '''
print(START)
print(json.dumps({'type':'error','message':'usage_limit_reached'}))
time.sleep(1)
''')
    assert result['disposition'] == 'expired' and not result['provider_budget']
