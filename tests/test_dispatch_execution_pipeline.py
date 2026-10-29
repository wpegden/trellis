"""Provider capacity is acquired before permission, including queued repairs."""
import asyncio
import copy
import threading
from concurrent.futures import ThreadPoolExecutor
from types import SimpleNamespace

import pytest

from trellis.dispatch.coordination import Coordinator
from trellis.dispatch.driver import ProductionDriver
from trellis.dispatch.preparation_capacity import PreparationDeferred


def test_two_repairs_competing_for_last_slot_do_not_create_losing_launcher(tmp_path):
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.runtime = tmp_path
    lock, capacity, starts, commits = threading.Lock(), [1], [], []
    tasks = {str(i): dict(id=i, status={'status': 'reserved'}, worker_attempt={'turn': 2},
                         execution_capacity_managed=True) for i in (1, 2)}
    def native_call(payload):
        assert lock.locked()
        event = payload.get('event')
        if event is None:
            return {'tasks': {k: dict(copy.deepcopy(v), execution_runnable=capacity[0] > 0)
                              for k, v in tasks.items()}}
        assert event['kind'] == 'launched' and capacity[0] == 1
        capacity[0] -= 1
        commits.append(event)
        tasks[str(event['task_id'])]['status'] = {'status': 'running'}
        return {}
    driver.native = SimpleNamespace(locked=lambda: lock, _require_recovered_publication=lambda: None,
                                   _call=native_call)
    def process(directory, **kwargs):
        assert lock.locked() and capacity[0] == 1
        starts.append(directory.name)
        return object(), {'pid': int(directory.name)}
    driver._process = process
    driver.now_ms = lambda: 1000
    original = copy.deepcopy(tasks)
    def launch(task):
        try:
            driver._start_pipelined_worker(task, compiler='managed', seconds=None)
            return 'owned'
        except PreparationDeferred:
            return 'queued'
    with ThreadPoolExecutor(2) as executor:
        outcomes = list(executor.map(launch, original.values()))
    assert sorted(outcomes) == ['owned', 'queued']
    assert len(starts) == len(commits) == 1
    loser = next(t for t in original.values() if str(t['id']) not in starts)
    # Slot release retries the SAME turn and original reservation.
    capacity[0] = 1
    assert launch(loser) == 'owned'
    assert len(starts) == len(commits) == 2


def test_changed_turn_is_deferred_before_creating_process(tmp_path):
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.runtime = tmp_path
    task = dict(id=1, status={'status': 'reserved'}, worker_attempt={'turn': 1})
    current = dict(task, worker_attempt={'turn': 2}, execution_runnable=True)
    driver.native = SimpleNamespace(locked=threading.Lock, _require_recovered_publication=lambda: None,
        _call=lambda _: {'tasks': {'1': current}})
    driver._process = lambda *a, **kw: pytest.fail('stale turn created a launcher')
    with pytest.raises(PreparationDeferred, match='scheduling view changed'):
        driver._start_pipelined_worker(task)


def test_coordinator_keeps_capacity_deferred_repair_reserved_without_fatal_error():
    async def scenario():
        driver = SimpleNamespace()
        async def launch(task):
            raise PreparationDeferred('worker execution capacity is occupied')
        driver.launch_or_adopt = launch
        coordinator = Coordinator(driver)
        result = await coordinator._execute({'id': 1})
        assert result == 'preparation_deferred'
        assert coordinator.fatal is None
    asyncio.run(scenario())


@pytest.mark.parametrize('failure', [None, 'busy', 'missing_explicit_config'])
def test_pipeline_configuration_records_cas_and_releases_transient_token(tmp_path, monkeypatch, failure):
    import json
    from contextlib import nullcontext
    from trellis.dispatch.__main__ import configure_pipeline
    from trellis.runtime import bridge
    events, released = [], []
    config = tmp_path / 'config.json'
    config.write_text(json.dumps({'parallel_dispatch': {} if failure == 'missing_explicit_config'
                                 else {'max_in_flight_tasks': 8}}))
    def call(payload):
        if payload['event'] is None:
            return {'config': {'max_in_flight_tasks': None}}
        if failure == 'busy':
            raise RuntimeError('execution pipeline changes require drained workers')
        events.append(payload['event'])
        return {}
    driver = SimpleNamespace(runtime=tmp_path, config_path=config,
        native=SimpleNamespace(own_coordinator=nullcontext, locked=nullcontext,
                               _require_recovered_publication=lambda: None, _call=call))
    monkeypatch.setattr(bridge, '_unregister_burst_token', lambda runtime, *, token: released.append((runtime, token)))
    if failure:
        with pytest.raises(RuntimeError):
            configure_pipeline(driver, {'max_in_flight_tasks': 8}, 'transient')
        assert not events
    else:
        assert configure_pipeline(driver, {'max_in_flight_tasks': 8}, 'transient') == 0
        assert events == [{'kind': 'execution_pipeline_changed',
                           'expected_max_in_flight_tasks': None, 'max_in_flight_tasks': 8}]
    assert released == [(tmp_path, 'transient')]
