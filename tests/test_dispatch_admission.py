import asyncio
import json
from contextlib import nullcontext
from types import SimpleNamespace

from trellis.dispatch.acceptance_queue import AcceptanceQueue, Footprint
from trellis.dispatch.driver import ProductionDriver


def driver_at(root):
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = root
    driver.acceptance_queue = AcceptanceQueue()
    return driver


def test_driver_admits_independent_checks_and_waits_for_published_dependency(tmp_path):
    async def scenario():
        driver = driver_at(tmp_path)
        scopes = {1: Footprint(frozenset(['A']), frozenset(['A'])),
                  2: Footprint(frozenset(['A', 'B']), frozenset(['B'])),
                  3: Footprint(frozenset(['C']), frozenset(['C']))}
        driver._acceptance_footprint = lambda task, _: scopes[task]
        entered, release, independent = (asyncio.Event() for _ in range(3))
        canonical = {'A': 'old'}
        reads = []
        async def check(task, _):
            if task == 1:
                entered.set()
                await release.wait()
                canonical['A'] = 'published'
            elif task == 2:
                reads.append(canonical['A'])
            else:
                assert 1 in driver.acceptance_queue.active
                independent.set()
        driver._integrate_reserved = check
        first = asyncio.create_task(driver.integrate(1, 'first'))
        await entered.wait()
        second = asyncio.create_task(driver.integrate(2, 'dependent'))
        third = asyncio.create_task(driver.integrate(3, 'independent'))
        await independent.wait()
        assert not reads
        release.set()
        await asyncio.gather(first, second, third)
        assert reads == ['published']
        timings = [json.loads((tmp_path / str(n) / 'acceptance-admission.json').read_text()) for n in (1, 2, 3)]
        assert all(t['finished_at_ms'] >= t['admitted_at_ms'] >= t['queued_at_ms'] for t in timings)
        assert timings[1]['admitted_at_ms'] >= timings[0]['finished_at_ms']
    asyncio.run(scenario())


def test_restart_restores_all_surviving_reservations_before_admission(tmp_path):
    driver = driver_at(tmp_path)
    tasks = {str(n): dict(id=n, status=dict(status='checking', candidate_sha256=str(n))) for n in (1, 2, 3)}
    driver.state = lambda: dict(dispatch=dict(tasks=tasks))
    driver.native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=lambda: None)
    for n in (1, 2):
        path = tmp_path / str(n) / 'acceptance-admission.json'
        path.parent.mkdir()
        path.write_text(json.dumps(dict(version=1, candidate_sha256=str(n),
            footprint=dict(reads=[str(n)], writes=[str(n)], exclusive=False))))
    legacy = tmp_path / '3/acceptance/1/canonical.json'
    legacy.parent.mkdir(parents=True)
    legacy.write_text('{}')
    driver.acceptance_queue.restore(driver._restore_acceptance_reservations())
    assert set(driver.acceptance_queue.active) == {1, 2, 3}
    assert driver.acceptance_queue.active[3].exclusive
    async def scenario():
        entered = asyncio.Event()
        async def fresh():
            async with driver.acceptance_queue.reserve(4, Footprint(frozenset(['4']), frozenset(['4']))):
                entered.set()
        work = asyncio.create_task(fresh())
        await asyncio.sleep(0)
        assert not entered.is_set()
        async with driver.acceptance_queue.reserve(3, Footprint(exclusive=True), adopt=True):
            assert not entered.is_set()
        await work
        assert entered.is_set()
        assert set(driver.acceptance_queue.active) == {1, 2}
    asyncio.run(scenario())


def test_interrupted_check_keeps_durable_reservation_for_adoption(tmp_path):
    async def scenario():
        driver = driver_at(tmp_path)
        scope = Footprint(frozenset(['A']), frozenset(['A']))
        driver._acceptance_footprint = lambda *_: scope
        started = asyncio.Event()
        async def check(*_):
            started.set()
            await asyncio.Event().wait()
        driver._integrate_reserved = check
        job = asyncio.create_task(driver.integrate(1, 'sealed'))
        await started.wait()
        job.cancel()
        await asyncio.gather(job, return_exceptions=True)
        record = json.loads((tmp_path / '1/acceptance-admission.json').read_text())
        assert 'finished_at_ms' not in record
        driver.state = lambda: dict(dispatch=dict(tasks={'1': dict(id=1,
            status=dict(status='checking', candidate_sha256='sealed'))}))
        driver.native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=lambda: None)
        assert driver._restore_acceptance_reservations() == {1: scope}
    asyncio.run(scenario())
