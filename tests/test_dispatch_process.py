import json
from pathlib import Path
import sys
import time

import pytest

from trellis.dispatch.process import AttemptProcess, alive


def test_reaping_kills_a_descendant_whose_filesystem_wait_defers_stop(monkeypatch):
    import signal
    from trellis.dispatch import process
    sent = []
    monkeypatch.setattr(process.signal, 'pidfd_send_signal', lambda fd, sig: sent.append((fd, sig)))
    monkeypatch.setattr(process.select, 'select', lambda r, w, x, timeout: (r, [], []))
    assert process._stop_for_reaping(42, lambda: ['S'], timeout=0) is False
    assert sent == [(42, signal.SIGSTOP), (42, signal.SIGKILL)]


def test_reaping_never_acknowledges_an_unreaped_descendant(monkeypatch):
    from trellis.dispatch import process
    monkeypatch.setattr(process.signal, 'pidfd_send_signal', lambda *args: None)
    monkeypatch.setattr(process.select, 'select', lambda *args: ([], [], []))
    with pytest.raises(RuntimeError, match='retain its slot'):
        process._stop_for_reaping(42, lambda: ['D'], timeout=0)


def wait_result(process):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if process.result() is not None:
            return process.result()
        time.sleep(.02)
    pytest.fail('attempt did not finish')


def test_launch_gate_restart_adoption_and_at_most_once_execution(tmp_path):
    process = AttemptProcess(tmp_path / 'attempt')
    touched = tmp_path / 'executed'
    command = [sys.executable, '-c',
        "import json,sys,pathlib; p=json.load(sys.stdin)['path']; pathlib.Path(p).write_text('once'); print('done')"]
    record = process.start_or_adopt(command, {'path': str(touched)})
    try:
        time.sleep(.1)
        assert alive(record) and not touched.exists(), 'reservation is not permission to execute'
        recovered = AttemptProcess(process.root)
        assert recovered.start_or_adopt(command, {'path': str(touched)}) == record
        recovered.permit(record)
        assert wait_result(recovered)['returncode'] == 0
        process.child.wait(timeout=5)
        touched.unlink()
        assert recovered.start_or_adopt(command, {'path': str(touched)}) == record
        assert not touched.exists(), 'restart after completion must not execute twice'
        with pytest.raises(RuntimeError, match='different launch recipe'):
            recovered.start_or_adopt(command, {'path': 'something else'})
    finally:
        process.stop_and_reap(record)


def test_timeout_reaps_launcher_and_retains_evidence(tmp_path):
    process = AttemptProcess(tmp_path / 'attempt')
    record = process.start_or_adopt([sys.executable, '-c', 'import time; time.sleep(60)'], {})
    process.permit(record)
    time.sleep(.1)
    process.stop_and_reap(record)
    assert not alive(record)
    assert (process.root / 'recipe.json').exists()
    assert (process.root / 'permit.json').exists()
    assert process.result() is None


def test_provider_budget_waits_for_exact_explicit_resume(tmp_path):
    from trellis.dispatch.publication import atomic
    from trellis.dispatch.candidate import canonical_json
    process = AttemptProcess(tmp_path / 'attempt')
    command = [sys.executable, '-c', '''
import json, sys
from pathlib import Path
p=Path(json.load(sys.stdin)['count'])
n=int(p.read_text())+1 if p.exists() else 1
p.write_text(str(n))
print(json.dumps({'pause': {'kind': 'provider_budget', 'reason': 'fixture quota'}} if n==1 else {'ok': True}))
sys.exit(3 if n==1 else 0)
''']
    record = process.start_or_adopt(command, {'count': str(tmp_path / 'count')})
    try:
        process.permit(record)
        deadline = time.monotonic() + 5
        paused = process.root / 'provider-pause.json'
        while not paused.exists() and time.monotonic() < deadline:
            time.sleep(.02)
        assert paused.exists() and process.result() is None and alive(record)
        assert (tmp_path / 'count').read_text() == '1'
        value = json.loads(paused.read_text())
        atomic(process.root / 'provider-resume.json', canonical_json(dict(ordinal=value['ordinal'], process=record)))
        assert wait_result(process)['returncode'] == 0
        assert (tmp_path / 'count').read_text() == '2'
        assert (process.root / 'provider-pauses/1.json').exists()
    finally:
        process.stop_and_reap(record)


@pytest.mark.parametrize('output', ['{"pause":{"kind":"provider_budget","reason":"synthetic quota"}}',
                                   'malformed synthetic response'])
def test_native_engineering_first_failure_never_enters_transport_resume(tmp_path, monkeypatch, output):
    from trellis.dispatch import process as module
    monkeypatch.setenv('TRELLIS_TMUX_SOCKET', 'inherited-unrelated-socket')
    original_run = module.subprocess.run
    def no_tmux(argv, *args, **kwargs):
        assert not argv or argv[0] != 'tmux'
        return original_run(argv, *args, **kwargs)
    monkeypatch.setattr(module.subprocess, 'run', no_tmux)
    process = AttemptProcess(tmp_path / 'engineering')
    count = tmp_path / 'count'
    body = '''import json,os,sys
from pathlib import Path
p=json.load(sys.stdin)
assert 'TRELLIS_TMUX_SOCKET' not in os.environ
path=Path(p['count']); path.write_text(str(int(path.read_text())+1 if path.exists() else 1))
print(p['output'])
raise SystemExit(3)
'''
    payload = dict(request=dict(kind='Engineering'), count=str(count), output=output)
    command = [sys.executable, '-c', body]
    record = process.start_or_adopt(command, payload)
    try:
        process.permit(record)
        assert wait_result(process)['returncode'] == 3
        assert (process.root / 'response.json').read_text() == output + '\n'
        assert not (process.root / 'provider-pause.json').exists()
        assert not (process.root / 'provider-resume.json').exists()
        assert not (process.root / 'provider-pauses').exists()
        process.child.wait(timeout=5)
        assert process.start_or_adopt(command, payload) == record
        assert count.read_text() == '1'
    finally:
        process.stop_and_reap(record)
    assert json.loads((process.root / 'cleanup.json').read_text()) == record


def test_missing_domain_still_stops_owned_launcher(tmp_path, monkeypatch):
    from trellis.dispatch import process as module
    process = AttemptProcess(tmp_path / 'engineering')
    record = process.start_or_adopt([sys.executable, '-c', 'import time;time.sleep(60)'],
                                    dict(request=dict(kind='Engineering')))
    process.permit(record)
    time.sleep(.1)
    (process.root / 'recipe.json').rename(process.root / 'retained-recipe.json')
    original_run = module.subprocess.run
    def no_tmux(argv, *args, **kwargs):
        assert not argv or argv[0] != 'tmux'
        return original_run(argv, *args, **kwargs)
    monkeypatch.setattr(module.subprocess, 'run', no_tmux)
    with pytest.raises(RuntimeError, match='domain is unavailable'):
        process.stop_and_reap(record)
    assert not alive(record)
    assert json.loads((process.root / 'cleanup.json').read_text()) == record


@pytest.mark.parametrize('initial_thread,retain_history', [('', True), ('same-thread', True), ('same-thread', False)])
def test_submission_budget_resume_keeps_owned_clock_and_exact_context(tmp_path, initial_thread, retain_history):
    """Synthetic provider streams exercise the real durable launcher and monitor."""
    from trellis.agent_wrapper import provider_clock
    from trellis.dispatch.publication import atomic
    from trellis.dispatch.candidate import canonical_json
    process = AttemptProcess(tmp_path / 'execution')
    runtime = tmp_path / 'runtime'
    runtime.mkdir()
    script = r'''
import json,sys,subprocess
from pathlib import Path
from trellis.agent_wrapper import provider_clock as clock
from trellis.agent_wrapper.continuation import record_session
p=json.load(sys.stdin)
root=Path(p['worker_turn']['execution_record'])
history=Path(p['history'])
received=json.loads(history.read_text()) if history.exists() else []
received.append(p['worker_turn'].get('transport_resume'))
history.write_text(json.dumps(received))
n=len(received)
session=(p['worker_turn'].get('transport_resume') or {}).get('session')
binding=dict(provider='codex',model='test-model',reasoning_effort='high',home=p['home'],thread_id=(session or {}).get('thread_id',''))
c=clock.prepare(root,allowance_ms=1000,binding=binding,output=Path(p['output']))
current=Path(c.get('segment_directory',root))
thread=p['initial_thread'] if n==1 else 'same-thread'
events=([dict(type='thread.started',thread_id=thread)] if thread else [])
events.append(dict(type='turn.failed',error=dict(message='usage_limit_reached')) if n<3 else dict(type='turn.completed'))
code='import json,time\nprint('+repr(c['marker'])+')\ntime.sleep(.02)\n'+'\n'.join('print('+repr(json.dumps(e))+')' for e in events)
subprocess.run([sys.executable,'-m','trellis.agent_wrapper.provider_clock',str(current),'--',sys.executable,'-u','-c',code],check=False)
if thread and p['retain_history']:
    record_session(Path(p['runtime_root'])/'worker-session.json',dict(binding,thread_id=thread))
    sessions=Path(p['home'])/'.codex/sessions'
    sessions.mkdir(parents=True,exist_ok=True)
    (sessions/('rollout-2026-10-08T12-34-56-'+thread+'.jsonl')).write_text('{}\n')
print(json.dumps(dict(pause=dict(kind='provider_budget',reason='synthetic provider quota')) if n<3 else dict(ok=True)))
sys.exit(3 if n<3 else 0)
'''
    payload = dict(worker_turn=dict(turn=1, execution_record=str(process.root / 'clock')),
        runtime_root=str(runtime), request=dict(worker_submission_policy=dict(max_infrastructure_retries=2)),
        home=str(tmp_path / 'home'), history=str(tmp_path / 'received.json'),
        output=str(tmp_path / 'output.log'), initial_thread=initial_thread, retain_history=retain_history)
    record = process.start_or_adopt([sys.executable, '-c', script], payload)
    def wait_pause(ordinal):
        deadline = time.monotonic() + 10
        path = process.root / 'provider-pause.json'
        while time.monotonic() < deadline:
            if path.exists():
                value = json.loads(path.read_text())
                if value['ordinal'] == ordinal:
                    return value
            if process.result() is not None:
                pytest.fail((process.root / 'launcher.log').read_text())
            time.sleep(.02)
        pytest.fail((process.root / 'launcher.log').read_text())
    try:
        process.permit(record)
        first = wait_pause(1)
        first_clock = (process.root / 'clock/finished.json').read_bytes()
        assert first['resume_allowed'] is retain_history
        # A mismatching manual permit never invokes the bridge again.
        atomic(process.root / 'provider-resume.json', canonical_json(dict(ordinal=9, process=record)))
        time.sleep(.25)
        assert json.loads(Path(payload['history']).read_text()) == [None]
        atomic(process.root / 'provider-resume.json', canonical_json(dict(ordinal=1, process=record)))
        if not retain_history:
            time.sleep(.3)
            assert 'context' in first['pause']['reason']
            assert process.result() is None and alive(record)
            assert json.loads(Path(payload['history']).read_text()) == [None]
            assert not (process.root / 'clock/segments').exists()
            return
        second = wait_pause(2)
        assert second['resume_allowed'] is True
        time.sleep(.25)
        received = json.loads(Path(payload['history']).read_text())
        assert len(received) == 2, 'old permit cannot authorize the next pause'
        assert received[1]['ordinal'] == 1
        if initial_thread:
            assert received[1]['session']['thread_id'] == initial_thread
        else:
            assert received[1]['session'] is None
        atomic(process.root / 'provider-resume.json', canonical_json(dict(ordinal=2, process=record)))
        assert wait_result(process)['returncode'] == 0
        received = json.loads(Path(payload['history']).read_text())
        assert len(received) == 3 and received[2]['session']['thread_id'] == 'same-thread'
        result = provider_clock.read(process.root / 'clock', 'finished')
        assert result['disposition'] == 'completed' and result['transport_segments'] == 3
        assert result['elapsed_ms'] < 200, 'manual pause waits do not consume provider allowance'
        assert (process.root / 'clock/finished.json').read_bytes() == first_clock
    finally:
        process.stop_and_reap(record)


def test_timeout_reaps_detached_term_ignoring_host_descendants(tmp_path):
    from trellis.dispatch.process import identity
    process = AttemptProcess(tmp_path/'attempt')
    pid_path = tmp_path/'detached.pid'
    child_code = 'import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); time.sleep(60)'
    command = [sys.executable, '-c',
        "import json,sys,subprocess,pathlib,time; p=json.load(sys.stdin); "
        "c=subprocess.Popen([sys.executable,'-c',p['code']],start_new_session=True); "
        "pathlib.Path(p['pid']).write_text(str(c.pid)); time.sleep(60)"]
    record = process.start_or_adopt(command, dict(code=child_code, pid=str(pid_path)))
    process.permit(record)
    deadline = time.monotonic()+5
    while not pid_path.exists() and time.monotonic()<deadline:
        time.sleep(.01)
    assert pid_path.exists()
    detached = identity(int(pid_path.read_text()))
    process.stop_and_reap(record)
    assert not alive(detached)
    assert json.loads((process.root/'cleanup.json').read_text()) == record
