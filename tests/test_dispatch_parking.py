import asyncio
import copy
import json

import pytest

from trellis.dispatch import candidate, parking, staged_acceptance as stages
from trellis.dispatch.acceptance_queue import AcceptanceQueue, Footprint
from trellis.dispatch.driver import ProductionDriver
from test_dispatch_recovery import recovered, put


@pytest.fixture
def parked_fixture(recovered, monkeypatch):
    f = recovered
    stages.complete(f.directory, f.final)
    f.driver.acceptance_queue = AcceptanceQueue()
    f.driver.finalization_gate = asyncio.Lock()
    f.driver._notify = lambda: None
    f.driver.now_ms = lambda: 1
    from trellis.dispatch import driver as module
    async def inline(function, *args, **kwargs):
        return function(*args, **kwargs)
    monkeypatch.setattr(module, '_run_blocking', inline)
    footprint = Footprint(frozenset({'A', 'H'}), frozenset({'A'}))
    f.driver._acceptance_footprint = lambda *_: footprint
    f.footprint = footprint
    return f


def _admission(f):
    value = dict(version=1, task_id=1, candidate_sha256=f.identity,
        footprint=f.driver._footprint_record(f.footprint), queued_at_ms=1, admitted_at_ms=1, adopted=False)
    put(f.driver.root / '1/acceptance-admission.json', value)
    return value


def _blocked(f):
    with pytest.raises(parking.HostCheckBlocked) as error:
        f.driver._retry_check(1, f.identity, f.directory, f.final['reason'])
    return error.value


def test_typed_park_requires_authentication_and_drain_then_retains_exact_evidence(parked_fixture):
    f = parked_fixture
    admission = _admission(f)
    before = {path: path.read_bytes() for path in f.owned.rglob('*.json')}
    blocked = _blocked(f)
    record = parking.persist(f.driver, blocked, admission)
    assert parking.load(f.driver, 1, f.identity, admission) == record
    assert all(path.read_bytes() == data for path, data in before.items())
    assert f.attempt['infrastructure_retries'] == 2
    assert f.task['status']['status'] == 'checking'
    assert not any('pause' in call for call in f.calls)
    assert parking.summary(f.driver.root, f.task)['slot_retained']


@pytest.mark.parametrize('damage', ['unknown', 'receipt', 'live', 'drain', 'channels', 'source', 'phase', 'binding', 'pending'])
def test_unclassified_or_unauthenticated_failures_are_never_parked(parked_fixture, monkeypatch, damage):
    f = parked_fixture
    if damage == 'unknown':
        f.final.pop('retry_class'); f.final['reason'] = 'unclassified internal failure'
        f.save(f.directory, f.final)
        (f.directory / 'staged-completed.json').unlink()
        stages.complete(f.directory, f.final)
    elif damage == 'receipt':
        put(f.directory / 'staged-completed.json', {'forged': True})
    elif damage == 'live':
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: True)
    elif damage == 'drain':
        def fail(*_): raise RuntimeError('drain not acknowledged')
        monkeypatch.setattr('trellis.dispatch.process.AttemptProcess.stop_and_reap', fail)
    elif damage == 'channels':
        (f.directory / 'execution/compiler-channels.json').unlink()
    elif damage == 'source':
        (f.directory / 'repo/Tablet/A.lean').write_text('mutated')
    elif damage == 'phase':
        f.attempt['phase'] = {'phase': 'ready'}
    elif damage == 'binding':
        f.task['assignment']['scope']['nodes'] = ['different']
    else:
        def pending(): raise RuntimeError('pending canonical publication')
        f.driver.native._require_recovered_publication = pending
    with pytest.raises(Exception) as error:
        f.driver._retry_check(1, f.identity, f.directory, f.final['reason'])
    assert not isinstance(error.value, parking.HostCheckBlocked)
    assert not (f.driver.root / '1' / parking.MARKER).exists()


def test_disjoint_publication_continues_conflicting_waits_and_restart_keeps_lease(parked_fixture):
    f = parked_fixture
    async def run():
        parked = asyncio.Event()
        f.driver._notify = parked.set
        calls = []
        for task_id, name in ((2, 'B'), (3, 'C')):
            f.state['dispatch']['tasks'][str(task_id)] = dict(id=task_id,
                status=dict(status='checking', candidate_sha256=name), assignment={})
        footprints = {1: f.footprint, 2: Footprint(frozenset({'B', 'H'}), frozenset({'B'})),
                      3: Footprint(frozenset({'A', 'C'}), frozenset({'C'}))}
        f.driver._acceptance_footprint = lambda task_id, *_: footprints[task_id]
        async def integrate(task_id, identity, **_):
            calls.append(task_id)
            async with f.driver.finalization_gate:
                if task_id == 1:
                    f.driver._retry_check(1, f.identity, f.directory, f.final['reason'])
                f.state['dispatch']['tasks'][str(task_id)]['status'] = {'status': 'resolved'}
        f.driver._integrate_reserved = integrate
        first = asyncio.create_task(f.driver.integrate(1, f.identity))
        await asyncio.wait_for(parked.wait(), 1)
        sibling = asyncio.create_task(f.driver.integrate(2, 'B'))
        conflict = asyncio.create_task(f.driver.integrate(3, 'C'))
        await asyncio.wait_for(sibling, 1)
        await asyncio.sleep(0)
        assert not first.done() and not conflict.done()
        assert calls == [1, 2]
        assert f.driver.acceptance_queue.active == {1: f.footprint}
        assert f.task['status']['status'] == 'checking'
        original_admission = (f.driver.root / '1/acceptance-admission.json').read_bytes()
        conflict.cancel(); first.cancel()
        await asyncio.gather(first, conflict, return_exceptions=True)
        assert 'finished_at_ms' not in json.loads(original_admission)
        # All unfinished admissions are restored before a new check can queue.
        f.driver.acceptance_queue = AcceptanceQueue()
        f.driver.acceptance_queue.restore(f.driver._restore_acceptance_reservations())
        assert f.driver.acceptance_queue.active == {1: f.footprint}
        restarted = asyncio.create_task(f.driver.integrate(1, f.identity))
        for _ in range(3): await asyncio.sleep(0)
        assert not restarted.done() and calls == [1, 2]
        assert (f.driver.root / '1/acceptance-admission.json').read_bytes() == original_admission
        restarted.cancel(); await asyncio.gather(restarted, return_exceptions=True)
    asyncio.run(run())


def test_unchanged_and_rejected_wakes_do_not_repeat_auth_or_checks(parked_fixture, monkeypatch):
    f = parked_fixture
    record = parking.persist(f.driver, _blocked(f), _admission(f))
    signatures = parking._bindings(f.driver, record)
    original = parking._authentication
    calls = []
    def authenticate(directory):
        calls.append(directory)
        return original(directory)
    monkeypatch.setattr(parking, '_authentication', authenticate)
    for _ in range(10):
        action, signatures = parking._poll(f.driver, record, signatures)
        assert action == 'waiting'
    assert calls == []
    f.driver._parking_implementation = 'new-reviewable-host'
    for _ in range(10):
        action, signatures = parking._poll(f.driver, record, signatures)
        assert action == 'waiting'
    assert len(calls) == 1
    assert f.attempt['infrastructure_retries'] == 2
    assert not any(call.get('action') == 'dispatch_control' for call in f.calls)


def test_exact_grant_wakes_once_and_existing_owner_receipt_is_reused(parked_fixture):
    f = parked_fixture
    from trellis.dispatch import recovery
    async def run():
        notified = asyncio.Event()
        f.driver._notify = notified.set
        invocations = []
        owned_bytes = (f.owned / 'staged-completed.json').read_bytes()
        async def integrate(*_, **__):
            invocations.append('finalize')
            # The same ordinary immutable owner reuse used by staged._execute;
            # no owner producer or compiler is invoked on recovery.
            key = json.loads((f.owned / 'canonical.json').read_text())['key']
            assert stages.reusable(f.owned, key)['status'] == 'facts'
            async with f.driver.finalization_gate:
                f.driver._retry_check(1, f.identity, f.directory, f.final['reason'])
        f.driver._integrate_reserved = integrate
        runner = asyncio.create_task(f.driver.integrate(1, f.identity))
        await asyncio.wait_for(notified.wait(), 1)
        for _ in range(3):
            f.driver._host_park_waiters[1].set(); await asyncio.sleep(0)
        assert invocations == ['finalize']
        # Tests native maintenance separately; here it records its exact grant
        # while fixture locks permit simulating a stopped/resumed coordinator.
        recovery.apply(f.driver, 1, 4, 3)
        f.driver._host_park_waiters[1].set()
        await asyncio.wait_for(runner, 1)
        assert invocations == ['finalize', 'finalize']
        assert f.attempt['infrastructure_retries'] == 3
        assert (f.owned / 'staged-completed.json').read_bytes() == owned_bytes
        assert f.driver.acceptance_queue.active == {}
    asyncio.run(run())


def test_corrupt_old_receipt_is_fatal_even_without_wake_key_change(parked_fixture):
    f = parked_fixture
    record = parking.persist(f.driver, _blocked(f), _admission(f))
    signatures = parking._bindings(f.driver, record)
    (f.directory / 'execution/response.json').write_text('{}')
    with pytest.raises(candidate.CandidateError, match='immutable evidence changed'):
        parking._poll(f.driver, record, signatures)


def test_missing_or_changed_durable_admission_cannot_release_parked_scope(parked_fixture):
    f = parked_fixture
    parking.persist(f.driver, _blocked(f), _admission(f))
    (f.driver.root / '1/acceptance-admission.json').unlink()
    with pytest.raises(candidate.CandidateError, match='unfinished semantic admission'):
        f.driver._restore_acceptance_reservations()


def test_external_native_resolution_retires_park_without_another_check(parked_fixture):
    f = parked_fixture
    record = parking.persist(f.driver, _blocked(f), _admission(f))
    f.task['status'] = {'status': 'resolved'}
    assert parking._poll(f.driver, record, {})[0] == 'retired'


def test_unrelated_runtime_exception_is_not_swallowed_inside_reservation(parked_fixture):
    f = parked_fixture
    async def run():
        async def fail(*_, **__): raise RuntimeError('canonical invariant broken')
        f.driver._integrate_reserved = fail
        with pytest.raises(RuntimeError, match='canonical invariant broken'):
            await f.driver.integrate(1, f.identity)
        assert not (f.driver.root / '1' / parking.MARKER).exists()
        assert 'finished_at_ms' not in json.loads((f.driver.root / '1/acceptance-admission.json').read_text())
    asyncio.run(run())


def test_native_snapshot_keeps_checking_slot_and_exposes_host_only_diagnostic(parked_fixture):
    f = parked_fixture
    parking.persist(f.driver, _blocked(f), _admission(f))
    status = dict(tasks={'1': copy.deepcopy(f.task)}, verifiers={'jobs': {}}, available_slots=3)
    f.driver.pausing = lambda: True
    f.driver.native._call = lambda _: copy.deepcopy(status)
    snapshot = f.driver._snapshot()
    assert snapshot['tasks']['1']['status'] == f.task['status']
    assert snapshot['available_slots'] == 3
    blocked = snapshot['tasks']['1']['host_check_blocked']
    assert blocked['summary'].startswith('Host check blocked: ')
    assert blocked['evidence'] == '1/acceptance/4/finalization/3'
    assert blocked['slot_retained'] is True
    assert 'host_check_blocked' not in f.task  # No native/proof state mutation.


def test_restore_reconciles_external_native_candidate_transition(parked_fixture):
    f = parked_fixture
    parking.persist(f.driver, _blocked(f), _admission(f))
    f.task['status'] = {'status': 'resolved'}
    assert f.driver._restore_acceptance_reservations() == {}
    marker = json.loads((f.driver.root / '1' / parking.MARKER).read_text())
    assert marker['state'] == 'retired' and marker['retired_by_native_status'] == f.task['status']


def test_changed_admission_during_park_is_global_invariant_failure(parked_fixture):
    f = parked_fixture
    record = parking.persist(f.driver, _blocked(f), _admission(f))
    path = f.driver.root / '1/acceptance-admission.json'
    value = json.loads(path.read_text()); value['footprint']['writes'] = []
    put(path, value)
    with pytest.raises(candidate.CandidateError, match='semantic admission changed'):
        parking._poll(f.driver, record, parking._bindings(f.driver, record))


def test_live_stopping_snapshot_retires_marker_before_cancel_and_same_id_correction(parked_fixture):
    f = parked_fixture
    async def run():
        parked = asyncio.Event()
        f.driver._notify = parked.set
        invocations = []
        async def integrate(task_id, identity, **_):
            invocations.append(identity)
            if identity == f.identity:
                f.driver._retry_check(task_id, identity, f.directory, f.final['reason'])
        f.driver._integrate_reserved = integrate
        first = asyncio.create_task(f.driver.integrate(1, f.identity))
        await asyncio.wait_for(parked.wait(), 1)
        # Native stopping can win immediately before Coordinator._reap cancels
        # the dormant integration, so its own poll need never see this state.
        f.task['status'] = {'status': 'stopping', 'outcome': {'kind': 'cancelled'}}
        status = dict(tasks={'1': copy.deepcopy(f.task)}, verifiers={'jobs': {}}, available_slots=0)
        f.driver.pausing = lambda: True
        f.driver.native._call = lambda _: copy.deepcopy(status)
        f.driver._snapshot()
        marker = json.loads((f.driver.root / '1' / parking.MARKER).read_text())
        assert marker['state'] == 'retired'
        first.cancel(); await asyncio.gather(first, return_exceptions=True)
        f.task['status'] = {'status': 'checking', 'candidate_sha256': 'corrected'}
        await f.driver.integrate(1, 'corrected')
        assert invocations == [f.identity, 'corrected']
        admission = json.loads((f.driver.root / '1/acceptance-admission.json').read_text())
        assert admission['candidate_sha256'] == 'corrected' and 'finished_at_ms' in admission
    asyncio.run(run())


def test_owned_capture_can_park_but_implementation_change_does_not_mint_recovery(parked_fixture):
    f = parked_fixture
    owner = json.loads((f.owned / 'execution/response.json').read_text())
    owner.pop('facts')
    owner.update(status='cold', retry_class='owned_environment_capture', reason='owner capture command failed')
    f.save(f.owned, owner)
    (f.owned / 'staged-completed.json').unlink()
    stages.complete(f.owned, owner)
    with pytest.raises(parking.HostCheckBlocked) as error:
        f.driver._retry_check(1, f.identity, f.owned, owner['reason'])
    record = parking.persist(f.driver, error.value, _admission(f))
    f.driver._parking_implementation = 'new-implementation'
    assert parking._poll(f.driver, record, parking._bindings(f.driver, record))[0] == 'waiting'
    assert f.attempt['infrastructure_retries'] == 2
    assert 'infrastructure_recovery_grant' not in f.attempt
    assert not any(call.get('action') == 'dispatch_control' for call in f.calls)
