"""Exercise the parallel consumer with real wait evidence and an owned-process stub."""
import asyncio
import copy
from types import SimpleNamespace

import pytest

from trellis.dispatch import driver as module, worker_transport as transport
from trellis.dispatch.candidate import canonical_json, digest
from trellis.dispatch.preparation_capacity import PreparationDeferred
from test_worker_transport import stopped, paused_attempt, put


def consumer(stopped, monkeypatch):
    execution, task, response = stopped
    driver = module.ProductionDriver.__new__(module.ProductionDriver)
    driver.root = execution.parents[3]
    driver.runtime = driver.root / 'coordinator'
    driver.runtime.mkdir()
    driver._hydrate_worker_cache = lambda _: None
    driver._handoff = lambda *args: None
    driver.pause_requested = lambda: False
    events = []
    async def observe(event):
        events.append(event)
    driver.observe = observe
    # Scheduling-thread mechanics are outside this consumer fixture; the
    # production wait, receipt validation and permit predicates remain real.
    async def inline(function, *args, **kwargs):
        return function(*args, **kwargs)
    monkeypatch.setattr(module, '_run_blocking', inline)
    return driver, events


@pytest.mark.parametrize('stopped', ['codex'], indirect=True)
@pytest.mark.parametrize('release', ['deadline', 'manual'])
def test_parallel_wait_consumes_exact_deadline_or_permit_once(stopped, monkeypatch, release):
    original, original_task, original_response = stopped
    submission, attempt, wrapper_id = paused_attempt(stopped)
    attempt['session'] = attempt['last_interruption']['session']
    driver, events = consumer(stopped, monkeypatch)
    status = transport.wait_status(submission, attempt, wrapper_id)
    now = [status['next_retry_at_ms'] - 1]
    driver.now_ms = lambda: now[0]
    task = dict(original_task, process=None, status={'status': 'reserved'},
                assignment={'compiler': 'managed'}, worker_attempt=attempt)
    execution = transport.execution_directory(submission, 1)
    calls = []
    record = {'pid': 102, 'start_ticks': 12, 'boot_id': 'boot'}
    timing = {'disposition': 'expired', 'elapsed_ms': status['remaining_ms']}
    def launch(directory, **kwargs):
        calls.append(kwargs)
        assert directory == driver.root / str(task['id'])
        assert kwargs['execution'] == execution
        assert kwargs['worker_turn']['remaining_ms'] == status['remaining_ms']
        assert kwargs['worker_turn']['session'] == attempt['session']
        config = {'allowance_ms': status['remaining_ms']}
        put(execution / 'clock/config.json', config)
        common = {'config_sha256': digest(canonical_json(config)), 'owner': record}
        put(execution / 'clock/started.json', dict(common, started_at_ms=100))
        timing.update(common)
        put(execution / 'clock/finished.json', timing)
        return SimpleNamespace(root=execution, permit=lambda value: put(execution / 'permit.json', value),
                               stop_and_reap=lambda _: None), record
    driver._process = launch
    async def wait(process, identity, *, poll):
        await poll()
        return {'kind': 'worker_submission', 'turn': 1, 'provider_execution': timing}
    driver._wait = wait
    sleeps = []
    async def advance(seconds):
        sleeps.append(seconds)
        assert not calls
        if release == 'manual':
            transport.resume(submission, attempt, wrapper_id)
        else:
            now[0] = status['next_retry_at_ms']
    monkeypatch.setattr(module.asyncio, 'sleep', advance)
    result = asyncio.run(driver._launch_submission(task))
    assert sleeps == [.2] and len(calls) == 1
    assert [event['kind'] for event in events] == ['launched', 'provider_started', 'worker_expired']
    assert all(event['task_id'] == task['id'] != wrapper_id for event in events)
    assert result.candidate_sha256 is None
    assert not (submission / 'candidate').exists()
    assert (original / 'transport-wait.json').is_file()


@pytest.mark.parametrize('stopped', ['codex'], indirect=True)
def test_parallel_pause_during_wait_creates_no_launcher(stopped, monkeypatch):
    submission, attempt, wrapper_id = paused_attempt(stopped)
    driver, events = consumer(stopped, monkeypatch)
    task = dict(stopped[1], process=None, status={'status': 'reserved'},
                assignment={'compiler': 'managed'}, worker_attempt=attempt)
    driver.now_ms = lambda: 0
    driver.pause_requested = lambda: True
    driver._process = lambda *args, **kwargs: pytest.fail('Paused retry launched')
    with pytest.raises(PreparationDeferred, match='paused'):
        asyncio.run(driver._launch_submission(task))
    assert not events
    assert not transport.execution_directory(submission, 1).exists()
    assert not (submission / 'execution/transport-resume.json').exists()


@pytest.mark.parametrize('stopped', ['codex'], indirect=True)
def test_parallel_adopts_running_transport_without_a_second_wait(stopped, monkeypatch):
    execution, original_task, response = stopped
    driver, events = consumer(stopped, monkeypatch)
    task = copy.deepcopy(original_task)
    task.update(status={'status': 'running'}, assignment={'compiler': 'managed'})
    task['worker_attempt']['phase'] = {'phase': 'running'}
    driver.now_ms = lambda: 0
    calls = []
    def adopt(*args, **kwargs):
        calls.append(kwargs)
        return SimpleNamespace(root=execution, stop_and_reap=lambda _: None), task['process']
    driver._process = adopt
    monkeypatch.setattr(transport, 'prepare_wait', lambda *args: pytest.fail('Running adoption waited'))
    async def wait(process, record, *, poll):
        await poll()
        return response
    driver._wait = wait
    result = asyncio.run(driver._launch_submission(task))
    assert len(calls) == 1 and calls[0]['execution'] == execution
    assert [event['kind'] for event in events] == ['worker_interrupted']
    assert result.candidate_sha256 is None
