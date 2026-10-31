import asyncio
import copy

import pytest

from trellis.dispatch.driver import ProductionDriver


@pytest.mark.parametrize('outcome', ['ok', 'missing', 'changed', 'host_failed', 'provider_failed'])
def test_wait_rechecks_completion_after_observing_launcher_exit(tmp_path, monkeypatch, outcome):
    from types import SimpleNamespace
    from trellis.dispatch import driver as module, candidate
    directory = tmp_path / 'execution'
    directory.mkdir()
    data = candidate.canonical_json({'ok': True})
    (directory / 'response.json').write_bytes(data)
    receipt = dict(returncode=1 if outcome.endswith('failed') else 0,
                   response_sha256='bad' if outcome == 'changed' else candidate.digest(data))
    available = False
    reads, reaped, paused = [], [], []
    def result():
        reads.append(True)
        return receipt if available and outcome != 'missing' else None
    def alive(_record):
        nonlocal available
        available = True  # Launcher finishes between the first read and liveness observation.
        return False
    monkeypatch.setattr(module, 'alive', alive)
    process = SimpleNamespace(root=directory, result=result, stop_and_reap=lambda r: reaped.append(r))
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = tmp_path
    driver._pause = lambda *args, **kwargs: paused.append(args)
    call = driver._wait(process, {'pid': 123}, host_check=outcome == 'host_failed')
    if outcome == 'ok':
        assert asyncio.run(call) == {'ok': True}
    else:
        error = {'missing': 'launcher exited without a result', 'changed': 'completion receipt',
                 'host_failed': 'host checker exited 1', 'provider_failed': 'attempt process failed'}[outcome]
        with pytest.raises(module.HostCheckFailure if outcome == 'host_failed' else RuntimeError,
                           match=error):
            asyncio.run(call)
    assert len(reads) == 2
    assert bool(reaped) == outcome.endswith('failed')
    assert bool(paused) == (outcome == 'provider_failed')


@pytest.mark.parametrize('fails', [False, True])
def test_blocking_operation_drains_repeated_cancellation_and_preserves_failure(fails):
    import threading
    from trellis.dispatch.driver import _run_blocking
    async def scenario():
        loop = asyncio.get_running_loop()
        started, joined = asyncio.Event(), asyncio.Event()
        release = threading.Event()
        finished = []
        def operation():
            loop.call_soon_threadsafe(started.set)
            assert release.wait(5)
            finished.append(True)
            if fails:
                raise RuntimeError('real publication failure')
            return 'completed'
        async def owner():
            try:
                return await _run_blocking(operation)
            finally:
                joined.set()
        task = asyncio.create_task(owner())
        try:
            await started.wait()
            for _ in range(3):
                task.cancel()
                await asyncio.sleep(0)
                assert not joined.is_set() and not task.done()
            release.set()
            with pytest.raises(RuntimeError if fails else asyncio.CancelledError,
                               match='real publication failure' if fails else None):
                await asyncio.wait_for(task, 2)
            assert finished == [True] and joined.is_set()
        finally:
            release.set()
            await asyncio.gather(task, return_exceptions=True)
    asyncio.run(scenario())


def test_timeout_during_worker_launch_waits_for_thread_before_cleanup(tmp_path):
    import threading
    from types import SimpleNamespace
    from trellis.dispatch.coordination import Coordinator
    async def scenario():
        directory = tmp_path / '1'
        directory.mkdir()
        (directory / 'prepared.json').write_text('{}')
        record = {'pid': 123, 'start_ticks': 4, 'boot_id': 'test'}
        task = dict(id=1, status={'status': 'running'}, deadline_ms=2000,
                    process=record, binding={'timeout_seconds': 1}, assignment={'compiler': 'managed'})
        now = 1000
        started, reaped = asyncio.Event(), asyncio.Event()
        release = threading.Event()
        loop = asyncio.get_running_loop()
        events = []
        driver = ProductionDriver.__new__(ProductionDriver)
        driver.root = tmp_path
        driver.now_ms = lambda: now
        def launch(*args, **kwargs):
            loop.call_soon_threadsafe(started.set)
            assert release.wait(5)
            events.append('launcher recorded')
            return SimpleNamespace(root=directory / 'execution'), record
        driver._process = launch
        driver._permit = lambda *_: pytest.fail('cancelled launch must never receive permission')
        async def snapshot():
            return dict(tasks={'1': copy.deepcopy(task)}, reviewer=None, verifiers_runnable=False)
        driver.snapshot = snapshot
        async def observe(event):
            if event['kind'] == 'attempt_stopping':
                task['status'] = dict(status='stopping', outcome=event['outcome'])
            else:
                assert event['kind'] == 'processes_reaped'
                assert events == ['launcher recorded', 'process cleanup acknowledged']
                task['status']['status'] = 'resolved'
                reaped.set()
        driver.observe = observe
        async def cleanup(_):
            assert events == ['launcher recorded']
            events.append('process cleanup acknowledged')
        driver.stop_and_reap = cleanup
        c = Coordinator(driver)
        try:
            await c.tick()
            await started.wait()
            now = 2000
            await c.tick()
            await c.tick()
            for _ in range(4): await asyncio.sleep(0)
            assert not events and task['status']['status'] == 'stopping'
            assert not c.reaping[1].done()
            release.set()
            await asyncio.wait_for(reaped.wait(), 2)
            assert task['status'] == dict(status='resolved', outcome={'kind': 'timed_out'})
            assert c.fatal is None
        finally:
            release.set()
            await c.shutdown()
    asyncio.run(scenario())


@pytest.mark.parametrize('superseded', [False, True])
def test_snapshot_reloads_only_after_verifier_supersession(tmp_path, superseded):
    from contextlib import nullcontext
    from types import SimpleNamespace
    calls = []
    first = dict(stage='Reviewer', tasks={}, verifiers={'jobs': {}}, revision=1)
    refreshed = dict(first, revision=2)
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.runtime = driver.repo = tmp_path
    driver.pausing = lambda: False
    driver.state = lambda: pytest.fail('idle status must not reload full protocol state')
    def query(payload):
        calls.append(payload)
        return first if len(calls) == 1 else refreshed
    driver.native = SimpleNamespace(prepare_retries=lambda _: None, locked=nullcontext,
        _require_recovered_publication=lambda: None, _call=query)
    driver.native.prepare_retries = lambda *_: pytest.fail('snapshot must expose durable work before retry preparation')
    driver._prepare_verifiers = lambda status: superseded
    driver.measurements = SimpleNamespace(sample=lambda _: None)
    result = driver._snapshot()
    assert len(calls) == (2 if superseded else 1)
    assert result['revision'] == (2 if superseded else 1)


def test_reserved_exclusive_recovery_uses_canonical_assignment(tmp_path):
    from contextlib import nullcontext
    from types import SimpleNamespace
    full = dict(scope={'exclusive': True}, request={'id': 7, 'contract': 'full ordinary request'})
    projected = dict(scope={'exclusive': True})
    status = dict(stage='Reviewer', tasks={'1': dict(id=1, status={'status': 'reserved'}, assignment=projected)},
                  verifiers={'jobs': {}})
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.runtime = driver.repo = tmp_path
    driver.pausing = lambda: False
    driver.state = lambda: dict(dispatch=dict(exclusive_workers={'1': {'state': {'cycle': 4}}},
        tasks={'1': {'assignment': full}}))
    driver.native = SimpleNamespace(prepare_retries=lambda _: None, locked=nullcontext,
        _require_recovered_publication=lambda: None, _call=lambda _: status)
    driver._prepare_verifiers = lambda _: False
    driver.measurements = SimpleNamespace(sample=lambda _: None)
    prepared = []
    driver._materialize = prepared.append
    assert driver._snapshot() is status
    assert prepared == [{'1': dict(private_state={'cycle': 4}, assignment=full)}]


@pytest.mark.parametrize('review', [None, {'request_id': 91}])
def test_worker_and_retry_handoffs_use_frozen_assignment_review(tmp_path, review):
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = tmp_path
    driver.state = lambda: dict(dispatch=dict(tasks={'8': {'id': 8, 'retry_of': 7, 'status': {'status': 'reserved'}}}))
    prepared = []
    def prepare(directory, private, **kwargs):
        directory.mkdir(); prepared.append(kwargs)
    driver._prepare = prepare
    driver._materialize({'8': dict(private_state={'in_flight_request': {'id': 95}},
        assignment=dict(review=review, scope={'nodes': ['A']}))})
    assert prepared == [dict(previous=tmp_path / '7',
        handoff_repo=tmp_path / 'review-91/repo' if review is not None else None)]


def test_materialize_batch_reads_one_state_and_keeps_distinct_handoffs(tmp_path):
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = tmp_path
    reads = []
    def state():
        reads.append(True)
        return dict(dispatch=dict(tasks={
            '8': dict(id=8, retry_of=7, review_id=91, status={'status': 'reserved'}),
            '9': dict(id=9, retry_of=None, review_id=92, status={'status': 'reserved'}),
        }))
    driver.state = state
    prepared = []
    def prepare(directory, private, **kwargs):
        directory.mkdir(); prepared.append(kwargs)
    driver._prepare = prepare
    worker = dict(private_state={}, assignment=dict(review=None, scope={'nodes': ['A']}))
    driver._materialize({'8': worker, '9': worker, '10': None})
    assert reads == [True]
    assert prepared == [
        dict(previous=tmp_path / '7', handoff_repo=tmp_path / 'review-91/repo'),
        dict(previous=None, handoff_repo=tmp_path / 'review-92/repo'),
    ]
    # Reuse is local to a batch; a subsequent operation reads fresh state.
    driver._materialize({'10': dict(private_state={}, assignment=dict(review=None, scope={'nodes': ['A']}))})
    assert len(reads) == 2


@pytest.mark.parametrize('reserved', [False, True])
def test_exclusive_worker_handoff_keeps_native_action_origin_across_retry(tmp_path, reserved):
    driver = ProductionDriver.__new__(ProductionDriver); driver.root = tmp_path
    tasks = {'7': dict(id=7, review_id=91, pending_retry={}, assignment={'scope': {'exclusive': True}})}
    if reserved:
        tasks['8'] = dict(id=8, retry_of=7, review_id=91, status={'status': 'reserved'})
    driver.state = lambda: dict(dispatch=dict(tasks=tasks))
    prepared = []
    def prepare(directory, private, **kwargs):
        directory.mkdir(); prepared.append(kwargs)
    driver._prepare = prepare
    driver._materialize({'8': dict(private_state={'in_flight_request': {'id': 95}},
        assignment=dict(review=None, scope={'exclusive': True}))})
    assert prepared == [dict(previous=tmp_path / '7', handoff_repo=tmp_path / 'review-91/repo')]


def test_resumed_task_seals_using_input_hashes_from_compact_status(tmp_path, monkeypatch):
    import json
    from types import SimpleNamespace
    directory = tmp_path / '1'; directory.mkdir()
    (directory / 'prepared.json').write_text('{}')
    (directory / 'base-manifest.json').write_text('{"Tablet/A.lean":"base"}')
    bridge = directory / 'runtime/bridge/latest_worker.json'; bridge.parent.mkdir(parents=True)
    response = {'kind': 'worker', 'status': 'Ok'}
    bridge.write_text(json.dumps(dict(response=response, raw={'outcome': 'valid'})))
    record = {'pid': 123}
    task = dict(id=1, status={'status': 'running'}, process=record, binding={'timeout_seconds': 14400},
                assignment={'compiler': 'managed', 'input_hashes': {'Tablet/A.lean': 'captured'}})
    driver = ProductionDriver.__new__(ProductionDriver); driver.root = tmp_path
    driver._process = lambda *a, **kw: (SimpleNamespace(root=directory / 'execution', stop_and_reap=lambda _: None), record)
    (directory / 'execution').mkdir(); (directory / 'execution/permit.json').write_text('{}')
    async def wait(*a): return response
    driver._wait = wait
    driver._record_transport = lambda *a: None
    def seal(*a, **kw):
        assert kw['inputs'] == {'Tablet/A.lean': 'captured'}
        assert kw['base_manifest'] == {'Tablet/A.lean': 'base'}
        assert kw['bridge_response'] == response
        return 'digest'
    monkeypatch.setattr('trellis.dispatch.candidate.seal', seal)
    completion = asyncio.run(driver.launch_or_adopt(task))
    assert completion.task_id == 1 and completion.candidate_sha256 == 'digest'


def test_verifier_preparation_reports_native_supersession(tmp_path):
    from types import SimpleNamespace
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.repo = tmp_path
    driver.state = lambda: {'canonical': 'current'}
    calls = []
    driver.native = SimpleNamespace(_call=lambda payload: calls.append(payload))
    routed = []
    driver._routing = routed.append
    status = dict(verifiers={'jobs': {'4': dict(status={'status': 'queued'},
        identity={'input_hashes': {'source': 'old'}})}})
    assert driver._prepare_verifiers(status) is True
    assert calls == [dict(action='dispatch_prepare_verifier', state={'canonical': 'current'},
                          id=4, repo_path=str(tmp_path))]
    assert routed == [dict(kind='supersede_queued', id=4,
                           reason='verifier inputs or panel policy changed before launch')]


def test_prepared_queued_verifier_is_not_rechecked_while_waiting_for_capacity(tmp_path):
    from types import SimpleNamespace
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = tmp_path
    prepared = tmp_path / 'verifier-4/prepared.json'
    prepared.parent.mkdir(); prepared.write_text('{}')
    driver.native = SimpleNamespace(_call=lambda _: pytest.fail('waiting panel was rechecked'))
    driver.state = lambda: pytest.fail('waiting panel reloaded protocol state')
    assert not driver._prepare_verifiers(dict(verifiers={'jobs': {'4': dict(status={'status': 'queued'})}}))


@pytest.mark.parametrize('superseded', [None, 4, 5])
def test_verifier_preparation_reuses_locked_state_until_native_mutation(tmp_path, superseded):
    from types import SimpleNamespace
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.repo = tmp_path
    generation, reads, checked, prepared = [0], [], [], []
    def state():
        reads.append(generation[0])
        return {'generation': generation[0]}
    def native(payload):
        assert payload['state']['generation'] == generation[0]
        checked.append(payload['id'])
        return None if payload['id'] == superseded else {'prepared_id': payload['id']}
    def supersede(id):
        assert id == superseded
        generation[0] += 1
    driver.state = state
    driver.native = SimpleNamespace(_call=native)
    driver._supersede_verifier = supersede
    driver._prepare_fresh = lambda path, value: prepared.append(value['prepared_id'])
    jobs = {str(id): dict(status={'status': 'queued'}) for id in (4, 5, 6)}
    assert driver._prepare_verifiers(dict(verifiers={'jobs': jobs})) is (superseded is not None)
    assert checked == [4, 5, 6]  # Every job retains its native current-input check.
    assert reads == ([0] if superseded is None else [0, 1])
    assert prepared == [id for id in (4, 5, 6) if id != superseded]


@pytest.mark.parametrize('prepared', [False, True])
@pytest.mark.parametrize('current', [False, True])
def test_verifier_launch_rechecks_current_native_panel_before_provider_work(tmp_path, prepared, current):
    from contextlib import contextmanager
    from types import SimpleNamespace
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.repo = driver.runtime = tmp_path
    directory = tmp_path / 'verifier-4'
    if prepared:
        directory.mkdir(); (directory / 'prepared.json').write_text('{}')
    events = []
    held = False
    @contextmanager
    def locked():
        nonlocal held
        assert not held
        held = True
        try: yield
        finally: held = False
    job = dict(id=4, status={'status': 'queued'}, identity={'compiler': 'none', 'input_hashes': {'same': 'bytes'}})
    state = dict(dispatch={'verifiers': {'jobs': {'4': job}}})
    def state_read():
        assert held
        return state
    driver.state = state_read
    private = {'fresh': 'native panel state'}
    def query(payload):
        assert held
        assert payload == dict(action='dispatch_prepare_verifier', state=state, id=4, repo_path=str(tmp_path))
        events.append('check')
        return private if current else None
    driver.native = SimpleNamespace(locked=locked, _call=query, _require_recovered_publication=lambda: None)
    def prepare(destination, actual):
        assert held and destination == directory and actual is private
        events.append('prepare')
    driver._prepare = prepare
    process = SimpleNamespace(root=directory / 'execution')
    record = {'pid': 123}
    def launch(destination, **kwargs):
        assert held and current and destination == directory
        events.append('launcher')
        return process, record
    driver._process = launch
    def route(event):
        assert held
        events.append(event['kind'])
        if current:
            assert event['process'] == record
        else:
            assert event['reason'] == 'verifier inputs or panel policy changed before launch'
    driver._routing = route
    driver.now_ms = lambda: 100
    def permit(actual, identity):
        assert held and current and actual is process and identity == record
        events.append('provider permit')
    driver._permit = permit
    result = driver._start_verifier(4)
    if current:
        assert result == (job, process, record)
        assert events == ['check'] + ([] if prepared else ['prepare']) + ['launcher', 'verifier_launched', 'provider permit']
    else:
        assert result is None
        assert events == ['check', 'supersede_queued']


def test_stale_queued_verifier_reaps_unpermitted_launcher_on_restart(tmp_path):
    from types import SimpleNamespace
    execution = tmp_path / 'verifier-4/execution'
    execution.mkdir(parents=True)
    (execution / 'process.json').write_text('{"pid":123}')
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = tmp_path
    events = []
    def process_factory(path):
        assert path == execution
        return SimpleNamespace(stop_and_reap=lambda record: events.append(('reap', record)))
    driver.process_factory = process_factory
    driver._routing = lambda event: events.append(('route', event))
    driver._supersede_verifier(4)
    assert events[0] == ('reap', {'pid': 123})
    assert events[1][1]['kind'] == 'supersede_queued'


@pytest.mark.parametrize('status', ['running', 'checking', 'resolved'])
def test_verifier_adoption_does_not_preflight_started_jobs(tmp_path, status):
    from contextlib import nullcontext
    from types import SimpleNamespace
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = tmp_path
    record = {'pid': 123}
    job = dict(id=4, status={'status': status, 'process': record}, identity={'compiler': 'none'})
    driver.state = lambda: dict(dispatch={'verifiers': {'jobs': {'4': job}}})
    driver.native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=lambda: None,
        _call=lambda _: pytest.fail('started job cannot be retired by queued preflight'))
    process = SimpleNamespace(root=tmp_path / 'verifier-4/execution')
    process.root.mkdir(parents=True); (process.root / 'permit.json').write_text('{}')
    driver._process = lambda *a, **kw: (process, record)
    driver._routing = lambda _: pytest.fail('adoption cannot repeat native launch')
    result = driver._start_verifier(4)
    assert result == (None if status == 'resolved' else (job, process, record))


@pytest.mark.parametrize('prepared', [False, True])
def test_only_stale_queued_verifier_reroutes_without_waiting_for_another_event(tmp_path, monkeypatch, prepared):
    from contextlib import nullcontext
    from types import SimpleNamespace
    from trellis.dispatch.coordination import Coordinator
    monkeypatch.setattr('trellis.dispatch.storage.BackgroundCollector.schedule', lambda *a: None)

    async def run():
        driver = ProductionDriver.__new__(ProductionDriver)
        driver.root = driver.repo = driver.runtime = tmp_path
        driver.require_no_halt = lambda: None
        driver.pausing = lambda: False
        driver.now_ms = lambda: 100
        driver.measurements = SimpleNamespace(sample=lambda _: None)
        def job(id):
            return dict(id=id, status={'status': 'queued'}, required=True, reviewer_requested=False,
                        identity={'compiler': 'none', 'input_hashes': {'unchanged': 'source'}})
        jobs = {'4': job(4)}
        routing_pending = False
        events = []
        def status():
            return dict(stage='Reviewer', tasks={}, reviewer=None, reviewer_runnable=False,
                routing_pending=routing_pending, config={'verifier_slots': 1},
                verifiers={'jobs': copy.deepcopy(jobs)},
                verifiers_runnable=any(j['status']['status'] != 'resolved' for j in jobs.values()))
        def native(payload):
            nonlocal routing_pending
            if payload['action'] == 'dispatch_prepare_verifier':
                events.append(('check', payload['id']))
                return None if payload['id'] == 4 else {'fresh': True}
            if payload['action'] == 'dispatch_routing':
                event = payload['event']
                events.append((event['kind'], event.get('id')))
                if event['kind'] == 'supersede_queued':
                    jobs[str(event['id'])]['status'] = {'status': 'resolved'}
                    routing_pending = True
                elif event['kind'] == 'route':
                    assert event['verifier_context_version'] == 5
                    assert event['suppress_sound_review_self_wakeup'] is True
                    assert routing_pending
                    jobs['5'] = job(5)
                    routing_pending = False
                elif event['kind'] == 'verifier_launched':
                    assert event['id'] == 5
                    jobs['5']['status'] = {'status': 'running', 'process': event['process']}
            return status()
        driver.native = SimpleNamespace(_call=native, locked=nullcontext,
            _require_recovered_publication=lambda: None, prepare_retries=lambda *_, **kw: None)
        driver.state = lambda: dict(dispatch={'verifiers': {'jobs': copy.deepcopy(jobs)}})
        def prepare(directory, private):
            assert private == {'fresh': True}
            directory.mkdir(exist_ok=True)
            (directory / 'prepared.json').write_text('{}')
        driver._prepare = prepare
        if prepared:
            directory = tmp_path / 'verifier-4'
            directory.mkdir(); (directory / 'prepared.json').write_text('{}')
        def process(directory, **kwargs):
            assert directory == tmp_path / 'verifier-5', 'stale panel reached the launcher'
            events.append(('launcher', 5))
            return SimpleNamespace(root=directory / 'execution'), {'pid': 123}
        driver._process = process
        permitted = asyncio.Event()
        loop = asyncio.get_running_loop()
        def permit(*args):
            events.append(('provider', 5))
            loop.call_soon_threadsafe(permitted.set)
        driver._permit = permit
        async def wait(*args):
            await asyncio.Future()
        driver._wait = wait
        async def recover(): pass
        driver.recover_publication = recover
        coordinator = Coordinator(driver)
        running = asyncio.create_task(coordinator.run())
        try:
            await asyncio.wait_for(permitted.wait(), 2)
            assert jobs['4']['status']['status'] == 'resolved'
            assert events.count(('check', 4)) == 1
            assert events.index(('supersede_queued', 4)) < events.index(('route', None)) < events.index(('provider', 5))
        finally:
            await coordinator.shutdown()
            await asyncio.wait_for(running, 2)
    asyncio.run(run())


@pytest.mark.parametrize('handoffs', [None, [], [{'task_id': 9, 'files': [], 'unavailable': ['missing note']} ]])
@pytest.mark.parametrize('kind', ['Paper', 'Worker', 'review'])
def test_frozen_ordinary_request_is_resolved_once_before_launch(tmp_path, handoffs, kind):
    import json
    from types import SimpleNamespace
    directory = tmp_path / '1'
    directory.mkdir()
    calls = []
    resolved = dict(kind=kind, paper_verify_lane_bindings={'v1': {'model': 'strong'}})
    if handoffs is not None:
        (directory / 'worker-handoffs.json').write_text(json.dumps(handoffs))
    class Process:
        def start_or_adopt(self, command, payload, **kwargs):
            expected = dict(resolved, worker_handoffs=handoffs) if handoffs is not None else resolved
            assert payload['request'] == expected
            return {'pid': 1}
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = tmp_path
    driver.repo = tmp_path / 'repo'
    driver.runtime, driver.view, driver.environment = tmp_path, tmp_path, {}
    driver.native = SimpleNamespace(_call=lambda request: calls.append(request) or resolved)
    driver.process_factory = lambda _: Process()
    driver._process(directory, compiler='none', seconds=3600)
    driver._process(directory, compiler='none', seconds=3600)
    assert calls == [dict(action='dispatch_prepared_request', root=str(directory / 'runtime'))]
    assert json.loads((directory / 'dispatch-request.json').read_text()) == resolved


@pytest.mark.parametrize('cached', [False, True])
@pytest.mark.parametrize('kind, shared', [('Worker', True), ('review', True),
                                        ('stuck_math_audit', True), ('Sound', False)])
def test_host_payload_avoids_new_request_cache_but_keeps_validation_and_mounts(tmp_path, cached, kind, shared):
    import json
    import os
    from types import SimpleNamespace
    directory = tmp_path / '1/acceptance/1'
    directory.mkdir(parents=True)
    published = tmp_path / 'repo/.trellis/chats/parallel'
    published.mkdir(parents=True)
    cache = directory / 'dispatch-request.json'
    resolved = dict(id=11, kind=kind, body='frozen host request')
    before = json.dumps(resolved).encode() if cached else None
    if cached:
        cache.write_bytes(before)
    native_calls, launches = [], []
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.runtime = tmp_path
    driver.repo, driver.view = tmp_path / 'repo', None
    driver.environment = {'TRELLIS_SANDBOX_EXTRA_RO_PATHS': '/existing/support'}
    driver.native = SimpleNamespace(_call=lambda request: native_calls.append(request) or dict(resolved))
    def launch(command, payload, **kwargs):
        launches.append((command, copy.deepcopy(payload), kwargs))
        return {'pid': 1}
    driver.process_factory = lambda _: SimpleNamespace(start_or_adopt=launch)
    command = ['python3', '-m', 'trellis.dispatch.staged_acceptance']
    payload = dict(stage='owned', candidate='sealed', captured_inputs={'Tablet/A.lean': 'abc'})
    for _ in range(2):  # A restart retains the same custom recipe inputs.
        driver._process(directory, compiler='managed', seconds=None, command=command, payload=dict(payload))
    assert len(native_calls) == (0 if cached else 2)
    assert all(call == dict(action='dispatch_prepared_request', root=str(directory / 'runtime'))
               for call in native_calls)
    assert (cache.read_bytes() if cache.exists() else None) == before
    for actual_command, actual_payload, options in launches:
        assert actual_command == command
        assert actual_payload == payload
        assert options['environment']['TRELLIS_SANDBOX_EXTRA_RO_PATHS'] == '/existing/support' + (
            os.pathsep + str(published) if shared else '')


def test_host_request_validation_failure_cannot_launch_or_create_cache(tmp_path):
    from types import SimpleNamespace
    directory = tmp_path / '1/acceptance/1'
    directory.mkdir(parents=True)
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.runtime = tmp_path
    def invalid(_):
        raise RuntimeError('invalid frozen state')
    driver.native = SimpleNamespace(_call=invalid)
    driver.process_factory = lambda _: pytest.fail('invalid host request must not launch')
    with pytest.raises(RuntimeError, match='invalid frozen state'):
        driver._process(directory, compiler='managed', seconds=None, payload={'candidate': 'sealed'})
    assert not (directory / 'dispatch-request.json').exists()


@pytest.mark.parametrize('legacy_lifetime', ['absent', 4200, None])
def test_acceptance_grants_follow_owner_but_adopt_existing_recipe(tmp_path, legacy_lifetime):
    import json
    from types import SimpleNamespace
    directory = tmp_path / '1/acceptance/1'
    execution = directory / 'execution'; execution.mkdir(parents=True)
    (directory / 'dispatch-request.json').write_text(json.dumps({'kind': 'worker'}))
    recipe = execution / 'recipe.json'
    if legacy_lifetime != 'absent':
        recipe.write_text(json.dumps({'checker': {'lifetime': legacy_lifetime}}))
    before = recipe.read_bytes() if recipe.exists() else None
    calls = []
    process = SimpleNamespace(start_or_adopt=lambda *args, **kw: calls.append(kw) or {'pid': 1})
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.runtime = tmp_path
    driver.view, driver.environment = None, {}
    driver.repo = tmp_path / 'repo'
    driver.process_factory = lambda _: process
    driver._process(directory, compiler='managed', seconds=None,
        command=['python3', '-m', 'trellis.dispatch.acceptance'], payload={'candidate': 'sealed'})
    assert calls[0]['checker']['lifetime'] == (None if legacy_lifetime == 'absent' else legacy_lifetime)
    assert (recipe.read_bytes() if recipe.exists() else None) == before


def test_acceptance_identity_ignores_numbering_but_not_semantics():
    from trellis.dispatch.driver import acceptance_identity
    before = dict(request_seq=7, in_flight_request=dict(id=7, cycle=4, blockers=[]),
                  live={'open_nodes': ['A']}, dispatch=None)
    current = copy.deepcopy(before)
    current['request_seq'] = 20
    current['in_flight_request']['id'] = 20
    saved = copy.deepcopy(current)
    assert acceptance_identity(before) == acceptance_identity(current)
    assert current == saved  # Normalizing the identity must not change the request.
    from trellis.dispatch.candidate import canonical_json, digest
    saved['request_seq'] = 0
    saved['in_flight_request']['id'] = 0
    assert acceptance_identity(current) == digest(canonical_json(saved))
    current['in_flight_request']['blockers'] = ['a new restriction']
    assert acceptance_identity(before) != acceptance_identity(current)
    current = copy.deepcopy(before)
    current['live']['open_nodes'].append('B')
    assert acceptance_identity(before) != acceptance_identity(current)


def test_verifier_queue_refills_before_slow_panel_finishes():
    async def run():
        driver = ProductionDriver.__new__(ProductionDriver)
        notifications = []
        driver.set_notifier(lambda: notifications.append(True))
        jobs = {str(id): dict(id=id, status={'status': 'queued'}, required=True,
                             reviewer_requested=False) for id in (1, 2, 3)}
        started = {id: asyncio.Event() for id in (1, 2, 3)}
        release = {id: asyncio.Event() for id in (1, 2, 3)}
        active = set()
        launches = []
        async def snapshot():
            return dict(stage='Reviewer', config={'verifier_slots': 2}, verifiers={'jobs': copy.deepcopy(jobs)})
        async def verifier(job):
            id = job['id']
            assert id not in active
            active.add(id)
            assert len(active) <= 2
            launches.append(id)
            jobs[str(id)]['status'] = {'status': 'running'}
            started[id].set()
            await release[id].wait()
            jobs[str(id)]['status'] = {'status': 'resolved'}
            active.remove(id)
        driver.snapshot, driver._verifier = snapshot, verifier
        runner = asyncio.create_task(driver.run_verifiers())
        await asyncio.wait_for(started[1].wait(), 1)
        await asyncio.wait_for(started[2].wait(), 1)
        assert not started[3].is_set()
        release[1].set()
        await asyncio.wait_for(started[3].wait(), 1)
        assert active == {2, 3}, 'a slow panel must not block replenishing an independent slot'
        assert notifications, 'completion should wake the reviewer/coordinator'
        release[2].set()
        release[3].set()
        await asyncio.wait_for(runner, 1)
        assert launches == [1, 2, 3]
    asyncio.run(run())


@pytest.mark.parametrize('waiter_kind', ['prelaunch', 'finishing'])
@pytest.mark.parametrize('boundary', ['exclusive', 'pv_revision', 'withdrawn', 'waiter_failure'])
def test_exclusive_transition_joins_natively_resolved_verifier_waiters(waiter_kind, boundary):
    from contextlib import nullcontext
    from types import SimpleNamespace
    async def scenario():
        driver = ProductionDriver.__new__(ProductionDriver)
        driver.set_notifier(lambda: None)
        jobs = {str(id): dict(id=id, status={'status': 'queued'}, required=True,
                             reviewer_requested=False) for id in (17, 18)}
        waiting, release, joined = asyncio.Event(), asyncio.Event(), asyncio.Event()
        snapshots, exclusive_calls, cancelled = [], [], []
        driver.native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=lambda: None)
        driver.state = lambda: {'dispatch': {'verifiers': {'jobs': jobs}}}
        driver._process = lambda *a, **k: pytest.fail('superseded verifier must not launch or receive permission')
        async def snapshot():
            snapshots.append(True)
            stage = 'Reviewer'
            if len(snapshots) >= 2:
                stage = 'StuckMathAudit'
                # The completed panel allowed native routing to supersede the
                # queued peer (or finish resolving its already-reaped peer).
                jobs['18']['status'] = {'status': 'resolved'}
                release.set()
            if len(snapshots) >= 3:
                assert joined.is_set(), 'do not reuse the snapshot from before joining'
                if boundary == 'withdrawn':
                    stage = 'Reviewer'
            return dict(stage=stage, pv_revision_ready=boundary == 'pv_revision' and len(snapshots) >= 2,
                config={'verifier_slots': 2}, verifiers={'jobs': copy.deepcopy(jobs)})
        async def verifier(job):
            if job['id'] == 17:
                await waiting.wait()
                jobs['17']['status'] = {'status': 'resolved'}
                return
            if waiter_kind == 'finishing':
                jobs['18']['status'] = {'status': 'checking'}
            waiting.set()
            try:
                await release.wait()
                if waiter_kind == 'prelaunch':
                    # Exercise the real locked launch recheck after native
                    # routing supersedes the job, rather than merely a no-op.
                    assert driver._start_verifier(18) is None
                if boundary == 'waiter_failure':
                    raise RuntimeError('verifier completion failed')
                joined.set()
            except asyncio.CancelledError:
                cancelled.append(True)
                raise
        async def exclusive(status):
            assert joined.is_set() and len(snapshots) == 3
            assert all(j['status']['status'] == 'resolved' for j in jobs.values())
            exclusive_calls.append(status['stage'])
        driver.snapshot, driver._verifier, driver._exclusive = snapshot, verifier, exclusive
        if boundary == 'waiter_failure':
            with pytest.raises(RuntimeError, match='verifier completion failed'):
                await asyncio.wait_for(driver.run_verifiers(), 1)
        else:
            await asyncio.wait_for(driver.run_verifiers(), 1)
            assert joined.is_set() and len(snapshots) == 3
        assert not cancelled, 'terminal waiters must finish naturally and expose their errors'
        assert exclusive_calls == (['StuckMathAudit'] if boundary == 'exclusive' else [])
    asyncio.run(scenario())


@pytest.mark.parametrize('status', ['running', 'checking'])
@pytest.mark.parametrize('pv_revision_ready', [False, True])
def test_exclusive_transition_rejects_durable_live_verifier_without_local_waiter(status, pv_revision_ready):
    async def scenario():
        driver = ProductionDriver.__new__(ProductionDriver)
        async def snapshot():
            return dict(stage='StuckMathAudit', pv_revision_ready=pv_revision_ready,
                verifiers={'jobs': {'18': dict(id=18, status={'status': status})}})
        driver.snapshot = snapshot
        driver._exclusive = lambda _: pytest.fail('live durable verifier must prevent exclusive work')
        with pytest.raises(RuntimeError, match='exclusive routing crossed live verifier attempts'):
            await driver.run_verifiers()
    asyncio.run(scenario())


@pytest.mark.parametrize('native_job', ['queued', 'missing'])
def test_exclusive_transition_requires_native_resolution_before_joining_waiter(native_job):
    async def scenario():
        driver = ProductionDriver.__new__(ProductionDriver)
        driver.set_notifier(lambda: None)
        jobs = {str(id): dict(id=id, status={'status': 'queued'}, required=True,
                             reviewer_requested=False) for id in (17, 18)}
        waiting, cancelled = asyncio.Event(), asyncio.Event()
        count = 0
        async def snapshot():
            nonlocal count
            count += 1
            if count == 2 and native_job == 'missing':
                del jobs['18']
            return dict(stage='Reviewer' if count == 1 else 'StuckMathAudit',
                config={'verifier_slots': 2}, verifiers={'jobs': copy.deepcopy(jobs)})
        async def verifier(job):
            if job['id'] == 17:
                await waiting.wait()
                jobs['17']['status'] = {'status': 'resolved'}
                return
            waiting.set()
            try:
                await asyncio.Event().wait()
            finally:
                cancelled.set()
        driver.snapshot, driver._verifier = snapshot, verifier
        driver._exclusive = lambda _: pytest.fail('unresolved waiter must prevent exclusive work')
        with pytest.raises(RuntimeError, match='exclusive routing crossed live verifier attempts'):
            await asyncio.wait_for(driver.run_verifiers(), 1)
        assert cancelled.is_set()  # Fault shutdown parks the attempt as before.
        assert '18' not in jobs or jobs['18']['status']['status'] == 'queued'
    asyncio.run(scenario())


def test_verifier_host_fault_parks_other_waiters_without_fabricating_outcomes():
    async def run():
        driver = ProductionDriver.__new__(ProductionDriver)
        driver.set_notifier(lambda: None)
        jobs = {str(id): dict(id=id, status={'status': 'running'}, required=True,
                             reviewer_requested=False) for id in (1, 2)}
        peer = asyncio.Event()
        parked = asyncio.Event()
        async def snapshot():
            return dict(stage='Reviewer', config={'verifier_slots': 2}, verifiers={'jobs': copy.deepcopy(jobs)})
        async def verifier(job):
            if job['id'] == 1:
                await peer.wait()
                raise RuntimeError('compiler service unavailable')
            peer.set()
            try:
                await asyncio.Event().wait()
            finally:
                parked.set()
        driver.snapshot, driver._verifier = snapshot, verifier
        with pytest.raises(RuntimeError, match='compiler service unavailable'):
            await asyncio.wait_for(driver.run_verifiers(), 1)
        assert parked.is_set()
        assert all(j['status']['status'] == 'running' for j in jobs.values())
    asyncio.run(run())


@pytest.mark.parametrize('fault', [None, 'blocked_transition', 'response', 'native_binding', 'cleanup', 'channel_ack'])
def test_paused_verifier_drains_exact_saved_result_then_resume_delivers_it(tmp_path, monkeypatch, fault):
    from contextlib import nullcontext
    from types import SimpleNamespace
    from trellis.dispatch import candidate, process as processes, resources
    from trellis.dispatch.coordination import Coordinator

    async def scenario():
        driver = ProductionDriver.__new__(ProductionDriver)
        driver.root = driver.repo = driver.runtime = tmp_path
        driver.finalization_gate = asyncio.Lock()
        driver.require_no_halt = lambda: None
        paused = [fault != 'blocked_transition']
        driver.pausing = lambda: paused[0]
        driver.now_ms = lambda: 100
        events = []
        identity = dict(pid=123, start_ticks=4, boot_id='saved-launcher')
        response = dict(kind='verifier', preserved='exact saved provider response')
        raw = candidate.canonical_json(response)
        execution = tmp_path / 'verifier-91/execution'
        execution.mkdir(parents=True)
        (execution / 'response.json').write_bytes(raw)
        (execution / 'result.json').write_bytes(candidate.canonical_json(dict(
            returncode=0, response_sha256=candidate.digest(raw))))
        for name in ('process.json', 'permit.json', 'cleanup.json'):
            (execution / name).write_bytes(candidate.canonical_json(identity))
        original = {p.name: p.read_bytes() for p in execution.iterdir()}
        if fault == 'response':
            (execution / 'response.json').write_text('{}')
        if fault == 'cleanup':
            (execution / 'cleanup.json').write_text('{}')
        sealed = dict(status='checking', result_sha256='e' * 64)
        jobs = {'91': dict(id=91, identity={'compiler': 'none'}, status=copy.deepcopy(sealed)),
                '92': dict(id=92, identity={'compiler': 'none'}, status={'status': 'queued'},
                           required=True, reviewer_requested=False)}
        cursor = dict(request_id=268)
        state = dict(dispatch=dict(verifiers=dict(jobs=jobs), reviewer=cursor))
        driver.state = lambda: state
        driver.native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=lambda: None,
            _call=lambda _: pytest.fail('paused adoption must not prepare a new verifier'))
        def adopted(directory, **_):
            assert directory == tmp_path / 'verifier-91'
            events.append('adopt saved process')
            return processes.AttemptProcess(execution), identity
        driver._process = adopted
        driver._permit = lambda *_: pytest.fail('saved result must not grant another provider permit')
        driver._record_transport = lambda directory, actual: events.append(('transport', actual))
        driver._prepare_verifiers = lambda _: events.append('prepare verifiers')
        def routing(event):
            assert event['id'] == 91 and event['response'] == response
            events.append(event['kind'])
            if event['kind'] == 'verifier_candidate':
                if fault == 'native_binding':
                    raise RuntimeError('verifier result differs from sealed candidate')
                assert jobs['91']['status'] == sealed
            elif event['kind'] == 'verifier_finished':
                assert not paused[0]
                if fault == 'blocked_transition' and events.count('verifier_finished') == 1:
                    # The completed reviewer is retained during deployment;
                    # native exclusive_ready cannot consume this verdict yet.
                    paused[0] = True
                else:
                    jobs['91']['status'] = dict(status='resolved', result_sha256=sealed['result_sha256'])
            else:
                pytest.fail('unexpected native transition')
            return dict(verifiers=dict(jobs=copy.deepcopy(jobs)))
        driver._routing = routing
        async def snapshot():
            return dict(stage='Reviewer', pausing=paused[0], config={'verifier_slots': 1},
                tasks={}, reviewer=cursor, verifiers=dict(jobs=copy.deepcopy(jobs)),
                verifiers_runnable=True)
        driver.snapshot = snapshot
        monkeypatch.setattr(processes, 'alive', lambda _: False)
        monkeypatch.setattr(processes.subprocess, 'run', lambda *a, **k: None)
        monkeypatch.setattr(resources, 'revoke', lambda _: events.append('revoke'))
        def drain(_):
            if fault == 'channel_ack':
                raise RuntimeError('compiler cleanup was not acknowledged')
            events.append('channels drained')
        monkeypatch.setattr(resources, 'drain', drain)
        coordinator = Coordinator(driver)
        try:
            if fault == 'blocked_transition':
                coordinator.verifiers = coordinator._start(driver.run_verifiers())
            else:
                await coordinator.tick()
            assert not coordinator.closed and coordinator.verifiers is not None
            if fault not in (None, 'blocked_transition'):
                expected = dict(response='completion receipt', native_binding='sealed candidate',
                    cleanup='descendant cleanup', channel_ack='compiler cleanup')[fault]
                with pytest.raises(RuntimeError, match=expected):
                    await coordinator.verifiers
                with pytest.raises(RuntimeError, match=expected):
                    await coordinator.tick()
                assert not coordinator.closed
                assert jobs['91']['status'] == sealed
                return
            assert await coordinator.verifiers == {91: sealed}
            await coordinator.tick()
            assert coordinator.closed and state['dispatch']['reviewer'] == cursor
            assert jobs['91']['status'] == sealed and jobs['92']['status']['status'] == 'queued'
            finish_count = 1 if fault == 'blocked_transition' else 0
            assert events.count('verifier_finished') == finish_count
            assert events.count('prepare verifiers') == (1 if fault == 'blocked_transition' else 0)
            assert events.count('adopt saved process') == 1
            assert events.index('verifier_candidate') < events.index('channels drained')
            assert original == {p.name: p.read_bytes() for p in execution.iterdir()}

            # Resume uses no in-memory drain acknowledgment: it authenticates
            # and reaps the same saved process before ordinary native delivery.
            paused[0] = False
            await driver._verifier(copy.deepcopy(jobs['91']))
            assert jobs['91']['status']['status'] == 'resolved'
            assert events.count('adopt saved process') == 2
            assert events.count('verifier_finished') == finish_count + 1
            assert original == {p.name: p.read_bytes() for p in execution.iterdir()}
        finally:
            await coordinator.shutdown()
    asyncio.run(scenario())


def test_queued_verifier_launch_rechecks_pause_under_lock(tmp_path):
    from contextlib import nullcontext
    from types import SimpleNamespace
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.repo = driver.runtime = tmp_path
    (tmp_path / 'pause_request.json').write_text('{}')
    driver.native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=lambda: None)
    driver.state = lambda: dict(dispatch=dict(verifiers=dict(jobs={
        '1': dict(id=1, status={'status': 'queued'})})))
    driver._process = lambda *a, **kw: pytest.fail('pause must prevent a fresh launch')
    assert driver._start_verifier(1) is None


def test_reviewer_start_withdrawn_before_lock_does_not_prepare_workspace(tmp_path):
    from contextlib import nullcontext
    from types import SimpleNamespace
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.runtime = driver.repo = tmp_path
    driver.require_no_halt = lambda: None
    driver.native = SimpleNamespace(locked=nullcontext,
        _start_review_locked=lambda: dict(reviewer=None, reviewer_runnable=False))
    driver._prepare = lambda *_: pytest.fail('no reviewer was admitted')
    assert driver._start_review() is None
    assert list(tmp_path.iterdir()) == []


@pytest.mark.parametrize('marker', ['pause_request.json', '.trellis-stop-after-checkpoint'])
@pytest.mark.parametrize('retained', [False, True])
def test_reviewer_start_rechecks_pause_after_native_lock(tmp_path, marker, retained):
    from contextlib import contextmanager
    from types import SimpleNamespace
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.runtime = driver.repo = tmp_path
    driver.require_no_halt = lambda: None
    if retained:
        (tmp_path / 'original-response.json').write_text('{"original": true}')
    originals = {p.name: p.read_bytes() for p in tmp_path.iterdir()}
    assert not driver.pausing()  # The coordinator's earlier snapshot.
    @contextmanager
    def locked():
        (tmp_path / marker).write_text('original ordinary drain\n')
        yield
    driver.native = SimpleNamespace(locked=locked,
        _start_review_locked=lambda: pytest.fail('paused admission must not reach native ReviewStarted or adoption'))
    driver._prepare_fresh = lambda *a, **kw: pytest.fail('paused admission must not prepare a workspace')
    assert driver._start_review() is None
    assert {p.name: p.read_bytes() for p in tmp_path.iterdir()} == {
        **originals, marker: b'original ordinary drain\n'}


@pytest.mark.parametrize("advisory", [None, [{"kind": "NodeCorr", "object": {"otype": "node", "node": "SmallPairRows"}}]])
def test_new_reviewer_context_advertises_ordinal_helper_prefix(tmp_path, advisory):
    import json
    from contextlib import nullcontext
    from types import SimpleNamespace
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.runtime = driver.repo = tmp_path
    driver.require_no_halt = lambda: None
    captured = tmp_path / 'reviews/793/state.json'
    captured.parent.mkdir(parents=True)
    captured.write_text(json.dumps(dict(dispatch={}, in_flight_request=dict(
        lean_closure_candidates=['Old', 'Fresh'],
        **({} if advisory is None else {'deferred_verification_advisory': advisory})))))
    cursor = {'request_id': 793}
    status = dict(config=dict(profiles={}, compiler_slots=2, verifier_slots=1),
        available_slots=1, draining=None, tasks={'9': dict(id=9, status={'status': 'running'},
            assignment=dict(profile='luna-high', task_kind='lean_closure', scope={'nodes': ['C']}))})
    # Native status serializes map keys lexicographically: the latest review
    # crosses a digit boundary and sorts before older IDs in that JSON object.
    receipts = {str(i): dict(review_id=i, decisions=[]) for i in range(990, 1002)}
    receipts['1001']['decisions'] = [dict(assignment_id='bad', task_id=None, rejection='dismissed_tasks')]
    from trellis.dispatch.candidate import canonical_json, digest
    prior = dict(kind='parallel_review', assignments=[], action=None,
                 reason='Six proposals read the running ordinary dependency writer; wait for its publication.')
    prior_path = tmp_path / 'review-1001/execution/response.json'
    prior_path.parent.mkdir(parents=True)
    prior_path.write_bytes(canonical_json(prior))
    receipts['1001']['response_sha256'] = digest(canonical_json(prior))
    status['lightweight_attempts_per_node'] = 2
    status['lightweight_attempts_by_node'] = {'older-node-not-in-recent-tasks': 2}
    status['receipts'] = dict(sorted(receipts.items()))
    status['tasks']['1'] = dict(id=1, launched_at_ms=0,
        binding=dict(provider='openai', model='old-model', reasoning_effort='high'),
        status=dict(status='resolved', outcome=dict(kind='stuck')),
        assignment=dict(profile='old-profile', task_kind='lean_closure', scope=dict(nodes=['Old'])))
    status['tasks']['100'] = dict(id=100, launched_at_ms=100,
        status=dict(status='running'), assignment=dict(profile='current-profile',
            task_kind='ordinary', scope=dict(nodes=['Current'])))
    status['reviewer'] = cursor
    driver.native = SimpleNamespace(_start_review_locked=lambda: status, locked=nullcontext)
    driver._prepare = lambda directory, _: directory.mkdir(exist_ok=True)
    driver.measurements = SimpleNamespace(value={})
    assert driver._start_review() == cursor
    context_path = tmp_path / 'review-793/review-context.json'
    context = json.loads(context_path.read_text())
    assert context['deferred_verification_advisory'] == (advisory or [])
    assert context['helper_prefix'] == 'P793_T<assignment-number>_'
    assert context['tasks'][0]['task_kind'] == 'lean_closure'
    assert context['lightweight_attempts_per_node'] == 2
    assert context['lightweight_attempts_by_node'] == {'older-node-not-in-recent-tasks': 2}
    assert 1 not in [t['id'] for t in context['tasks']]
    assert context['closure_attempts_by_node']['Old'][0]['model'] == 'old-model'
    assert context['closure_attempts_by_node']['Old'][0]['native_outcomes'] == {'stuck': 1}
    assert context['closure_candidates_without_recorded_launches'] == ['Fresh']
    assert [r['review_id'] for r in context['dispatch_receipts']] == list(range(992, 1002))
    assert context['dispatch_receipts'][-1]['decisions'][0]['rejection'] == 'dismissed_tasks'
    assert context['recent_reviewer_rationales'] == [dict(review_id=1001,
        response_sha256=receipts['1001']['response_sha256'], reason=prior['reason'], truncated=False)]
    # A reviewer already launched with the old context keeps its frozen hint.
    context['helper_prefix'] = 'P793_<assignment-id>_'
    context_path.write_text(json.dumps(context))
    prior_path.write_text('{}')  # Adoption retains its original captured history.
    status['tasks']['1']['binding']['model'] = 'later-model'
    driver._start_review()
    assert json.loads(context_path.read_text()) == context


def test_reviewer_adoption_keeps_frozen_worker_handoffs(tmp_path, monkeypatch):
    import json
    from contextlib import nullcontext
    from types import SimpleNamespace
    from trellis.dispatch import handoffs
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.runtime = driver.repo = tmp_path
    driver.require_no_halt = lambda: None
    captured = tmp_path / 'reviews/793/state.json'
    captured.parent.mkdir(parents=True); captured.write_text('{"dispatch": {}}')
    cursor = {'request_id': 793}
    status = dict(config=dict(profiles={}, compiler_slots=2, verifier_slots=1),
                  available_slots=1, draining=None, tasks={})
    status['reviewer'] = cursor
    driver.native = SimpleNamespace(_start_review_locked=lambda: status, locked=nullcontext)
    driver._prepare = lambda directory, _: directory.mkdir(exist_ok=True)
    driver.measurements = SimpleNamespace(value={})
    calls = []
    def capture(*args):
        calls.append(args)
        return [{'task_id': 9, 'files': [{'path': '.trellis/scratch/worker-handoffs/task-9/helper.lean'}]}]
    monkeypatch.setattr(handoffs, 'worker_handoffs', capture)
    driver._start_review()
    path = tmp_path / 'review-793/review-context.json'
    original = path.read_bytes()
    assert json.loads(original)['worker_handoffs'][0]['task_id'] == 9
    driver._start_review()
    assert len(calls) == 1 and path.read_bytes() == original


@pytest.fixture
def reviewer_driver(tmp_path):
    """Real preparation/receipts, with the native event boundary simulated."""
    import json
    import subprocess
    from contextlib import contextmanager
    from types import SimpleNamespace
    from trellis.dispatch.native import NativeLedger
    from trellis.dispatch.workspace import prepare

    repo, runtime = tmp_path / 'repo', tmp_path / 'runtime'
    (repo / 'Tablet').mkdir(parents=True); runtime.mkdir()
    (repo / 'Tablet/A.lean').write_text('-- captured source\n')
    config = repo / 'trellis.config.json'
    config.write_text(json.dumps(dict(repo_path=str(repo), workflow={})))
    for args in [('init',), ('config', 'user.name', 'Test'),
                 ('config', 'user.email', 'test@example.invalid'), ('add', '.'),
                 ('commit', '-m', 'base')]:
        subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True)
    (runtime / 'runtime_metadata.json').write_text(json.dumps(dict(repo_path=str(repo))))
    state = dict(request_seq=7, in_flight_request={'id': 7}, dispatch={})
    (runtime / 'protocol_state.json').write_text(json.dumps(state))
    status = dict(reviewer=None, reviewer_runnable=True, config=dict(profiles={}, compiler_slots=2, verifier_slots=1),
                  available_slots=1, draining=None, tasks={})
    calls, preparations, launches = [], [], []
    held = False
    def kernel(payload):
        assert held, 'native status must remain in the preparation lock'
        calls.append(payload['event'])
        if payload['event'] is not None:
            assert payload['event'] == dict(kind='review_started', request_id=7)
            assert status['reviewer'] is None
            status['reviewer'] = dict(request_id=7, observed_event=1)
        return dict(status='dispatch_ok', output=copy.deepcopy(status))
    native = NativeLedger(runtime, kernel=kernel)
    locked = native.locked
    @contextmanager
    def tracked_lock():
        nonlocal held
        assert not held, 'preparation must not reacquire its own file lock'
        with locked():
            held = True
            try:
                yield
            finally:
                held = False
    native.locked = tracked_lock
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root, driver.runtime, driver.repo = native.root, runtime, repo
    driver.config_path, driver.native = config, native
    driver.measurements = SimpleNamespace(value={})
    driver.require_no_halt = lambda: None
    def prepare_workspace(*args, **kwargs):
        assert held, 'native status reuse cannot outlive the preparation lock'
        preparations.append(args[3])
        return prepare(*args, **kwargs, seed_packages=False)
    driver.prepare_workspace = prepare_workspace
    def process(directory, **kwargs):
        assert not held
        assert (directory / 'prepared.json').exists()
        assert (directory / 'review-context.json').exists()
        execution = directory / 'execution'; execution.mkdir(exist_ok=True)
        (execution / 'recipe.json').write_text('{}')
        launches.append(directory)
        return SimpleNamespace(root=execution, stop_and_reap=lambda _: None), {'pid': 1}
    driver._process = process
    driver._permit = lambda *args: None
    async def wait(*args): return {}
    driver._wait = wait
    driver._record_transport = driver._dispatch_review = lambda *args: None
    driver.finalization_gate = asyncio.Lock()
    return driver, status, calls, preparations, launches


def test_fresh_review_prepares_once_and_reuses_locked_native_status(reviewer_driver):
    driver, status, calls, preparations, launches = reviewer_driver
    async def run():
        cursor = await driver.start_review()
        await driver.review(cursor, prepared=True)
    asyncio.run(run())
    assert calls == [None, dict(kind='review_started', request_id=7)]
    assert len(preparations) == len(launches) == 1
    assert (driver.root / 'reviews/7/state.json').exists()


def test_review_delivery_waits_for_acceptance_finalization_gate(reviewer_driver):
    driver, _, _, _, _ = reviewer_driver
    delivered = []
    driver._dispatch_review = lambda *_: delivered.append(True)
    async def run():
        cursor = await driver.start_review()
        await driver.finalization_gate.acquire()
        finished = asyncio.Event()
        async def wait(*_):
            finished.set()
            return {}
        driver._wait = wait
        review = asyncio.create_task(driver.review(cursor, prepared=True))
        await finished.wait()
        await asyncio.sleep(.02)
        assert not review.done() and not delivered
        driver.finalization_gate.release()
        await review
    asyncio.run(run())
    assert delivered == [True]


@pytest.mark.parametrize('crash_after', ['event', 'capture', 'workspace', 'context'])
def test_reviewer_adoption_completes_each_prelaunch_crash_window(reviewer_driver, crash_after):
    driver, status, calls, preparations, launches = reviewer_driver
    cursor = dict(request_id=7, observed_event=1)
    if crash_after == 'event':
        # The native start committed, but Python never captured its snapshot.
        status['reviewer'] = cursor
    elif crash_after == 'capture':
        assert driver.native.start_review() == cursor
    else:
        assert driver._start_review() == cursor
        if crash_after == 'workspace':
            (driver.root / 'review-7/review-context.json').unlink()
    captured = driver.root / 'reviews/7/state.json'
    original = captured.read_bytes() if captured.exists() else None
    calls.clear(); preparations.clear()
    asyncio.run(driver.review(cursor))  # Adoption never inherits the fresh flag.
    assert calls == [None], 'adoption must validate current status without restarting review'
    assert len(preparations) == len(launches) == 1
    if original is not None:
        assert captured.read_bytes() == original


def test_launched_reviewer_adoption_still_refuses_changed_prepared_inputs(reviewer_driver):
    driver, status, calls, preparations, launches = reviewer_driver
    cursor = driver._start_review()
    asyncio.run(driver.review(cursor, prepared=True))
    marker = driver.root / 'review-7/prepared.json'
    original = marker.read_bytes()
    # A changed configuration must still be checked against the frozen receipt
    # on recovery; it cannot silently overwrite already launched inputs.
    driver.config_path.write_text(driver.config_path.read_text() + '\n')
    with pytest.raises(RuntimeError, match='different prepared inputs'):
        asyncio.run(driver.review(cursor))
    assert marker.read_bytes() == original
    assert len(launches) == 1


def test_reviewer_context_has_active_and_ten_recent_verifier_jobs_without_contracts(reviewer_driver):
    import json
    driver, status, calls, preparations, launches = reviewer_driver
    jobs = {}
    for id in range(20):
        phase = ['queued', 'running', 'checking'][id] if id < 3 else 'resolved'
        jobs[str(id)] = dict(id=id, identity=dict(lane='substantiveness', input_hashes={'private': 'omit'}),
            required=id % 2 == 0, reviewer_requested=id % 2 == 1,
            status=dict(status=phase, result_sha256='omit', process={'pid': 123},
                        stale_reason='source inputs changed' if id == 19 else None),
            request={'large_frozen_contract': 'omit'})
        if id != 0:  # Older compact status lacks these optional fields.
            jobs[str(id)].update(nodes=[f'N{id}'], targets=[f'T{id}'])
    status['verifiers'] = dict(jobs=dict(sorted(jobs.items())))
    driver._start_review()
    path = driver.root / 'review-7/review-context.json'
    captured = path.read_bytes()
    summaries = json.loads(captured)['verifier_jobs']
    assert [job['id'] for job in summaries] == [0, 1, 2, *range(10, 20)]
    assert summaries[0] == dict(id=0, lane='substantiveness', status='queued',
                               required=True, reviewer_requested=False)
    assert summaries[-1] == dict(id=19, lane='substantiveness', status='resolved',
        required=False, reviewer_requested=True, nodes=['N19'], targets=['T19'],
        stale_reason='source inputs changed')
    assert all(set(job) <= {'id', 'lane', 'status', 'required', 'reviewer_requested',
                           'nodes', 'targets', 'stale_reason'} for job in summaries)
    assert len(json.dumps(summaries)) < 4096
    # Queues may advance during review; its already-issued context stays frozen.
    status['verifiers']['jobs'].clear()
    driver._start_review()
    assert path.read_bytes() == captured


def test_publication_rechecks_full_dependency_hash_even_with_same_proof_prefix(tmp_path):
    from contextlib import nullcontext
    from types import SimpleNamespace
    from trellis.dispatch.candidate import canonical_json
    from trellis.dispatch.driver import acceptance_identity
    task = dict(status={'status': 'checking'}, assignment={'proof_body_rebase_inputs': {'Tablet/C.lean': 'prefix'}})
    private = {'in_flight_request': {'id': 8}}
    captured = dict(input_hashes={'Tablet/C.lean': 'old-proof'},
                    proof_body_rebase_inputs={'Tablet/C.lean': 'prefix'}, graph={})
    current = dict(captured, input_hashes={'Tablet/C.lean': 'new-proof'})
    (tmp_path / 'canonical.json').write_bytes(canonical_json(dict(
        acceptance_sha256=acceptance_identity(private), source=captured)))
    driver = ProductionDriver.__new__(ProductionDriver)
    (tmp_path / 'runtime').mkdir()
    (tmp_path / 'runtime/protocol_state.json').write_bytes(canonical_json(private))
    driver.repo = tmp_path
    driver.require_no_halt = lambda: None
    driver.state = lambda: {'dispatch': {'tasks': {'1': task}}}
    driver.publisher = SimpleNamespace(locked=nullcontext)
    driver.native = SimpleNamespace(_require_recovered_publication=lambda: None,
        observe_sources=lambda *args: current, _call=lambda payload: private)
    # Even a successfully checked candidate cannot publish across this change.
    assert driver._publish(1, 'candidate', tmp_path, {'response': {'status': 'Ok', 'outcome': 'Valid'}}) is False


@pytest.mark.parametrize('status', ['checking', 'stopping'])
def test_candidate_scope_waits_for_completed_publication(tmp_path, monkeypatch, status):
    from concurrent.futures import ThreadPoolExecutor
    from contextlib import contextmanager
    from threading import Event
    from trellis.dispatch import candidate
    from trellis.dispatch.native import NativeLedger
    from trellis.dispatch.publication import Publisher

    repo = tmp_path / 'repo'
    repo.mkdir()
    statement = repo / 'statement'
    statement.write_text('before')
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.repo = repo
    driver.root = tmp_path
    driver.native = NativeLedger(tmp_path / 'runtime')
    publisher = Publisher(tmp_path / 'runtime', repo)
    task = dict(id=1, status={'status': status, 'candidate_sha256': 'candidate'}, assignment=dict(task_kind='lean_closure',
        compiler='managed', scope=dict(nodes=['A'], helper_prefixes=[], shared_writes=[], exclusive=False)))
    observed = []
    def state():
        assert statement.read_text() == 'after', 'read a partial canonical publication'
        return {'dispatch': {'tasks': {'1': task}}}
    driver.state = state
    monkeypatch.setattr(candidate, 'read', lambda _: ({'response': {}}, 'candidate'))
    monkeypatch.setattr(candidate, 'check_scope', lambda *_args, **_kwargs: observed.append('scope'))
    monkeypatch.setattr(candidate, 'check_lean_closure', lambda *_args: observed.append(statement.read_text()))
    waiting = Event()
    locked = driver.native.locked
    @contextmanager
    def notified_lock():
        waiting.set()
        with locked():
            yield
    driver.native.locked = notified_lock
    with ThreadPoolExecutor(max_workers=1) as pool:
        with publisher.locked():
            statement.write_text('partial')
            result = pool.submit(driver._check_candidate_scope, 1, 'candidate')
            assert waiting.wait(5)
            assert not result.done() and not observed
            statement.write_text('after')
        assert result.result(timeout=5) == ((task, tmp_path / '1/candidate', None) if status == 'checking' else None)
    assert observed == (['scope', 'after'] if status == 'checking' else [])


def test_frozen_revision_snapshot_does_not_route_or_prepare_more_work(tmp_path):
    from contextlib import nullcontext
    from types import SimpleNamespace
    status = dict(stage='Start', pv_revision_ready=True, routing_pending=True,
                  tasks={}, verifiers={'jobs': {}})
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.runtime = driver.repo = tmp_path
    driver.pausing = lambda: False
    driver.native = SimpleNamespace(prepare_retries=lambda _: None, locked=nullcontext,
        _require_recovered_publication=lambda: None, _call=lambda _: status)
    driver._routing = lambda _: pytest.fail('frozen generation must not route')
    driver._prepare_verifiers = lambda _: pytest.fail('frozen generation must not prepare verifiers')
    assert driver._snapshot() is status


@pytest.mark.parametrize('archivable,frozen', [(True, False), (False, False), (True, True)])
def test_draining_archives_only_native_eligible_requests_without_starting_work(tmp_path, archivable, frozen):
    from contextlib import nullcontext
    from types import SimpleNamespace
    status = dict(stage='Reviewer', pv_revision_ready=frozen, routing_pending=True,
        reviewer_runnable=True, tasks={
            '1': dict(id=1, status={'status': 'resolved'}, request_archivable=archivable),
            '2': dict(id=2, status={'status': 'checking'}, request_archivable=False)},
        verifiers={'jobs': {'3': dict(status={'status': 'checking'})}})
    original_tasks = copy.deepcopy(status['tasks'])
    pause = tmp_path / 'pause_request.json'
    pause.write_text('{"kind":"deployment"}')
    sentinel = tmp_path / '.trellis-stop-after-checkpoint'
    sentinel.write_text('ordinary drain')
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.runtime = driver.repo = tmp_path
    calls = []
    def call(payload):
        calls.append(payload['action'])
        if payload['action'] == 'dispatch_archive_requests':
            status['tasks']['1']['request_archivable'] = False
        else:
            assert payload == dict(action='dispatch_control', root=str(tmp_path), event=None)
        return status
    driver.native = SimpleNamespace(locked=nullcontext,
        _require_recovered_publication=lambda: None, _call=call)
    driver._routing = lambda _: pytest.fail('draining must not route fresh work')
    driver._prepare_verifiers = lambda _: pytest.fail('draining must not prepare verifiers')
    result = driver._snapshot()
    assert calls == ['dispatch_control'] + (['dispatch_archive_requests'] if archivable and not frozen else [])
    assert result['tasks']['2'] == original_tasks['2']
    assert result['tasks']['1']['status'] == original_tasks['1']['status']
    assert pause.read_text() == '{"kind":"deployment"}'
    assert sentinel.read_text() == 'ordinary drain'
    if not frozen:
        assert result['pausing'] and not result['reviewer_runnable']
        assert result['verifiers_runnable'], 'sealed verifier drainage remains available'
        calls.clear()
        driver._snapshot()
        assert calls == ['dispatch_control'], 'already archived requests do not repeat maintenance'


def test_verifier_loop_returns_after_pv_revision_handoff():
    import asyncio
    async def scenario():
        driver = ProductionDriver.__new__(ProductionDriver)
        async def snapshot():
            return dict(stage='Start', pv_revision_ready=True)
        async def exclusive(_):
            pytest.fail('revision handoff must not execute StartCycle')
        driver.snapshot, driver._exclusive = snapshot, exclusive
        await driver.run_verifiers()
    asyncio.run(scenario())


@pytest.mark.parametrize('current_digest', ['old', 'new'])
def test_integration_guard_reads_one_state_and_only_current_submission(tmp_path, monkeypatch, current_digest):
    from trellis.dispatch import candidate
    from trellis.dispatch.native import NativeLedger
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.repo = tmp_path
    driver.native = NativeLedger(tmp_path)
    task = dict(id=1, status=dict(status='checking', candidate_sha256=current_digest),
        worker_attempt={'turn': 2}, assignment=dict(task_kind='lean_closure', compiler='managed',
        scope=dict(nodes=['A'], helper_prefixes=[], shared_writes=[], exclusive=False)))
    reads, candidates = [], []
    def state():
        reads.append(True)
        return {'dispatch': {'tasks': {'1': task}}}
    def read(path):
        candidates.append(path)
        return {'response': None}, current_digest
    driver.state = state
    monkeypatch.setattr(candidate, 'read', read)
    monkeypatch.setattr(candidate, 'check_scope', lambda *a, **k: None)
    result = driver._check_candidate_scope(1, 'old')
    assert len(reads) == 1
    if current_digest == 'new':
        assert result is None and candidates == [], 'old guard must not even read the repaired candidate'
    else:
        sealed = tmp_path / '1/turns/2/candidate'
        assert result == (task, sealed, None) and candidates == [sealed]


def test_delayed_scope_rejection_cannot_reject_different_repaired_candidate(tmp_path):
    from trellis.dispatch.native import NativeLedger
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.native = NativeLedger(tmp_path, kernel=lambda _: pytest.fail('stale rejection reached native'))
    driver.require_no_halt = lambda: None
    driver.state = lambda: {'dispatch': {'tasks': {'1': {'status':
        dict(status='checking', candidate_sha256='new')}}}}
    driver._reject_candidate(1, 'old', 'scope error from earlier turn')


@pytest.mark.parametrize('failure', ['read', 'digest', 'scope'])
def test_combined_candidate_guard_keeps_corruption_distinct_from_scope_rejection(tmp_path, monkeypatch, failure):
    from trellis.dispatch import candidate
    from trellis.dispatch.native import NativeLedger
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.repo = tmp_path
    driver.native = NativeLedger(tmp_path)
    task = dict(id=1, status=dict(status='checking', candidate_sha256='expected'),
        assignment=dict(task_kind='lean_closure', compiler='managed',
        scope=dict(nodes=['A'], helper_prefixes=[], shared_writes=[], exclusive=False)))
    driver.state = lambda: {'dispatch': {'tasks': {'1': task}}}
    def read(_):
        if failure == 'read':
            raise candidate.CandidateError('damaged sealed candidate')
        return {'response': None}, ('wrong' if failure == 'digest' else 'expected')
    def scope(*args, **kwargs):
        raise candidate.CandidateError('outside assigned scope')
    monkeypatch.setattr(candidate, 'read', read)
    monkeypatch.setattr(candidate, 'check_scope', scope)
    if failure == 'read':
        with pytest.raises(candidate.CandidateError, match='damaged sealed candidate'):
            driver._check_candidate_scope(1, 'expected')
    elif failure == 'digest':
        with pytest.raises(RuntimeError, match='does not match'):
            driver._check_candidate_scope(1, 'expected')
    else:
        assert driver._check_candidate_scope(1, 'expected') == (
            task, tmp_path / '1/candidate', 'outside assigned scope')



def test_snapshot_exposes_work_and_releases_native_lock_during_background_prune(tmp_path, monkeypatch):
    import fcntl
    import threading
    from types import SimpleNamespace
    from trellis.dispatch.native import NativeLedger
    from trellis.dispatch import storage
    async def scenario():
        driver = ProductionDriver.__new__(ProductionDriver)
        status = dict(stage='Reviewer', tasks={'1': dict(id=1, review_id=1, reserved_at_ms=1,
                          assignment={'assignment_id': 'assignment-1'}, status={'status': 'resolved'})},
                      verifiers={'jobs': {}}, reviewer_runnable=True)
        driver.runtime = driver.repo = tmp_path
        driver.native = NativeLedger(tmp_path, kernel=lambda _: dict(status='dispatch_ok', output=status))
        driver.root = driver.native.root
        driver.native.prepare_retries = lambda _: None
        driver.require_no_halt = lambda: None
        driver.pausing = lambda: False
        driver._prepare_verifiers = lambda _: False
        driver.measurements = SimpleNamespace(sample=lambda _: None)
        started, release = threading.Event(), threading.Event()
        calls = []
        def prune(directory, **kwargs):
            calls.append(directory)
            started.set()
            assert release.wait(5)
            return True
        monkeypatch.setattr(storage, 'prune', prune)
        try:
            assert await asyncio.wait_for(driver.snapshot(), 1) is status
            assert await asyncio.to_thread(started.wait, 1)
            # Cleanup is deliberately stuck; native publication/dispatch can
            # still acquire their lock, and another snapshot queues no job.
            with (driver.root / 'coordinator.lock').open('a+b') as stream:
                fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            assert await asyncio.wait_for(driver.snapshot(), 1) is status
            assert calls == [driver.root / '1']
        finally:
            release.set()
            await asyncio.to_thread(driver._build_cleanup._thread.join, 2)
        assert not driver._build_cleanup._thread.is_alive()
    asyncio.run(scenario())

@pytest.mark.parametrize('assigned_exclusive,admitted_exclusive', [(True, True), (False, True), (False, False)])
def test_legacy_full_check_orders_verifier_delivery_only_for_exclusive_scope(tmp_path, monkeypatch, assigned_exclusive, admitted_exclusive):
    from trellis.dispatch import staged_driver
    from types import SimpleNamespace
    async def scenario():
        driver = ProductionDriver.__new__(ProductionDriver)
        driver.finalization_gate = asyncio.Lock()
        exclusive = assigned_exclusive or admitted_exclusive
        driver.acceptance_queue = SimpleNamespace(active={2: SimpleNamespace(exclusive=admitted_exclusive)})
        task = {'assignment': {'scope': {'exclusive': assigned_exclusive}}}
        driver._check_candidate_scope = lambda *_: (task, tmp_path, None)
        async def unsupported(*_):
            return False
        monkeypatch.setattr(staged_driver, 'run', unsupported)
        observing, release, delivered = asyncio.Event(), asyncio.Event(), asyncio.Event()
        order = []
        async def legacy(*_, finalization_locked):
            assert finalization_locked is exclusive
            order.append('prepare')
            observing.set()
            await release.wait()
            # The full legacy path uses this flag to avoid a nested acquisition.
            if finalization_locked:
                assert driver.finalization_gate.locked()
                order.append('publish')
            else:
                async with driver.finalization_gate:
                    order.append('publish')
        driver._integrate_legacy = legacy
        integration = asyncio.create_task(driver._integrate_reserved(2, 'candidate'))
        await observing.wait()
        async def verifier():
            # This is the actual _verifier delivery mutex; provider completion
            # is independent and already available while checking continues.
            async with driver.finalization_gate:
                order.append('verifier')
                delivered.set()
        delivery = asyncio.create_task(verifier())
        await asyncio.sleep(0)
        assert delivered.is_set() is not exclusive
        release.set()
        await asyncio.gather(integration, delivery)
        assert order == (['prepare', 'publish', 'verifier'] if exclusive
                         else ['prepare', 'verifier', 'publish'])
    asyncio.run(scenario())


def test_exclusive_legacy_failure_releases_verifier_gate(tmp_path, monkeypatch):
    from trellis.dispatch import staged_driver
    async def scenario():
        driver = ProductionDriver.__new__(ProductionDriver)
        driver.finalization_gate = asyncio.Lock()
        from types import SimpleNamespace
        driver.acceptance_queue = SimpleNamespace(active={})
        driver._check_candidate_scope = lambda *_: ({'assignment': {'scope': {'exclusive': True}}}, tmp_path, None)
        async def unsupported(*_): return False
        monkeypatch.setattr(staged_driver, 'run', unsupported)
        async def fail(*_, **kwargs): raise RuntimeError('retained check')
        driver._integrate_legacy = fail
        with pytest.raises(RuntimeError, match='retained check'):
            await driver._integrate_reserved(2, 'candidate')
        assert not driver.finalization_gate.locked()
    asyncio.run(scenario())


@pytest.mark.parametrize('phase', ['Modeling', 'ProofFormalization'])
def test_modeling_rebased_publication_keeps_checked_new_helper_graph(tmp_path, monkeypatch, phase):
    from contextlib import nullcontext
    from types import SimpleNamespace
    from trellis.dispatch import driver as module, acceptance_receipts, candidate
    task = {'status': {'status': 'checking'},
            'assignment': {'scope': {'exclusive': True, 'nodes': ['A']}}}
    source = {'input_hashes': {'Tablet/A.lean': 'old'}, 'manifest': {},
              'graph': {'edges': {'A': []}}}
    proposed = dict(source, graph={'edges': {'A': ['NewHelper'], 'NewHelper': []}})
    original = {'response': {'status': 'Ok', 'outcome': 'Valid', 'request_id': 1, 'cycle': 4}}
    private = {'phase': phase, 'in_flight_request': {'id': 2, 'cycle': 4}}
    (tmp_path / 'runtime').mkdir()
    for name, data in [('canonical.json', {'key': {}, 'source': source, 'acceptance_sha256': 'old'}),
                       ('base-manifest.json', {}), ('runtime/protocol_state.json', private)]:
        (tmp_path / name).write_bytes(candidate.canonical_json(data))
    monkeypatch.setattr(module, 'acceptance_identity', lambda _: 'new')
    monkeypatch.setattr(acceptance_receipts, 'key', lambda *args: {'acceptance_sha256': 'new'})
    monkeypatch.setattr(acceptance_receipts, 'reusable', lambda *args: original)
    monkeypatch.setattr(candidate, 'read', lambda *args: ({'changes': {'Tablet/NewHelper.lean': {}}}, None))
    prepared, committed = [], []
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.repo = tmp_path / 'canonical'
    driver.require_no_halt = lambda: None
    driver.state = lambda: {'dispatch': {'tasks': {'2': task}}}
    driver.now_ms = lambda: 1
    driver._submission_directory = lambda _: tmp_path
    driver._rebase_acceptance = lambda *args: original  # Native guarded response, tested separately.
    driver.native = SimpleNamespace(_require_recovered_publication=lambda: None,
        _call=lambda _: private,
        observe_sources=lambda state, repo, **kw: source if repo == driver.repo else proposed,
        prepare_publication=lambda payload: prepared.append(payload) or payload,
        commit_publication=lambda *args: None, publication_receipt=lambda *args: None)
    driver.publisher = SimpleNamespace(locked=nullcontext,
        stage=lambda *args, **kwargs: committed.append(kwargs) or object(),
        matches=lambda *args, **kwargs: True, activate=lambda *args, **kwargs: None,
        discard=lambda *args: None, recover=lambda *args, **kwargs: None)
    assert driver._publish(2, 'candidate', tmp_path, original) is True
    assert prepared[0]['graph_after'] == proposed['graph']
    assert prepared[0]['actual_scope']['nodes'] == ['A', 'NewHelper']
    assert prepared[0]['response']['request_id'] == 2
    assert original['response']['request_id'] == 1
    assert committed[0]['checked_baseline'] == {}


@pytest.mark.parametrize('exclusive', [False, True])
@pytest.mark.parametrize('plan_status', ['unsupported', 'planned'])
def test_retained_legacy_completion_reaches_plan_and_publication(tmp_path, monkeypatch, exclusive, plan_status):
    from types import SimpleNamespace
    from trellis.dispatch import staged_driver, acceptance_receipts
    import json

    async def scenario():
        driver = ProductionDriver.__new__(ProductionDriver)
        driver.root = tmp_path
        driver.acceptance_queue = SimpleNamespace(active={})
        driver.finalization_gate = asyncio.Lock()
        task = {'assignment': {'scope': {'exclusive': exclusive}}}
        driver._check_candidate_scope = lambda *_: (task, tmp_path / 'sealed', None)
        directory = tmp_path / '2/acceptance/1'
        execution = directory / 'execution'
        execution.mkdir(parents=True)
        (directory / 'canonical.json').write_text(json.dumps({'source': {'input_hashes': {'A': 'hash'}}}))
        (directory / 'prepared.json').write_text('{}')
        (execution / 'permit.json').write_text('{}')
        original = {'ok': True, 'response': {'outcome': 'Valid'}}
        calls = []
        process = SimpleNamespace(root=execution, stop_and_reap=lambda _: calls.append('reap'))
        driver._process = lambda *args, **kwargs: (process, {'pid': 1})
        async def retained(*args, **kwargs):
            calls.append('retained')
            return original
        driver._wait = retained
        monkeypatch.setattr(acceptance_receipts, 'complete', lambda d, r: calls.append('complete'))
        def plan(*args):
            calls.append('plan')
            assert driver.finalization_gate.locked() is exclusive
            return {'status': plan_status}
        monkeypatch.setattr(staged_driver, 'plan', plan)
        def publish(*args):
            calls.append('publish')
            assert driver.finalization_gate.locked()
            assert args[-1] is original
            return True
        driver._publish = publish
        result = await asyncio.wait_for(driver._integrate_reserved(2, 'candidate', legacy_adoption=True), 2)
        assert result == ('readmit' if plan_status == 'planned' else None)
        assert calls == ['retained', 'reap', 'complete', 'plan'] + ([] if plan_status == 'planned' else ['publish'])
        assert not driver.finalization_gate.locked()
    asyncio.run(scenario())


@pytest.mark.parametrize('submission', [False, True])
@pytest.mark.parametrize('permitted', [False, True])
def test_worker_handoff_runs_on_main_loop_after_real_permit_or_exact_adoption(tmp_path, submission, permitted):
    import threading
    from types import SimpleNamespace
    async def scenario():
        driver = ProductionDriver.__new__(ProductionDriver)
        driver.root = tmp_path
        directory = tmp_path / '1'; directory.mkdir()
        (directory / 'prepared.json').write_text('{}')
        execution = directory / ('turns/1/execution' if submission else 'execution')
        execution.mkdir(parents=True)
        if permitted:
            (execution / 'permit.json').write_text('{}')
        record = {'pid': 123, 'start_ticks': 4, 'boot_id': 'test'}
        task = dict(id=1, status={'status': 'running'}, process=record,
                    assignment={'compiler': 'managed'}, binding={'timeout_seconds': 10})
        if submission:
            task['worker_attempt'] = dict(turn=1, allowance_ms=1000, consumed_ms=0,
                                          phase={'phase': 'ready'})
        driver._process = lambda *a, **kw: (SimpleNamespace(root=execution), record)
        entered, waiting = asyncio.Event(), asyncio.Event()
        release = threading.Event()
        main_thread = threading.get_ident()
        loop = asyncio.get_running_loop()
        events = []
        def permit(*_):
            assert not permitted
            loop.call_soon_threadsafe(entered.set)
            assert release.wait(3)
            (execution / 'permit.json').write_text('{}')
        driver._permit = permit
        def handed_off(task_id, kind):
            assert threading.get_ident() == main_thread
            assert (execution / 'permit.json').exists()
            events.append((task_id, kind))
        driver.set_handoff_notifier(handed_off)
        async def wait(*_, **kw):
            assert events == [(1, 'worker')]
            waiting.set()
            await asyncio.Future()
        driver._wait = wait
        running = asyncio.create_task(driver.launch_or_adopt(task))
        try:
            if not permitted:
                await asyncio.wait_for(entered.wait(), 2)
                assert events == []
                release.set()
            await asyncio.wait_for(waiting.wait(), 2)
            assert events == [(1, 'worker')]
        finally:
            release.set()
            running.cancel()
            await asyncio.gather(running, return_exceptions=True)
    asyncio.run(scenario())


def test_retry_preparation_rechecks_pause_and_system_feedback_inside_native_lock(tmp_path):
    from types import SimpleNamespace
    driver = ProductionDriver.__new__(ProductionDriver); driver.repo = tmp_path
    paused = [False]
    driver.pausing = lambda: paused[0]
    events = []
    def no_halt():
        events.append('halt checked')
    driver.require_no_halt = no_halt
    def prepare(repo, *, proceed):
        assert repo == tmp_path
        assert proceed()
        paused[0] = True  # Control arrived during preparation.
        assert not proceed()
        return []
    driver.native = SimpleNamespace(prepare_retries=prepare)
    assert not asyncio.run(driver.prepare_one_retry())
    assert events == ['halt checked', 'halt checked']


def test_checking_handoff_registers_real_queue_waiter_without_waiting_for_conflict(tmp_path):
    from trellis.dispatch.acceptance_queue import AcceptanceQueue, Footprint
    async def scenario():
        driver = ProductionDriver.__new__(ProductionDriver)
        driver.root = tmp_path
        driver.now_ms = lambda: 1000
        driver.acceptance_queue = AcceptanceQueue()
        footprint = Footprint(writes=frozenset({'A'}))
        driver.acceptance_queue.restore({9: footprint})
        driver._acceptance_footprint = lambda *_: footprint
        notified = asyncio.Event()
        driver.set_handoff_notifier(lambda task_id, kind: notified.set())
        running = asyncio.create_task(driver.integrate(1, 'candidate'))
        try:
            await asyncio.wait_for(notified.wait(), 2)
            assert driver.acceptance_queue.pending == [(1, footprint)]
            assert driver.acceptance_queue.active == {9: footprint}
            assert not running.done()
            assert not (tmp_path / '1/acceptance-admission.json').exists()
        finally:
            running.cancel()
            await asyncio.gather(running, return_exceptions=True)
        assert driver.acceptance_queue.pending == []
        assert driver.acceptance_queue.active == {9: footprint}
    asyncio.run(scenario())


@pytest.mark.parametrize('origin', ['matching', 'missing', 'changed', 'bad_id'])
def test_exclusive_audit_uses_native_origin_before_stripping_dispatch(tmp_path, origin):
    import json
    from contextlib import nullcontext
    from types import SimpleNamespace
    from trellis.dispatch.candidate import digest
    comments = 'Read .trellis/stuck-math-audit/probe.lean — "quoted"\nnotes'
    handoff = dict(review_id=3282, comments_sha256=digest(
        json.dumps(comments, ensure_ascii=False, separators=(',', ':')).encode()))
    if origin == 'changed':
        handoff['comments_sha256'] = 'wrong'
    if origin == 'bad_id':
        handoff['review_id'] = '../other'
    request = dict(id=3288, kind='StuckMathAudit', reviewer_comments=comments)
    state = dict(in_flight_request=request, dispatch=dict(
        worker_handoff=None if origin == 'missing' else handoff))
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.repo = tmp_path
    driver.state = lambda: copy.deepcopy(state)
    driver.native = SimpleNamespace(locked=nullcontext, _call=lambda action: {})
    captured = []
    def prepare(directory, private, **kwargs):
        assert private['dispatch'] is None
        directory.mkdir()
        (directory / 'worker-handoffs.json').write_text('[]')
        captured.append(kwargs)
    driver._prepare = prepare
    if origin in ('changed', 'bad_id'):
        with pytest.raises(RuntimeError, match='native reviewer origin'):
            driver._prepare_exclusive(dict(request=request, tasks={}))
        assert not captured
    else:
        driver._prepare_exclusive(dict(request=request, tasks={}))
        assert captured == [dict(handoff_repo=tmp_path / 'review-3282/repo'
                                if origin == 'matching' else None)]
    assert state['dispatch'] is not None


@pytest.mark.parametrize('case', ['stale', 'current', 'prefix', 'corrupt', 'replaced', 'halt'])
def test_scope_rejection_stale_dependency_is_native_disposition_not_global_stop(tmp_path, monkeypatch, case):
    from types import SimpleNamespace
    from trellis.dispatch import candidate
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = tmp_path / 'dispatch'; driver.repo = tmp_path / 'repo'; driver.runtime = tmp_path
    (driver.repo / 'Tablet').mkdir(parents=True)
    data = b'theorem B : True := by\n-- BODY\n  trivial\n'
    (driver.repo / 'Tablet/B.lean').write_bytes(data)
    actual = candidate.digest(data)
    record = dict(task_id=1, inputs={'Tablet/B.lean': actual if case == 'current' else 'old'}, changes={})
    task = dict(id=1, status=dict(status='checking', candidate_sha256='new' if case == 'replaced' else 'sealed'),
                assignment={'proof_body_rebase_inputs': {}}, worker_attempt={'turn': 1})
    if case == 'prefix':
        from trellis.dispatch.acceptance import proof_prefix_digest
        task['assignment']['proof_body_rebase_inputs']['Tablet/B.lean'] = proof_prefix_digest(data)
    source = dict(input_hashes={'Tablet/B.lean': actual}, graph={})
    monkeypatch.setattr(candidate, 'read', lambda _: (record, 'corrupt' if case == 'corrupt' else 'sealed'))
    calls = []
    driver.native = SimpleNamespace(_call=lambda event: calls.append(event))
    def no_halt():
        if case == 'halt': raise RuntimeError('halt remains authoritative')
    driver.require_no_halt = no_halt
    if case in ('corrupt', 'replaced', 'halt'):
        with pytest.raises(RuntimeError): driver._stop_stale_negative(task, 'sealed', source, 'scope feedback')
        assert not calls
    else:
        assert driver._stop_stale_negative(task, 'sealed', source, 'scope feedback') is (case == 'stale')
        if case == 'stale':
            assert len(calls) == 1
            assert calls[0]['event'] == dict(kind='attempt_stopping', task_id=1, outcome=dict(kind='stale',
                reason='stale task input: Tablet/B.lean; retained candidate feedback: scope feedback'))
        else: assert not calls


def test_scope_rejection_routes_stale_before_native_failure_replay(tmp_path, monkeypatch):
    from types import SimpleNamespace
    driver = ProductionDriver.__new__(ProductionDriver)
    source = {'input_hashes': {}}
    driver.native = SimpleNamespace(observe_sources=lambda *_: source,
        _call=lambda _: pytest.fail('stale rejection must not attempt a current Invalid verdict'))
    driver.repo = tmp_path
    seen = []
    driver._stop_stale_negative = lambda *args: seen.append(args) or True
    driver._reject_candidate_locked({}, {'id': 1}, 'sealed', 'scope feedback')
    assert seen == [({'id': 1}, 'sealed', source, 'scope feedback')]


@pytest.mark.parametrize('outcome,stale', [('Stuck', True), ('Invalid', True), ('Invalid', False), ('Valid', True)])
def test_checked_negative_stale_disposition_preserves_positive_publication(tmp_path, monkeypatch, outcome, stale):
    from contextlib import nullcontext
    from types import SimpleNamespace
    from trellis.dispatch import driver as module, acceptance_receipts, candidate
    task = dict(id=1, status=dict(status='checking', candidate_sha256='sealed'),
                assignment=dict(scope=dict(nodes=['A'], exclusive=False)))
    source = dict(input_hashes={}, graph={'edges': {'A': []}}, manifest={})
    private = dict(phase='ProofFormalization', in_flight_request=dict(id=2, cycle=3))
    captured = dict(acceptance_sha256='context', source=source)
    (tmp_path / 'canonical.json').write_bytes(candidate.canonical_json(captured))
    monkeypatch.setattr(module, 'acceptance_identity', lambda _: 'context')
    monkeypatch.setattr(acceptance_receipts, 'key', lambda *args: {})
    monkeypatch.setattr(candidate, 'read', lambda _: ({'changes': {}}, 'sealed'))
    calls = []
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.repo = tmp_path; driver.runtime = tmp_path
    driver.require_no_halt = lambda: None
    driver.state = lambda: {'dispatch': {'tasks': {'1': task}}}
    driver.now_ms = lambda: 0
    driver._submission_directory = lambda _: tmp_path
    driver._stop_stale_negative = lambda *args: calls.append('stale-check') or stale
    driver.native = SimpleNamespace(_require_recovered_publication=lambda: None, _call=lambda _: private,
        observe_sources=lambda *args, **kwargs: source,
        prepare_publication=lambda payload: calls.append('prepare') or payload,
        commit_failure=lambda payload: calls.append('failure'))
    driver.publisher = SimpleNamespace(locked=nullcontext)
    driver._publication_basis = lambda: {}
    result = driver._publish_locked(1, 'sealed', tmp_path,
        {'response': {'status': 'Ok', 'outcome': outcome}})
    if outcome == 'Valid':
        assert calls == ['prepare']
        assert result['kernel_event']['response']['outcome'] == 'Valid'
    elif stale:
        assert result is True and calls == ['stale-check']
    else:
        assert result is True and calls == ['stale-check', 'prepare', 'failure']


def test_stale_negative_authenticates_real_sealed_candidate_and_preserves_it(tmp_path):
    from types import SimpleNamespace
    from trellis.dispatch import candidate
    worker, repo, sealed = (tmp_path / name for name in ('worker', 'repo', 'sealed'))
    for root in (worker, repo):
        (root / 'Tablet').mkdir(parents=True)
        (root / 'Tablet/A.lean').write_text('-- BODY\nold proof')
        (root / 'Tablet/B.lean').write_text('-- BODY\nold dependency')
    baseline = candidate.source_manifest(worker)
    (worker / 'Tablet/A.lean').write_text('set_option maxHeartbeats 1000000\n-- BODY\nnew proof')
    identity = candidate.seal(worker, sealed, task_id=1, base_manifest=baseline,
        inputs=baseline, response={'outcome': 'valid'})
    pins = {p.relative_to(sealed).as_posix(): p.read_bytes() for p in sealed.rglob('*') if p.is_file()}
    (repo / 'Tablet/B.lean').write_text('-- BODY\nnew accepted dependency')
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.repo = repo; driver.runtime = tmp_path
    driver.require_no_halt = lambda: None
    driver._submission_directory = lambda _: tmp_path
    sealed.rename(tmp_path / 'candidate')
    calls = []
    driver.native = SimpleNamespace(_call=lambda event: calls.append(event))
    task = dict(id=1, status=dict(status='checking', candidate_sha256=identity), assignment={})
    assert driver._stop_stale_negative(task, identity,
        {'input_hashes': candidate.source_manifest(repo)}, 'closure prefix changed')
    assert calls[0]['event']['outcome']['kind'] == 'stale'
    assert pins == {p.relative_to(tmp_path / 'candidate').as_posix(): p.read_bytes()
                    for p in (tmp_path / 'candidate').rglob('*') if p.is_file()}
