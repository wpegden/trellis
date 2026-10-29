"""Startup handoff fairness without weakening native permit ordering."""
import asyncio
import threading
from types import SimpleNamespace

import pytest

from trellis.dispatch import candidate
from trellis.dispatch.driver import ProductionDriver
from trellis.dispatch.publication import atomic


def put(path, value):
    atomic(path, candidate.canonical_json(value))


def fixture(tmp_path, *, submission=False, managed=False, resumed=False):
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = tmp_path
    driver.now_ms = lambda: 42
    events = []
    waiting = asyncio.Event()
    entered = {n: asyncio.Event() for n in (1, 2)}
    tasks = []
    for n in (1, 2):
        record = dict(pid=n, start_ticks=n, boot_id='fixture')
        task = dict(id=n, status=dict(status='running' if resumed else 'reserved'),
                    process=record if resumed else None, assignment=dict(compiler='managed'),
                    binding=dict(timeout_seconds=100), execution_capacity_managed=managed)
        if submission:
            task['worker_attempt'] = dict(turn=1, allowance_ms=1000, consumed_ms=0,
                session=None, phase=dict(phase='ready'))
        directory = tmp_path / str(n)
        execution = directory / ('turns/1/execution' if submission else 'execution')
        put(directory / 'prepared.json', {})
        if resumed:
            put(execution / 'permit.json', record)
            if submission:
                put(execution / 'recipe.json', dict(payload=dict(worker_turn=dict(
                    turn=1, remaining_ms=1000, session=None, execution_record=str(execution/'clock')))))
        tasks.append(task)
    def hydrate(task): events.append(('hydrate', task['id']))
    def start(directory, *, execution=None, **kwargs):
        n=int(directory.name)
        events.append(('launcher', n))
        return SimpleNamespace(root=execution or directory/'execution'), dict(pid=n,start_ticks=n,boot_id='fixture')
    def managed_start(task, **kwargs):
        result=start(tmp_path/str(task['id']),**kwargs)
        if task['status']['status']=='reserved': events.append(('launched',task['id']))
        return result
    async def observe(event): events.append((event['kind'],event['task_id']))
    def permit(process, record):
        n=record['pid']
        assert ('launched',n) in events
        put(process.root/'permit.json',record)
        events.append(('permit',n))
    async def wait(process, record, **kwargs):
        events.append(('provider_wait',record['pid']))
        entered[record['pid']].set()
        await waiting.wait()
        pytest.fail('test provider should remain running')
    driver._hydrate_worker_cache=hydrate
    driver._process=start
    driver._start_pipelined_worker=managed_start
    driver.observe=observe
    driver._permit=permit
    driver._wait=wait
    return driver,tasks,events,entered


@pytest.mark.parametrize('submission', [False, True])
@pytest.mark.parametrize('managed,resumed', [(False,False),(True,False),(False,True),(True,True)])
def test_first_permit_precedes_next_hydration_but_provider_wait_is_concurrent(tmp_path, submission, managed, resumed):
    async def scenario():
        driver,tasks,events,entered=fixture(tmp_path,submission=submission,managed=managed,resumed=resumed)
        runners=[asyncio.create_task(driver.launch_or_adopt(task)) for task in tasks]
        try:
            await asyncio.wait_for(entered[2].wait(),5)
            assert entered[1].is_set() and not runners[0].done()
            boundary=('provider_wait',1) if resumed else ('permit',1)
            assert events.index(boundary)<events.index(('hydrate',2))
            if resumed: assert not any(e[0] in ('permit','launched') for e in events)
        finally:
            for runner in runners:runner.cancel()
            await asyncio.gather(*runners,return_exceptions=True)
    asyncio.run(scenario())


def test_cancelled_owned_hydration_finishes_before_gate_released(tmp_path):
    async def scenario():
        driver,tasks,events,entered=fixture(tmp_path,submission=True)
        began=threading.Event();release=threading.Event()
        original=driver._hydrate_worker_cache
        def hydrate(task):
            original(task)
            if task['id']==1:
                began.set()
                assert release.wait(5)
                events.append(('hydration_finished',1))
        driver._hydrate_worker_cache=hydrate
        first=asyncio.create_task(driver.launch_or_adopt(tasks[0]))
        second=None
        try:
            assert await asyncio.to_thread(began.wait,5)
            second=asyncio.create_task(driver.launch_or_adopt(tasks[1]))
            first.cancel()
            await asyncio.sleep(.03)
            assert ('hydrate',2) not in events and not first.done()
            release.set()
            with pytest.raises(asyncio.CancelledError):await first
            await asyncio.wait_for(entered[2].wait(),5)
            assert events.index(('hydration_finished',1))<events.index(('hydrate',2))
            assert ('permit',1) not in events
        finally:
            release.set()
            for runner in (first,second):
                if runner is not None:runner.cancel()
            await asyncio.gather(*(r for r in (first,second) if r is not None),return_exceptions=True)
    asyncio.run(scenario())


def test_native_launch_failure_releases_gate_without_granting_failed_worker(tmp_path):
    async def scenario():
        driver,tasks,events,entered=fixture(tmp_path)
        observe=driver.observe
        async def failing(event):
            if event['task_id']==1:raise RuntimeError('native launch refused')
            await observe(event)
        driver.observe=failing
        first=asyncio.create_task(driver.launch_or_adopt(tasks[0]))
        second=asyncio.create_task(driver.launch_or_adopt(tasks[1]))
        try:
            with pytest.raises(RuntimeError,match='native launch refused'):await first
            await asyncio.wait_for(entered[2].wait(),5)
            assert ('permit',1) not in events and ('permit',2) in events
        finally:
            second.cancel();await asyncio.gather(second,return_exceptions=True)
    asyncio.run(scenario())


def test_cancel_while_queued_never_starts_hydration(tmp_path):
    async def scenario():
        driver,tasks,events,entered=fixture(tmp_path)
        async with driver._worker_bootstrap():
            queued=asyncio.create_task(driver.launch_or_adopt(tasks[0]))
            await asyncio.sleep(0)
            queued.cancel()
            with pytest.raises(asyncio.CancelledError):await queued
            assert events==[]
        runner=asyncio.create_task(driver.launch_or_adopt(tasks[1]))
        try:await asyncio.wait_for(entered[2].wait(),5)
        finally:
            runner.cancel();await asyncio.gather(runner,return_exceptions=True)
    asyncio.run(scenario())


@pytest.mark.parametrize('submission',[False,True])
def test_cancel_during_native_launch_joins_commit_before_next_hydration(tmp_path,submission):
    async def scenario():
        driver,tasks,events,entered=fixture(tmp_path,submission=submission,managed=True)
        began=threading.Event();release=threading.Event()
        original=driver._start_pipelined_worker
        def start(task,**kwargs):
            if task['id']==1:
                began.set()
                assert release.wait(5)
            return original(task,**kwargs)
        driver._start_pipelined_worker=start
        first=asyncio.create_task(driver.launch_or_adopt(tasks[0]));second=None
        try:
            assert await asyncio.to_thread(began.wait,5)
            second=asyncio.create_task(driver.launch_or_adopt(tasks[1]))
            first.cancel();await asyncio.sleep(.03)
            assert not first.done() and ('hydrate',2) not in events
            release.set()
            with pytest.raises(asyncio.CancelledError):await first
            await asyncio.wait_for(entered[2].wait(),5)
            assert events.index(('launched',1))<events.index(('hydrate',2))
            assert ('permit',1) not in events  # Committed launcher is recoverable.
        finally:
            release.set()
            for runner in (first,second):
                if runner is not None:runner.cancel()
            await asyncio.gather(*(r for r in (first,second) if r is not None),return_exceptions=True)
    asyncio.run(scenario())
