import asyncio
import copy
from types import SimpleNamespace

import pytest

from trellis.dispatch import candidate
from trellis.dispatch.coordination import Coordinator
from trellis.dispatch.driver import ProductionDriver
from trellis.dispatch.publication import atomic


def put(path, value):
    atomic(path, candidate.canonical_json(value))


@pytest.mark.parametrize('disposition,raw,thread', [
    ('completed', {'outcome': 'valid'}, 'retained'),
    ('expired', {'outcome': 'valid'}, 'retained'),
    ('expired', None, ''),
])
def test_submission_waits_for_cleanup_but_not_scheduler_deadline(tmp_path, disposition, raw, thread):
    async def scenario():
        driver = ProductionDriver.__new__(ProductionDriver)
        driver.root = tmp_path
        directory = tmp_path/'1'
        put(directory/'prepared.json', {})
        repo = directory/'repo'
        (repo/'Tablet').mkdir(parents=True)
        (repo/'Tablet/A.lean').write_text('theorem A : True := by trivial')
        put(directory/'base-manifest.json', {'Tablet/A.lean': candidate.digest(b'sorry')})
        process = dict(pid=123, start_ticks=42, boot_id='test')
        session = dict(provider='codex', model='test', reasoning_effort='high',
                       thread_id='retained', home=str(directory/'runtime/home'))
        put(directory/'runtime/worker-session.json', session)
        task = dict(id=1, status=dict(status='reserved'), process=None,
            assignment=dict(compiler='managed', input_hashes={}),
            worker_attempt=dict(turn=1, allowance_ms=1000, consumed_ms=0,
                contract_sha256='a'*64, session=None, phase=dict(phase='ready', feedback_sha256=None)))
        events = []
        timing = {}
        def launch(_, *, worker_turn, execution, **kwargs):
            assert kwargs['seconds'] is None, 'managed grants follow the owned turn, not a second wall clock'
            clock = execution/'clock'
            put(clock/'config.json', dict(allowance_ms=1000, binding={}))
            config_sha = candidate.digest((clock/'config.json').read_bytes())
            put(clock/'started.json', dict(config_sha256=config_sha, started_at_ms=9_000_000))
            timing.update(config_sha256=config_sha, thread_id=thread, disposition=disposition,
                          elapsed_ms=35 if disposition == 'completed' else 1002)
            put(clock/'finished.json', timing)
            put(clock/'exited.json', dict(config_sha256=config_sha, returncode=0))
            def reap(_): events.append('cleanup and compiler drain')
            return SimpleNamespace(root=execution, stop_and_reap=reap), process
        driver._process = launch
        driver._permit = lambda *args: events.append('permit')
        driver.now_ms = lambda: 99_000_000  # Native lock/polling arrived much later.
        async def observe(event):
            events.append(event['kind'])
            if event['kind'] == 'launched':
                task.update(process=process, status=dict(status='running'))
            elif event['kind'] == 'provider_started':
                assert event['started_at_ms'] == 9_000_000
                task['deadline_ms'] = 9_001_000
            elif event['kind'] == 'turn_completed':
                assert events[-2] == 'cleanup and compiler drain'
                completion = event['completion']
                assert completion['elapsed_ms'] == (35 if disposition == 'completed' else 1000)
                assert completion['exhausted'] == (disposition == 'expired')
                task['status'] = dict(status='submitted')
                task['deadline_ms'] = None
            elif event['kind'] == 'candidate_ready':
                assert task['status']['status'] == 'submitted'
                task['status'] = dict(status='checking', candidate_sha256=event['candidate_sha256'])
            elif event['kind'] == 'worker_expired':
                assert raw is None and not thread
                task['status'] = dict(status='stopping')
            else:
                pytest.fail(f'unexpected lifecycle event {event}')
        driver.observe = observe
        from trellis.dispatch.native import NativeLedger
        def kernel(payload):
            event = payload['event']
            if event is not None:
                # The delivery thread uses the same fixture transition as the
                # preceding provider lifecycle, with a fresh locked status.
                asyncio.run(observe(event))
            return dict(status='dispatch_ok', output={'tasks': {'1': copy.deepcopy(task)}})
        driver.native = NativeLedger(tmp_path, kernel=kernel)
        async def wait(*args, poll):
            await poll()
            return dict(kind='worker_submission', turn=1, request_id=7, cycle=2, raw=raw,
                provider_execution=timing, provider_turn_completed=disposition == 'completed', delivery_error='')
        driver._wait = wait
        async def snapshot(): return dict(tasks={'1': copy.deepcopy(task)})
        driver.snapshot = snapshot
        coordinator = Coordinator(driver)
        await coordinator._execute(copy.deepcopy(task))
        if raw is None:
            assert task['status']['status'] == 'stopping'
            assert events[-1] == 'worker_expired'
            assert not (directory/'turns/1/candidate').exists()
            return
        assert task['status']['status'] == 'checking'
        sealed, identity = candidate.read(directory/'turns/1/candidate')
        assert sealed['submission']['turn'] == 1
        assert sealed['response'] == {'outcome':'valid'}
        assert identity == task['status']['candidate_sha256']
        assert events == ['launched','permit','provider_started','cleanup and compiler drain',
                          'turn_completed','candidate_ready']
    asyncio.run(scenario())


def test_unsupported_provider_continuation_stops_without_offering_doomed_resume(tmp_path):
    driver = ProductionDriver.__new__(ProductionDriver)
    pauses = []
    driver._pause = lambda kind, reason, **kwargs: pauses.append((kind, reason))
    put(tmp_path / 'provider-pause.json', dict(ordinal=1, process={}, resume_allowed=False,
        pause=dict(reason='original conversation unavailable')))
    process = SimpleNamespace(root=tmp_path)
    with pytest.raises(RuntimeError, match='continuation unavailable'):
        asyncio.run(driver._wait(process, {}))
    assert pauses == [('parallel_infrastructure', 'original conversation unavailable')]


@pytest.mark.parametrize('reaped', [True, False])
def test_resume_does_not_reissue_historical_provider_permits(tmp_path, monkeypatch, reaped):
    from trellis.dispatch.__main__ import provider_resume_permits
    from trellis.dispatch import process
    monkeypatch.setattr(process, 'alive', lambda _: not reaped)
    owner = dict(pid=123, start_ticks=42, boot_id='test')
    old = tmp_path / '1/turns/1/execution'
    put(old / 'process.json', owner)
    put(old / 'cleanup.json', owner)
    put(old / 'provider-pause.json', dict(ordinal=1, process=owner, resume_allowed=False,
        pause=dict(reason='conversation unavailable')))
    current = tmp_path / '2/turns/1/execution'
    put(current / 'process.json', owner)
    pause = dict(ordinal=1, process=owner, resume_allowed=True)
    put(current / 'provider-pause.json', pause)
    if reaped:
        assert provider_resume_permits(tmp_path) == [(current, pause)]
    else:
        with pytest.raises(RuntimeError, match='ordinary resume cannot restart'):
            provider_resume_permits(tmp_path)
    assert not list(tmp_path.rglob('provider-resume.json'))
