import asyncio

import pytest

from trellis.dispatch.acceptance_queue import AcceptanceQueue, Footprint


def scope(reads=(), writes=(), exclusive=False):
    return Footprint(frozenset(reads), frozenset(writes), exclusive)


def test_conflicting_checks_wait_through_publication_but_independent_checks_overlap():
    async def scenario():
        queue = AcceptanceQueue()
        checked, publish, first_done, second_started = (asyncio.Event() for _ in range(4))
        canonical = {'A': 'before'}
        observations = []
        async def first():
            async with queue.reserve(1, scope(['A', 'SharedDefinition'], ['A'])):
                checked.set()
                await publish.wait()
                canonical['A'] = 'published'
            first_done.set()
        async def dependent():
            async with queue.reserve(2, scope(['A', 'B', 'SharedDefinition'], ['B'])):
                observations.append(canonical['A'])
                second_started.set()
        one = asyncio.create_task(first())
        await checked.wait()
        two = asyncio.create_task(dependent())
        await asyncio.sleep(0)
        assert not second_started.is_set(), 'a completed check still owns inputs until publication'
        async with queue.reserve(3, scope(['C', 'SharedDefinition'], ['C'])):
            assert set(queue.active) == {1, 3}
            assert not second_started.is_set()
        publish.set()
        await asyncio.gather(one, two)
        assert observations == ['published'], 'dependent preparation must see the committed source'
        assert not queue.active and not queue.pending
    asyncio.run(scenario())


def test_waiting_exclusive_check_is_not_starved_by_later_work():
    async def scenario():
        queue = AcceptanceQueue()
        order = []
        release = asyncio.Event()
        async def reserve(identity, footprint):
            async with queue.reserve(identity, footprint):
                order.append(identity)
                if identity == 2:
                    await release.wait()
        async with queue.reserve(1, scope(['A'], ['A'])):
            structural = asyncio.create_task(reserve(2, scope(exclusive=True)))
            await asyncio.sleep(0)
            later = asyncio.create_task(reserve(3, scope(['B'], ['B'])))
            await asyncio.sleep(0)
            assert not order
        await asyncio.sleep(0)
        assert order == [2]
        release.set()
        await asyncio.gather(structural, later)
        assert order == [2, 3]
    asyncio.run(scenario())


@pytest.mark.parametrize('active', [False, True])
def test_cancellation_releases_only_owned_reservation(active):
    async def scenario():
        queue = AcceptanceQueue()
        entered = asyncio.Event()
        async def check():
            async with queue.reserve(2, scope(['B'], ['B'])):
                entered.set()
                await asyncio.Event().wait()
        async with queue.reserve(1, scope(['A'], ['A'] if active else ['B'])):
            job = asyncio.create_task(check())
            await asyncio.sleep(0)
            assert entered.is_set() == active
            job.cancel()
            with pytest.raises(asyncio.CancelledError):
                await job
            assert set(queue.active) == {1}
            assert not queue.pending
        assert not queue.active
    asyncio.run(scenario())


def test_duplicate_reservation_fails_without_releasing_original():
    async def scenario():
        queue = AcceptanceQueue()
        async with queue.reserve(1, scope(['A'], ['A'])):
            with pytest.raises(RuntimeError, match='already'):
                async with queue.reserve(1, scope(['B'], ['B'])):
                    pass
            assert set(queue.active) == {1}
    asyncio.run(scenario())


def test_restored_overlapping_checks_readmit_before_using_new_owned_facts(tmp_path):
    from trellis.dispatch.driver import ProductionDriver
    async def scenario():
        driver = ProductionDriver.__new__(ProductionDriver)
        driver.root = tmp_path
        driver.acceptance_queue = AcceptanceQueue()
        driver.acceptance_queue.restore({1: scope(exclusive=True), 2: scope(exclusive=True)})
        driver._latest_acceptance_is_legacy = lambda *_: True
        driver._acceptance_footprint = lambda task, _: (
            scope(['A', 'B'], ['A']) if task == 1 else scope(['B'], ['B']))
        driver.now_ms = lambda: 100
        original_finished = set()
        checked = []
        async def integrate(task, identity, *, legacy_adoption=False):
            if legacy_adoption:
                original_finished.add(task)
                return 'readmit'
            assert original_finished == {1, 2}, 'new proof stage crossed an original full check'
            assert list(driver.acceptance_queue.active) == [task], 'dependent owner checks overlapped'
            checked.append(task)
            await asyncio.sleep(0)
        driver._integrate_reserved = integrate
        await asyncio.gather(driver.integrate(1, 'candidate1'), driver.integrate(2, 'candidate2'))
        assert sorted(checked) == [1, 2]
        assert not driver.acceptance_queue.active
    asyncio.run(scenario())


def test_completed_legacy_lease_waiting_for_readmission_is_not_restored(tmp_path):
    import json
    from contextlib import nullcontext
    from types import SimpleNamespace
    from trellis.dispatch.driver import ProductionDriver
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = tmp_path
    driver.native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=lambda: None)
    driver.state = lambda: {'dispatch': {'tasks': {'1': {
        'id': 1, 'status': {'status': 'checking', 'candidate_sha256': 'candidate'}}}}}
    directory = tmp_path / '1'
    (directory / 'acceptance/1').mkdir(parents=True)
    (directory / 'acceptance/1/canonical.json').write_text('{}')
    (directory / 'acceptance-admission.json').write_text(json.dumps(dict(
        version=1, candidate_sha256='candidate', finished_at_ms=100, readmitting=True,
        footprint=dict(reads=[], writes=[], exclusive=True))))
    assert driver._restore_acceptance_reservations() == {}
