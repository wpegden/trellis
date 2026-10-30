import asyncio
from contextlib import nullcontext
import json
from types import SimpleNamespace

import pytest

from trellis.dispatch.coordination import Coordinator
from trellis.dispatch.driver import ProductionDriver
from trellis.dispatch.native import NativeLedger
from trellis.dispatch.preparation_capacity import PreparationCapacity, PreparationDeferred


def capacity(tmp_path, *, free=20, inodes=1000):
    (tmp_path / 'dispatch').mkdir(exist_ok=True)
    now = [100.0]
    space = SimpleNamespace(f_bavail=free, f_frsize=1, f_favail=inodes)
    reads = []
    def stat(path):
        reads.append(path)
        return space
    gate = PreparationCapacity(tmp_path, tmp_path, floor_bytes=10, workspace_bytes=3,
        floor_inodes=100, workspace_inodes=10, clock=lambda: now[0], statvfs=stat)
    return gate, now, space, reads


def test_capacity_backoff_rechecks_space_without_protocol_change(tmp_path):
    gate, now, space, reads = capacity(tmp_path, free=12)
    with pytest.raises(PreparationDeferred):
        gate.require()
    assert not gate.ready() and len(reads) == 1  # Same filesystem counted once.
    receipt = tmp_path / 'dispatch/preparation-capacity.json'
    assert json.loads(receipt.read_text())['waiting']
    space.f_bavail = 100
    with pytest.raises(PreparationDeferred):
        gate.require()
    assert len(reads) == 1
    now[0] += 30
    assert gate.ready()
    gate.require()
    assert len(reads) == 2 and not json.loads(receipt.read_text())['waiting']


def test_atomic_preparation_estimate_is_not_charged_twice(tmp_path):
    gate, _, space, reads = capacity(tmp_path)
    with gate.admitted(3):
        space.f_bavail = 11  # The admitted batch has written its own first objects.
        gate.require(3)
        gate.require(1)
        assert len(reads) == 1
    with pytest.raises(PreparationDeferred):
        gate.require()
    assert len(reads) == 2


def test_inode_pressure_and_real_io_failure_are_distinct(tmp_path):
    gate, _, _, _ = capacity(tmp_path, inodes=109)
    with pytest.raises(PreparationDeferred):
        gate.require()
    gate, _, _, _ = capacity(tmp_path)
    def fail(_):
        raise OSError('real filesystem failure')
    gate.statvfs = fail
    with pytest.raises(OSError, match='real filesystem'):
        gate.require()
    assert gate.ready()


def test_zero_worker_batch_needs_no_allocation_and_reuses_no_lease(tmp_path):
    gate, _, _, reads = capacity(tmp_path, free=0)
    with gate.admitted(0):
        assert reads == []
    with pytest.raises(PreparationDeferred):
        gate.require()


def test_retry_storage_wait_does_not_poison_protocol_inode_cache(tmp_path):
    gate, now, space, _ = capacity(tmp_path, free=12)
    ledger = NativeLedger(tmp_path, preparation_capacity=gate)
    state = {'dispatch': {'tasks': {'1': {'pending_retry': {}, 'status': {'status': 'resolved'}}}}}
    (tmp_path / 'protocol_state.json').write_text(json.dumps(state))
    observations = []
    ledger.observe_sources = lambda *_: observations.append(True) or {
        'graph': {}, 'input_hashes': {}, 'manifest': {}}
    ledger._call = lambda _: None
    with pytest.raises(PreparationDeferred):
        ledger.prepare_retries(tmp_path)
    assert ledger._no_retry_snapshot is None and observations == []
    space.f_bavail = 100; now[0] += 30
    assert ledger.prepare_retries(tmp_path) == []
    assert observations == [True]


def test_committed_batch_adoption_bypasses_fresh_storage_wait(tmp_path):
    gate, _, _, _ = capacity(tmp_path, free=0)
    ledger = NativeLedger(tmp_path, preparation_capacity=gate)
    state = {'dispatch': {'receipts': {'3': {}}, 'config': {'profiles': {'sol': {}}}}}
    (tmp_path / 'protocol_state.json').write_text(json.dumps(state))
    ledger.observe_sources = lambda *_: {'graph': {}, 'input_hashes': {}, 'manifest': {}}
    seen = []
    ledger._dispatch_batch = lambda *a, **kw: seen.append((a, kw)) or 'authenticated downstream'
    assert ledger.dispatch_current_batch(3, {'assignments': []}, tmp_path) == 'authenticated downstream'
    assert len(seen) == 1


def test_fresh_batch_wait_precedes_source_capture(tmp_path):
    gate, _, _, _ = capacity(tmp_path, free=12)
    ledger = NativeLedger(tmp_path, preparation_capacity=gate)
    (tmp_path / 'protocol_state.json').write_text(json.dumps({'dispatch': {'receipts': {}}}))
    ledger.observe_sources = lambda *_: pytest.fail('source capture before admission')
    with pytest.raises(PreparationDeferred):
        ledger.dispatch_current_batch(3, {'assignments': [{}]}, tmp_path)


def test_adopted_review_honors_deferred_start(tmp_path):
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = tmp_path
    driver._start_review = lambda: None
    driver._process = lambda *a, **k: pytest.fail('unprepared review launched')
    asyncio.run(driver.review({'request_id': 3}))


def test_deferred_post_commit_preparation_never_replays_route(tmp_path):
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=lambda: None)
    driver.pausing = lambda: False
    events = []
    status = {'committed': True}
    driver._routing = lambda event: events.append(event) or status
    def defer(_):
        raise PreparationDeferred()
    driver._prepare_verifiers = defer
    event = {'kind': 'verifier_finished', 'id': 2}
    assert asyncio.run(driver._route(event)) is status
    assert events == [event]


def test_preparation_receipt_still_undergoes_original_authentication(tmp_path):
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.preparation_capacity = capacity(tmp_path, free=0)[0]
    destination = tmp_path / 'dispatch/1'; destination.mkdir()
    (destination / 'prepared.json').write_text('{}')
    def authenticate(*a, **k):
        raise ValueError('binding mismatch')
    driver._prepare = authenticate
    with pytest.raises(ValueError, match='binding mismatch'):
        driver._prepare_fresh(destination, {})


def test_storage_wait_preserves_checking_work_and_suppresses_fresh_spin():
    from test_dispatch_coordination import Driver, tick
    async def scenario():
        driver = Driver()
        driver.tasks['1']['status'] = {'status': 'reserved'}
        driver.tasks['2']['status'] = {'status': 'checking', 'candidate_sha256': 'a'}
        driver.preparation_ready = lambda: False
        driver.seq = 1
        coordinator = Coordinator(driver)
        try:
            for _ in range(4):
                await tick(coordinator)
            assert coordinator.runners == {} and driver.review_count == 0
            assert driver.active_checks == 1 and set(coordinator.integrations) == {2}
        finally:
            await coordinator.shutdown()
    asyncio.run(scenario())


def test_late_storage_deferral_has_no_candidate_delivery():
    from test_dispatch_coordination import Driver
    async def scenario():
        driver = Driver()
        async def defer(_):
            raise PreparationDeferred()
        async def deliver(*_):
            pytest.fail('deferral is not a submitted candidate')
        driver.launch_or_adopt, driver.complete_delivery = defer, deliver
        await Coordinator(driver)._execute(driver.tasks['1'])
        assert driver.tasks['1']['status'] == {'status': 'running'}
    asyncio.run(scenario())


def test_prepared_targets_continue_at_exact_floor_without_second_allocation(tmp_path):
    driver=ProductionDriver.__new__(ProductionDriver);driver.root=tmp_path/'dispatch'
    gate,_,space,_=capacity(tmp_path,free=13);driver.preparation_capacity=gate
    with gate.admitted(1):
        directory=driver.root/'7';directory.mkdir();(directory/'prepared.json').write_text('{}')
        space.f_bavail=10
    with pytest.raises(PreparationDeferred):gate.require()
    assert driver.worker_preparation_ready({'id':7})
    assert not driver.worker_preparation_ready({'id':8})
    assert driver.prepared_target_ready(directory)
    # Prepared review/verifier/exclusive targets retain scheduling eligibility
    # while each actual adoption still checks its ordinary identity/recipe.
    for name in ['review-3','verifier-4','exclusive-5']:
        p=driver.root/name;p.mkdir();(p/'prepared.json').write_text('{}')
    assert driver.review_preparation_ready({'request_id':3})
    assert driver.verifier_preparation_ready({'verifiers':{'jobs':{'4':{'id':4,'status':{'status':'queued'}}}}})
    assert driver.verifier_preparation_ready({'request':{'id':5}})
    assert not driver.verifier_preparation_ready({'request':{'id':6}})


def test_saved_materialized_retry_adopts_during_storage_backoff(tmp_path):
    from trellis.dispatch.candidate import canonical_json,digest
    gate,_,_,_=capacity(tmp_path,free=0)
    ledger=NativeLedger(tmp_path,preparation_capacity=gate)
    state={'dispatch':{'tasks':{'1':{'pending_retry':{},'status':{'status':'resolved'}}}}}
    (tmp_path/'protocol_state.json').write_bytes(canonical_json(state))
    retry={'task_id':2,'worker':{},'event':{'kind':'retry_prepared'}}
    saved=ledger.root/'retries/1/prepared.json';saved.parent.mkdir(parents=True)
    saved.write_bytes(canonical_json({'base_sha256':digest(canonical_json(state)),'prepared':retry}))
    target=ledger.root/'2';target.mkdir();(target/'prepared.json').write_text('{}')
    with pytest.raises(PreparationDeferred):gate.require()
    ledger.observe_sources=lambda *_:{'graph':{},'input_hashes':{},'manifest':{}}
    events=[];ledger._call=lambda req:events.append(req)
    assert ledger.prepare_retries(tmp_path)==[retry]
    assert [x['event'] for x in events]==[retry['event']]


def test_retained_reserved_launcher_is_adopted_beside_blocked_new_work():
    from test_dispatch_coordination import Driver,tick
    async def scenario():
        driver=Driver();driver.seq=1
        driver.tasks['1']['status']={'status':'reserved'}
        driver.tasks['2']['status']={'status':'reserved'}
        driver.preparation_ready=lambda:False
        driver.worker_preparation_ready=lambda task:task['id']==1
        coordinator=Coordinator(driver)
        try:
            await tick(coordinator)
            assert 1 in coordinator.runners and 2 not in coordinator.runners
        finally:await coordinator.shutdown()
    asyncio.run(scenario())


@pytest.mark.parametrize('task_id', [3, None])
def test_earlier_fresh_retry_does_not_block_later_saved_retry_or_refusal(tmp_path, task_id):
    from trellis.dispatch.candidate import canonical_json, digest
    gate, _, _, _ = capacity(tmp_path, free=0)
    ledger = NativeLedger(tmp_path, preparation_capacity=gate)
    state = {'dispatch': {'tasks': {
        str(i): {'pending_retry': {}, 'status': {'status': 'resolved'}} for i in (1, 2)}}}
    (tmp_path / 'protocol_state.json').write_bytes(canonical_json(state))
    retry = {'task_id': task_id, 'worker': {}, 'event': {'kind': 'retry_prepared'}}
    saved = ledger.root / 'retries/2/prepared.json'; saved.parent.mkdir(parents=True)
    saved.write_bytes(canonical_json({'base_sha256': digest(canonical_json(state)), 'prepared': retry}))
    if task_id is not None:
        target = ledger.root / str(task_id); target.mkdir()
        (target / 'prepared.json').write_text('{}')
    ledger.observe_sources = lambda *_: {'graph': {}, 'input_hashes': {}, 'manifest': {}}
    events = []; ledger._call = lambda req: events.append(req)
    assert ledger.prepare_retries(tmp_path) == [retry]
    assert [req['event'] for req in events] == [retry['event']]
    assert ledger._no_retry_snapshot is None
    assert not gate.ready()  # Fresh preparation remains deferred.


def test_native_start_cycle_control_remains_ready_during_storage_backoff(tmp_path):
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = tmp_path / 'dispatch'
    driver.preparation_capacity = capacity(tmp_path, free=0)[0]
    with pytest.raises(PreparationDeferred): driver.preparation_capacity.require()
    assert driver.verifier_preparation_ready({'stage': 'Start', 'request': None})
    assert not driver.verifier_preparation_ready({'stage': 'Worker', 'request': None})
