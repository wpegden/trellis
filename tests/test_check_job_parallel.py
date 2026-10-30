import concurrent.futures
import json
import threading
import time

import pytest

from trellis.checker.admission import Admission
from trellis.checker.jobs import JobService, register_workspace, revoke_workspace


def register(tmp_path, name, priority="interactive", repo=None):
    repo = repo or tmp_path/name
    (repo/'Tablet').mkdir(parents=True, exist_ok=True)
    (repo/'Tablet/N.lean').write_text('example : True := by trivial\n')
    token = register_workspace(tmp_path/'runtime', repo, scope=name, priority=priority)['TRELLIS_CHECK_JOBS_TOKEN']
    return token, repo


def submit(service, token, request_id='one'):
    return service.handle(dict(action='submit', token=token, request_id=request_id,
                               kind='build', target='N', timeout=10))


def terminal(service, token, job):
    deadline = time.monotonic()+5
    while time.monotonic() < deadline:
        result = service.handle(dict(action='status', token=token, job_id=job['job_id']))
        if result['state'] not in {'queued', 'running'}:
            return result
        time.sleep(.01)
    raise AssertionError('job did not finish')


def test_different_workspaces_overlap_without_crossing_results(tmp_path):
    entered = threading.Barrier(2)
    def execute(spec, job):
        entered.wait(3)
        return dict(returncode=0, stdout=spec['scope'], stderr='')
    a, _ = register(tmp_path, 'a')
    b, _ = register(tmp_path, 'b')
    service = JobService(tmp_path/'runtime', scheduler=Admission(2), execute=execute)
    service.start()
    try:
        ja, jb = submit(service, a), submit(service, b)
        ra, rb = terminal(service, a, ja), terminal(service, b, jb)
        assert ra['state'] == rb['state'] == 'succeeded'
        assert ra['result']['stdout'] == 'a'
        assert rb['result']['stdout'] == 'b'
        assert max(ra['started_at'], rb['started_at']) < min(ra['finished_at'], rb['finished_at'])
    finally:
        service.shutdown()


def test_two_capabilities_for_one_workspace_never_overlap(tmp_path):
    entered, release = threading.Event(), threading.Event()
    calls = []
    def execute(spec, job):
        calls.append(spec['scope'])
        if spec['scope'] == 'first':
            entered.set()
            assert release.wait(3)
        return dict(returncode=0)
    a, repo = register(tmp_path, 'first')
    b, _ = register(tmp_path, 'second', repo=repo)
    service = JobService(tmp_path/'runtime', scheduler=Admission(2), execute=execute)
    service.start()
    try:
        ja = submit(service, a)
        assert entered.wait(2)
        jb = submit(service, b)
        time.sleep(.1)
        assert calls == ['first']
        release.set()
        assert terminal(service, a, ja)['state'] == 'succeeded'
        assert terminal(service, b, jb)['state'] == 'succeeded'
    finally:
        release.set()
        service.shutdown()


def test_long_background_job_leaves_a_slot_for_interactive_work(tmp_path):
    entered, release, primary = threading.Event(), threading.Event(), threading.Event()
    def execute(spec, job):
        if spec['scope'] == 'background1':
            entered.set()
            assert release.wait(3)
        elif spec['scope'] == 'background2':
            assert release.is_set(), 'second background used the foreground slot'
        else:
            primary.set()
        return dict(returncode=0)
    a, _ = register(tmp_path, 'background1', 'background')
    b, _ = register(tmp_path, 'background2', 'background')
    c, _ = register(tmp_path, 'worker')
    service = JobService(tmp_path/'runtime', scheduler=Admission(2), execute=execute)
    service.start()
    try:
        ja = submit(service, a)
        assert entered.wait(2)
        jb = submit(service, b)
        jc = submit(service, c)
        assert primary.wait(2), 'foreground check blocked behind background builds'
        assert terminal(service, c, jc)['state'] == 'succeeded'
        release.set()
        assert terminal(service, a, ja)['state'] == terminal(service, b, jb)['state'] == 'succeeded'
    finally:
        release.set()
        service.shutdown()


def test_memory_reservations_wait_and_report_reason():
    available = [10]
    scheduler = Admission(2, available=lambda: available[0], floor_bytes=2, reserve_bytes=6)
    entered, release = threading.Event(), threading.Event()
    reasons = []
    def next_job():
        with scheduler.acquire('next', waiting=reasons.append):
            entered.set()
            release.wait(2)
    with scheduler.acquire('first'):
        thread = threading.Thread(target=next_job)
        thread.start()
        assert not entered.wait(.1)
        assert any('memory headroom' in reason for reason in reasons)
        available[0] = 16
        with scheduler.condition:
            scheduler.condition.notify_all()
        assert entered.wait(2)
        release.set()
        thread.join(2)
    assert not scheduler.active


def test_shutdown_cancels_admission_wait_without_waiting_for_authority(tmp_path):
    token, _ = register(tmp_path, 'worker')
    service = JobService(tmp_path/'runtime', execute=lambda *_: pytest.fail('compiler started'))
    with service.scheduler.acquire('authoritative'):
        service.start()
        job = submit(service, token)
        service.shutdown()
        assert terminal(service, token, job)['state'] == 'cancelled'


def test_queued_authority_does_not_evict_warm_work_until_it_can_start():
    scheduler = Admission(2)
    evicted, waiting = threading.Event(), threading.Event()
    with scheduler.acquire('warm'):
        scheduler.retain('warm', evicted.set)
    with concurrent.futures.ThreadPoolExecutor(1) as pool:
        with scheduler.acquire('authoritative'):
            def authority():
                with scheduler.acquire('authoritative', waiting=lambda _: waiting.set()):
                    pass
            future = pool.submit(authority)
            assert waiting.wait(2)
            assert not evicted.is_set()
        future.result(2)
    assert not evicted.is_set(), 'a spare compiler slot did not require eviction'


def test_failed_process_cleanup_closes_admission_instead_of_reusing_capacity():
    scheduler = Admission(2)
    with pytest.raises(RuntimeError, match='compute lease retained'):
        with scheduler.acquire('failed-job'):
            scheduler.track_process(99999999)
            # Simulate cleanup failing before process removal. Capacity must
            # stay held and even this same thread must not re-enter the lease.
    assert scheduler.active
    with pytest.raises(OSError, match='compiler cleanup incomplete'):
        with scheduler.acquire('authoritative'):
            pytest.fail('unreaped compiler capacity was reused')


@pytest.mark.parametrize('running', [False, True])
def test_capability_revocation_cancels_live_owner_before_capacity_release(tmp_path, running):
    token, _ = register(tmp_path, 'worker')
    entered = threading.Event()
    service = JobService(tmp_path/'runtime')
    def execute(spec, job):
        entered.set()
        deadline = time.monotonic() + 3
        while True:
            service._check_stop(job, spec, deadline)
            time.sleep(.01)
    service.execute_override = execute
    from contextlib import nullcontext
    try:
        with nullcontext() if running else service.scheduler.acquire('authoritative'):
            service.start()
            job = submit(service, token)
            if running:
                assert entered.wait(2)
            revoke_workspace(tmp_path/'runtime', token)
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                with service.lock:
                    row = service.db.execute('SELECT state,result FROM jobs WHERE id=?', (job['job_id'],)).fetchone()
                if row['state'] == 'cancelled':
                    break
                time.sleep(.01)
            assert row['state'] == 'cancelled'
            assert 'revoked' in json.loads(row['result'])['message']
            assert entered.is_set() == running
    finally:
        service.shutdown()


def test_launch_configurable_compiler_capacity_retains_memory_admission():
    scheduler = Admission(4, available=lambda: 10, floor_bytes=2, reserve_bytes=6)
    with scheduler.acquire('first'):
        admitted, reason = scheduler._can_admit('next', 1, 6)
        assert not admitted and 'memory headroom' in reason
    for value in (True, 0, -1, 1.5):
        with pytest.raises(ValueError, match='positive integer'):
            Admission(value)


def test_release_ack_waits_for_job_cleanup_and_cannot_affect_another_token(tmp_path):
    token, _ = register(tmp_path, 'worker')
    other, _ = register(tmp_path, 'other')
    entered, reap = threading.Event(), threading.Event()
    service = JobService(tmp_path/'runtime', scheduler=Admission(2))
    def execute(spec, job):
        if spec['scope'] == 'worker':
            entered.set()
            assert reap.wait(3)
            service._check_stop(job, spec, float('inf'))
        return dict(returncode=0)
    service.execute_override = execute
    service.start()
    try:
        job = submit(service, token)
        assert entered.wait(2)
        release = lambda: service.handle(dict(action='release', token=token))
        assert release() == {'drained': False}
        with pytest.raises(ValueError, match='expired'):
            submit(service, token, 'late')
        assert terminal(service, other, submit(service, other))['state'] == 'succeeded'
        reap.set()
        deadline = time.monotonic() + 3
        while not release()['drained'] and time.monotonic() < deadline:
            time.sleep(.01)
        assert release() == {'drained': True}
        row = service.db.execute('SELECT state FROM jobs WHERE id=?', (job['job_id'],)).fetchone()
        assert row['state'] == 'cancelled'
    finally:
        reap.set()
        service.shutdown()


def test_release_reaps_warm_compiler_before_acknowledging(tmp_path):
    import hashlib
    token, _ = register(tmp_path, 'worker')
    service = JobService(tmp_path/'runtime')
    owner = hashlib.sha256(token.encode()).hexdigest()
    reaped = []
    with service.scheduler.acquire(owner):
        service.scheduler.retain(owner, lambda: reaped.append(True))
    assert service.release(token)['drained']
    assert reaped == [True]
    assert not service.scheduler.residents
    service.scheduler.cleanup_failed = True
    with pytest.raises(RuntimeError, match='retain the worker slot'):
        service.release(token)


def test_cancelled_queued_job_cannot_be_collected_before_its_claim_drains(tmp_path):
    token, _ = register(tmp_path, 'worker')
    other, _ = register(tmp_path, 'other')
    service = JobService(tmp_path/'runtime', execute=lambda *_: dict(returncode=0))
    # Keep the queued worker at its cancellation poll while collection is
    # exercised: inside the window the poll reports nothing. Holding the
    # service lock across handle instead would take the admission lock under
    # the database lock, the reverse of the documented order, and deadlock
    # against the worker polling under the admission lock.
    original_check_stop, window = service._check_stop, threading.Event()
    def check_stop(job, spec, deadline, *args, **kwargs):
        if window.is_set():
            return None
        return original_check_stop(job, spec, deadline, *args, **kwargs)
    service._check_stop = check_stop
    service.start()
    try:
        with service.scheduler.acquire('canonical'):
            job = submit(service, token)
            deadline = time.monotonic()+3
            while job['job_id'] not in service.claimed and time.monotonic()<deadline:
                time.sleep(.01)
            assert job['job_id'] in service.claimed
            window.set()
            # The client-visible status is already terminal, but the admission
            # ticket still belongs to the attempt.
            service.handle(dict(action='cancel', token=token, job_id=job['job_id']))
            revoke_workspace(tmp_path/'runtime', token)
            submit(service, other)
            assert service.db.execute('SELECT 1 FROM jobs WHERE id=?', (job['job_id'],)).fetchone()
            assert not service.release(token)['drained']
            window.clear()
        deadline = time.monotonic()+3
        while not service.release(token)['drained'] and time.monotonic()<deadline:
            time.sleep(.01)
        assert service.release(token)['drained']
    finally:
        service.shutdown()


def test_failed_warm_session_is_reaped_before_lease_release(tmp_path):
    token, _ = register(tmp_path, 'worker')
    service = JobService(tmp_path/'runtime')
    reaping, release, acquired = threading.Event(), threading.Event(), threading.Event()
    def execute(spec, job, deadline, session, *, captured=None):
        class Broker:
            def _teardown_server(self):
                reaping.set()
                assert release.wait(3)
        session.broker = Broker()
        session.broker_owner = job['owner']
        raise RuntimeError('synthetic server failure')
    service._execute = execute
    service.start()
    pool = concurrent.futures.ThreadPoolExecutor(1)
    try:
        job = submit(service, token)
        assert reaping.wait(2)
        def authority():
            with service.scheduler.acquire('authoritative'):
                acquired.set()
        future = pool.submit(authority)
        assert not acquired.wait(.1)
        release.set()
        future.result(2)
        assert terminal(service, token, job)['state'] == 'infrastructure_error'
    finally:
        release.set()
        service.shutdown()
        pool.shutdown()


@pytest.mark.parametrize('kind,required', [('file', 'Tablet/*.lean'), ('scratch', '.trellis/scratch/*.lean')])
def test_path_errors_name_the_requested_input_kind(tmp_path, kind, required):
    token, _ = register(tmp_path, 'worker')
    service = JobService(tmp_path/'runtime')
    with pytest.raises(ValueError) as caught:
        service.handle(dict(action='submit', token=token, request_id='one', kind=kind, target='../N.lean'))
    assert required in str(caught.value)
