"""Available-memory policy preserves admission ownership and typed recovery."""
import hashlib
import json
import threading
import time

import pytest

from trellis.checker import admission, memory_retry, serial_recovery
from trellis.checker.protocol import ProtocolError
from trellis.memory_guard import GIB


def scheduler(tmp_path, *, available=None):
    return admission.Admission(6, available=available or (lambda: 60 * GIB),
        floor_bytes=8 * GIB, reserve_bytes=24 * GIB,
        readonly_reserve_bytes=14 * GIB, readonly_slots=3,
        optimistic=True, available_memory=True, retry_root=tmp_path / 'history')


def record(sequence, pid, **kw):
    return dict(owner=str(sequence), admission_sequence=sequence, processes={pid},
                priority=0, estimate=48 * GIB, peak_bytes=0, controlled=True, **kw)


def test_floor_not_reservations_or_learned_peaks_controls_memory_admission(tmp_path, monkeypatch):
    available = [9 * GIB]
    pool = scheduler(tmp_path, available=lambda: available[0])
    pool.active = {11: record(0, 10), 12: record(1, 20)}
    monkeypatch.setattr(pool, '_resident_bytes', lambda _: pytest.fail('reservation accounting'))
    assert pool._can_admit('third', 0, 48 * GIB) == (True, None)
    available[0] = 8 * GIB
    assert pool._can_admit('third', 0, 48 * GIB)[0]
    available[0] -= 1
    assert not pool._can_admit('third', 0, 0)[0]
    available[0] = 9 * GIB
    assert not pool._can_admit('0', 0, 0)[0]  # Same workspace writer.
    assert not pool._can_admit('third', 0, 0, exclusive=True)[0]
    pool.max_active = 2
    assert not pool._can_admit('third', 0, 0)[0]


def test_newest_admitted_small_job_obeys_sticky_pressure_cancellation(tmp_path, monkeypatch):
    available = [7 * GIB]
    pool = scheduler(tmp_path, available=lambda: available[0])
    pool.active = {11: record(8, 100), 12: record(9, 200), 13: record(7, 300)}
    thread = [11]
    monkeypatch.setattr(admission.threading, 'get_ident', lambda: thread[0])
    assert pool.pressure_reason('old large reporter') is None
    assert pool.pressure_victim == 12
    assert not pool._can_admit('next', 0, 0)[0]
    # The selected small job's ordinary guard has no pressure reason at all.
    class Guard:
        limit_bytes, peak_bytes = 0, 1
        def check(self, pid):
            return None
    thread[0] = 12
    available[0] = 30 * GIB
    stop = pool.memory_stop(Guard(), 200)
    assert stop.cause == 'host_pressure' and 'newest admitted' in stop.reason
    assert pool.pressure_victim == 12


def test_hard_limit_is_not_redirected_to_newest_pressure_victim(tmp_path, monkeypatch):
    pool = scheduler(tmp_path, available=lambda: 7 * GIB)
    pool.active = {11: record(1, 100), 12: record(2, 200)}
    monkeypatch.setattr(admission.threading, 'get_ident', lambda: 11)
    pool.pressure_reason('pressure')
    assert pool.pressure_victim == 12
    class Guard:
        limit_bytes, peak_bytes = GIB, 2 * GIB
        def check(self, pid):
            return 'own hard cap'
    stop = pool.memory_stop(Guard(), 100)
    assert stop.cause == 'execution_limit' and stop.reason == 'own hard cap'
    assert pool.pressure_victim == 12


def test_newest_unspawned_lease_acknowledges_before_creating_process(tmp_path, monkeypatch):
    pool = scheduler(tmp_path, available=lambda: 7 * GIB)
    pool.active = {11: record(1, 100), 12: record(2, 200)}
    pool.active[12]['processes'].clear()
    thread = [11]
    monkeypatch.setattr(admission.threading, 'get_ident', lambda: thread[0])
    assert pool.pressure_reason('older reporter') is None
    assert pool.pressure_victim == 12
    thread[0] = 12
    with pytest.raises(memory_retry.MemoryStopped) as stopped:
        pool.process_command(['never-spawn'])
    assert stopped.value.cause == 'host_pressure'


def test_admission_order_is_fresh_on_reused_warm_lease_and_cleanup_fences(tmp_path):
    pool = scheduler(tmp_path)
    with pool.acquire('warm'):
        first = pool.active[threading.get_ident()]['admission_sequence']
        pool.retain('warm', lambda: None)
    with pool.acquire('warm'):
        rec = pool.active[threading.get_ident()]
        assert rec['admission_sequence'] > first
        rec['evict'] = None
        pool.pressure_victim = threading.get_ident()
        class Group:
            def close(self):
                assert pool.pressure_victim is not None
                assert not pool._can_admit('other', 0, 0)[0]
        rec['cgroup'] = Group()
    assert pool.pressure_victim is None and not pool.active


def test_warm_completion_linearizes_before_victim_selection(tmp_path, monkeypatch):
    pool = scheduler(tmp_path, available=lambda: 7 * GIB)
    pool.active = {11: record(1, 100), 12: record(2, 200)}
    thread = [12]
    monkeypatch.setattr(admission.threading, 'get_ident', lambda: thread[0])
    pool.complete_execution()
    thread[0] = 11
    assert pool.pressure_reason('pressure')
    assert pool.pressure_victim == 11  # Newer result already committed to completion.
    pool.pressure_victim = None
    pool.active[11].pop('pressure_stop')
    pool.active[12]['completed'] = False
    assert pool.pressure_reason('pressure') is None
    thread[0] = 12
    with pytest.raises(memory_retry.MemoryStopped):
        pool.complete_execution()  # Selected first: no success delivery.


def test_configured_float_maximum_is_serialized_as_integer(tmp_path):
    pool = scheduler(tmp_path)
    pool.maximum_job_bytes = float(48 * GIB)
    state = memory_retry.RetryState(pool, 'f' * 64, 'cold_compiler', pool.maximum_job_bytes)
    state.begin()
    state.stopped(memory_retry.MemoryStopped('cgroup_oom', 49 * GIB, 'hard cap'), 1)
    with pytest.raises(memory_retry.ResourceOutcome) as caught:
        state.remaining(10)
    assert type(caught.value.detail['maximum_job_bytes']) is int
    assert caught.value.detail['attempted_limit_bytes'] == [48 * GIB]


def test_hard_limit_evidence_dominates_concurrent_pressure_without_erasing_it(tmp_path):
    from trellis.checker import resource_failure
    pool = scheduler(tmp_path)
    resource_failure.memory_phase(tmp_path, 'candidate', 'a' * 64)
    pressure = memory_retry.RetryState(pool, '1' * 64, 'axioms', GIB)
    pressure.begin()
    pressure.stopped(memory_retry.MemoryStopped('host_pressure', GIB, 'newest'), 1)
    resource_failure.record_capacity(tmp_path, pressure.outcome('host_pressure_requeue', 'deadline').detail)
    first = resource_failure.capacity_record(tmp_path)
    hard = memory_retry.RetryState(pool, '2' * 64, 'kernel_replay', GIB)
    hard.begin()
    hard.stopped(memory_retry.MemoryStopped('cgroup_oom', 49 * GIB, 'hard cap'), 1)
    resource_failure.record_capacity(tmp_path, hard.outcome('resource_rejected', 'hard_limit_attempts_exhausted').detail)
    assert resource_failure.capacity_record(tmp_path)['kind'] == 'resource_rejected'
    retained = [json.loads(p.read_text()) for p in (tmp_path / 'checker-state/memory-outcomes').glob('*.json')]
    assert first in retained
    assert {r['kind'] for r in retained} == {'resource_rejected', 'host_pressure_requeue'}


@pytest.mark.parametrize('order', ['hard_first', 'concurrent'])
def test_hard_limit_arbitration_keeps_both_leaf_records(tmp_path, order):
    from concurrent.futures import ThreadPoolExecutor
    from trellis.checker import resource_failure
    pool = scheduler(tmp_path)
    resource_failure.memory_phase(tmp_path, 'baseline', 'b' * 64)
    details = []
    for key, cause, kind in [('3', 'cgroup_oom', 'resource_rejected'),
                              ('4', 'host_pressure', 'host_pressure_requeue')]:
        state = memory_retry.RetryState(pool, key * 64, 'kernel_replay', GIB)
        state.begin()
        state.stopped(memory_retry.MemoryStopped(cause, GIB, cause), 1)
        details.append(state.outcome(kind, 'fixture').detail)
    if order == 'concurrent':
        barrier = threading.Barrier(2)
        def write(detail):
            barrier.wait(2)
            resource_failure.record_capacity(tmp_path, detail)
        with ThreadPoolExecutor(2) as executor:
            list(executor.map(write, details))
    else:
        for detail in details:
            resource_failure.record_capacity(tmp_path, detail)
    selected = resource_failure.capacity_record(tmp_path)
    assert selected['kind'] == 'resource_rejected'
    retained = [json.loads(p.read_text()) for p in (tmp_path / 'checker-state/memory-outcomes').glob('*.json')]
    assert {r['detail']['input_key'] for r in retained} == {'3' * 64, '4' * 64}
    assert all(r['phase'] == selected['phase'] for r in retained)


def test_cold_authoritative_pressure_unwinds_to_stage_without_inner_writer_retry(tmp_path, monkeypatch):
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    seen, outcomes = [], []
    @admission.admitted
    def _run_lake_command(*args, **kw):
        lease = pool.active[threading.get_ident()]
        assert lease['controlled'] and lease['limit_bytes'] == 48 * GIB
        seen.append(1)
        raise memory_retry.MemoryStopped('host_pressure', GIB, 'newest')
    def outcome(value):
        assert not pool.active
        outcomes.append(value)
    with admission.authoritative_workspace('sealed', capacity_failure=outcome):
        with pytest.raises(ProtocolError) as caught:
            _run_lake_command(tmp_path, ['build'], timeout_secs=10, bwrap_role='lake_compiler')
    assert caught.value.kind == 'host_pressure_requeue' and seen == [1]
    assert outcomes[0]['attempted_limit_bytes'] == []
    assert outcomes[0]['pressure_requeues'] == 1 and outcomes[0]['drained']


def test_ungranted_cold_rpc_reprepares_whole_request_without_sticky_failure(tmp_path, monkeypatch):
    from types import SimpleNamespace
    from trellis.checker.server import CheckerServer
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    build = tmp_path / '.lake/build'
    calls, failures = [], []
    request = object()
    @admission.admitted
    def _run_lake_command(*args, **kw):
        assert not build.exists()
        calls.append(1)
        if len(calls) == 1:
            build.mkdir(parents=True)
            (build / 'partial.olean').write_text('incomplete')
            raise memory_retry.MemoryStopped('host_pressure', GIB, 'newest')
        return dict(returncode=0)
    def once(value):
        assert value is request
        return _run_lake_command(tmp_path, ['build'], timeout_secs=10, bwrap_role='lake_compiler')
    workspace = SimpleNamespace(supervisor_repo=tmp_path, _stop_event=threading.Event(),
        _workspace_lock=threading.RLock(), _sync_lock=threading.RLock(), _handle_request_once=once)
    with admission.authoritative_workspace('ungranted', resource_failure=failures.append):
        assert CheckerServer._handle_request(workspace, request) == dict(returncode=0)
    assert len(calls) == 2 and not failures and not pool.active


def test_ungranted_hard_limit_remains_explicit_infrastructure_without_candidate_grant(tmp_path, monkeypatch):
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    failures = []
    @admission.admitted
    def _run_lake_command(*args, **kw):
        raise memory_retry.MemoryStopped('cgroup_oom', 49 * GIB, 'hard cap')
    with admission.authoritative_workspace('ungranted', resource_failure=failures.append):
        with pytest.raises(ProtocolError) as caught:
            _run_lake_command(tmp_path, ['build'], timeout_secs=10, bwrap_role='lake_compiler')
    assert caught.value.kind == 'resource_exhausted' and failures


@pytest.mark.parametrize('failure', ['cancel', 'stop', 'wrong_repo', 'symlink'])
def test_rpc_reprepare_refuses_revoked_stopping_or_foreign_workspace(tmp_path, monkeypatch, failure):
    from types import SimpleNamespace
    from trellis.checker.server import CheckerServer
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    cancelled, calls = [], []
    outside = tmp_path / 'outside'
    outside.mkdir()
    protected = outside / 'keep'
    protected.write_text('untouched')
    stop = threading.Event()
    def once(request):
        calls.append(request)
        if failure == 'cancel':
            cancelled.append(True)
        if failure == 'stop':
            stop.set()
        if failure == 'symlink':
            (tmp_path / '.lake').mkdir()
            (tmp_path / '.lake/build').symlink_to(outside, target_is_directory=True)
        raise admission.ReprepareMemory(outside if failure == 'wrong_repo' else tmp_path, {})
    def cancel():
        if cancelled:
            raise ProtocolError('workspace_revoked', 'revoked')
    workspace = SimpleNamespace(supervisor_repo=tmp_path, _stop_event=stop,
        _workspace_lock=threading.RLock(), _sync_lock=threading.RLock(), _handle_request_once=once)
    with admission.authoritative_workspace('ungranted', cancelled=cancel):
        with pytest.raises(ProtocolError):
            CheckerServer._handle_request(workspace, 'original')
    assert calls == ['original'] and protected.read_text() == 'untouched'
    assert not pool.active


def test_real_request_reprepare_resynchronizes_changed_source_and_discards_old_artifacts(tmp_path, monkeypatch):
    from test_local_closure_cache_prefilter import make_server
    from trellis.checker.protocol import validate_request
    from trellis.atomic_actions import observations
    server = make_server(tmp_path)
    worker = tmp_path / 'worker'
    (worker / 'Tablet').mkdir(parents=True)
    for source in (server.supervisor_repo / 'Tablet').iterdir():
        (worker / 'Tablet' / source.name).write_bytes(source.read_bytes())
    server.worker_repo = worker
    monkeypatch.setattr(admission, 'ACTIVE', scheduler(tmp_path))
    # Retain real synchronization, request gates and source/artifact validation;
    # the external operation is the only injected pressure boundary.
    monkeypatch.setattr(server, '_try_cache_hit_only', lambda *a: None)
    calls = []
    changed = worker / 'Tablet/Root.lean'
    original = changed.read_text()
    def dispatch(request, sync, **kw):
        calls.append(sync)
        if len(calls) == 1:
            assert observations._tablet_olean_path(server.supervisor_repo, 'Root').exists()
            changed.write_text(original + '-- updated while pressure victim drained\n')
            raise admission.ReprepareMemory(server.supervisor_repo, {})
        assert 'Root.lean' in sync['changed']
        assert (server.supervisor_repo / 'Tablet/Root.lean').read_text() == changed.read_text()
        assert not observations._tablet_olean_path(server.supervisor_repo, 'Root').exists()
        return {'returncode': 0}, 0, {}
    monkeypatch.setattr(server, '_dispatch_op', dispatch)
    request = validate_request(dict(op='verify_node', request_id=1, node_name='Root'))
    assert server._handle_request(request) == {'returncode': 0}
    assert len(calls) == 2


def test_response_enrichment_pressure_does_not_replay_drained_measurement_spool(tmp_path, monkeypatch):
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    calls, outcomes = [], []
    def enrich():
        calls.append('drained spool')
        raise memory_retry.MemoryStopped('host_pressure', GIB, 'newest')
    enrich.__module__ = 'trellis.checker.host_response'
    with admission.authoritative_workspace('sealed', capacity_failure=outcomes.append):
        with pytest.raises(ProtocolError) as caught:
            admission.admitted(enrich)()
    assert caught.value.kind == 'host_pressure_requeue'
    assert calls == ['drained spool'] and outcomes[0]['drained']
    assert serial_recovery.is_capacity_wait(outcomes[0])
    assert not pool.active


def test_helper_admission_timeout_is_capacity_wait(tmp_path, monkeypatch):
    pool = scheduler(tmp_path, available=lambda: GIB)
    pool.serial_wait_seconds = .01
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    outcomes = []
    def enrich():
        pytest.fail('helper must wait for capacity before consuming inputs')
    enrich.__module__ = 'trellis.checker.host_response'
    with admission.authoritative_workspace('sealed', capacity_failure=outcomes.append):
        with pytest.raises(ProtocolError):
            admission.admitted(enrich)()
    assert serial_recovery.is_capacity_wait(outcomes[0])
    assert outcomes[0]['exhausted_reason'] == 'helper_admission_deadline'
    assert outcomes[0]['launch_count'] == 0 and not pool.active


def test_optional_heartbeat_pressure_is_unavailable_without_reusing_partial_output(tmp_path, monkeypatch):
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    calls = []
    def _run_pass():
        calls.append(1)
        raise memory_retry.MemoryStopped('host_pressure', GIB, 'newest')
    _run_pass.__module__ = 'trellis.checker.heartbeat'
    assert admission.admitted(_run_pass)() == (None, 'newest')
    assert calls == [1] and not pool.active


def test_prepared_job_exhausts_shared_and_serial_caps_before_resource_reason(tmp_path, monkeypatch):
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    seen, outcomes = [], []
    @admission.admitted
    def _run_lake_command(*args, **kw):
        seen.append(pool.active[threading.get_ident()]['limit_bytes'])
        raise memory_retry.MemoryStopped('cgroup_oom', 49 * GIB, 'limit')
    with admission.authoritative_workspace('sealed', capacity_failure=outcomes.append):
        with admission.prepared_readonly('axioms'), pytest.raises(ProtocolError) as caught:
            _run_lake_command(tmp_path, ['env', 'lean'], timeout_secs=10, bwrap_role='lake_compiler')
    assert caught.value.kind == 'resource_rejected'
    assert seen == outcomes[0]['attempted_limit_bytes'] == [48 * GIB, 56 * GIB]
    assert not pool.active


def test_atomic_helper_restarts_after_cleanup_only_for_pressure(tmp_path, monkeypatch):
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    monkeypatch.setattr(memory_retry.RetryState, 'wait_pressure', lambda state, cancel: cancel())
    path = tmp_path / 'partial'
    seen = []
    def _compile(directory, module, runner, source, lean, inputs, timeout_secs):
        assert not path.exists()
        seen.append(1)
        path.write_text('partial')
        try:
            if len(seen) == 1:
                raise memory_retry.MemoryStopped('host_pressure', GIB, 'newest')
            return 'complete'
        finally:
            path.unlink()
    _compile.__module__ = 'trellis.checker.compiled_probe'
    assert admission.admitted(_compile)(None, 'Collector', b'', None, None, {}, 20) == 'complete' and seen == [1, 1]
    assert not path.exists() and not pool.active


def test_cold_advisory_requeue_clears_partial_build_and_preserves_captured_job(tmp_path, monkeypatch):
    from test_check_jobs import setup_project as setup, request, wait_done
    repo, runtime, token, service = setup(tmp_path)
    service.scheduler = scheduler(tmp_path)
    monkeypatch.setattr(service, '_admission_inputs', lambda *args: ('advisory:scratch', None))
    monkeypatch.setattr(memory_retry.RetryState, 'pressure_delay', lambda _: .001)
    seen = []
    def execute(spec, job):
        seen.append((job['id'], job['snapshot']))
        key = hashlib.sha256(spec['repo'].encode()).hexdigest()
        build = service.root / 'workspaces' / key / 'repo/.lake/build'
        assert not build.exists()
        if len(seen) == 1:
            build.mkdir(parents=True)
            (build / 'partial.olean').write_text('unproven')
            raise memory_retry.MemoryStopped('host_pressure', GIB, 'newest')
        return dict(returncode=0, stdout='', stderr='')
    service.execute_override = execute
    service.worker.start()
    try:
        result = service.handle(request(token))
        done = wait_done(service, token, result)
        assert done['state'] == 'succeeded'
        assert seen[0] == seen[1]
        assert done['result']['memory_pressure_requeues'] == 1
        assert done['result']['memory_hard_attempt_count'] == 0
    finally:
        service.stop.set(); service.wake.set(); service.worker.join(3)


def test_advisory_capacity_timeout_requeues_same_job_then_completes(tmp_path):
    from test_check_jobs import setup_project, request
    seen = []
    def execute(spec, job):
        seen.append((job['id'], job['snapshot']))
        return dict(returncode=0, stdout='checked', stderr='')
    repo, runtime, token, service = setup_project(tmp_path, execute)
    available = [GIB]
    service.scheduler = scheduler(tmp_path, available=lambda: available[0])
    service.scheduler.serial_wait_seconds = .01
    submitted = service.handle(request(token))
    original = dict(service.db.execute('SELECT * FROM jobs').fetchone())
    service.worker.start()
    try:
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            with service.lock:
                row = dict(service.db.execute('SELECT * FROM jobs').fetchone())
            detail = json.loads(row['result'] or '{}').get('memory_retry', {})
            if detail.get('capacity_wait'):
                break
            time.sleep(.01)
        else:
            pytest.fail('admission timeout did not return the job to the queue')
        assert row['state'] == 'queued' and seen == []
        assert detail['launch_count'] == 0
        assert detail['retry_not_before'] > time.time()
        assert not service.scheduler.active
        service.scheduler.serial_wait_seconds = 30
        available[0] = 10 * GIB
        deadline = time.monotonic() + 7
        while time.monotonic() < deadline:
            result = service.handle(dict(action='status', token=token, job_id=submitted['job_id']))
            if result['state'] not in {'queued', 'running'}:
                break
            time.sleep(.01)
        assert result['state'] == 'succeeded', result
        assert seen == [(original['id'], original['snapshot'])]
        assert result['result']['memory_attempt_count'] == 1
        assert result['result']['memory_hard_attempt_count'] == 0
    finally:
        service.stop.set(); service.wake.set(); service.worker.join(3)
