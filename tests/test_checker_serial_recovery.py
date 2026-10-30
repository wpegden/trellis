import json
from pathlib import Path
import shutil
import threading
import time

import pytest

from trellis.checker import admission, memory_retry, serial_recovery
from trellis.checker.protocol import ProtocolError

GIB = 2**30


def pool(tmp_path, available=lambda: 60 * GIB):
    return admission.Admission(6, available=available, floor_bytes=8 * GIB,
        reserve_bytes=24 * GIB, optimistic=True, available_memory=True,
        retry_root=tmp_path / 'retries')


def retry(scheduler, key='a' * 64):
    state = serial_recovery.SerialRetry(scheduler, key, 'kernel_replay', 48 * GIB)
    state.initialize(20, targets=['Tablet.A', 'Tablet.B'])
    return state


def stop(state, cause='cgroup_oom', elapsed=1):
    with state.scheduler.acquire('test', **state.lease_options()):
        state.begin()
    state.stopped(memory_retry.MemoryStopped(cause, 48 * GIB, cause,
                  cgroup_peak_bytes=state.data['limit_bytes']), elapsed)


def test_serial_ceiling_is_not_a_free_memory_reservation(tmp_path, monkeypatch):
    now = [1000.0]
    monkeypatch.setattr(serial_recovery.time, 'time', lambda: now[0])
    available = [53.56 * GIB]
    scheduler = pool(tmp_path, lambda: available[0])
    state = retry(scheduler)
    stop(state)
    assert state.admission()[0]
    assert state.data['limit_bytes'] == 56 * GIB
    assert state.data['serial_admission'] == 'available_headroom'
    available[0] = 61 * GIB
    stop(state, 'host_pressure')
    assert state.admission()[0] and state.data['limit_bytes'] == 56 * GIB


def test_full_cap_can_admit_on_62_gib_host(tmp_path, monkeypatch):
    scheduler = pool(tmp_path)
    state = retry(scheduler)
    stop(state)
    assert state.admission()[0]
    assert state.data['limit_bytes'] == 56 * GIB
    assert state.data['serial_admission'] == 'available_headroom'
    # No extra 24 GiB reservation or duplicate 8 GiB floor.
    with scheduler.acquire('test', **state.lease_options()):
        assert scheduler.execution_floor() == 4 * GIB
        assert scheduler.lean_threads() == 1
    assert scheduler.execution_floor() == 8 * GIB
    monkeypatch.setenv('TRELLIS_CHECK_SERIAL_MAX_JOB_GIB', '56')
    original = Path.read_text
    monkeypatch.setattr(Path, 'read_text', lambda p, *a, **kw:
        f'MemTotal: {int(62.66 * 2**20)} kB\n' if str(p) == '/proc/meminfo' else original(p, *a, **kw))
    assert admission._configured_maximum(4 * GIB, 'TRELLIS_CHECK_SERIAL_MAX_JOB_GIB', '56') == 56 * GIB


def test_enclosing_cgroup_allowance_is_respected_without_mutation(tmp_path, monkeypatch):
    from trellis.checker.compiler_cgroup import CompilerGroups
    root = tmp_path / 'root'
    jobs = root / 'compilers'
    jobs.mkdir(parents=True)
    (root / 'memory.max').write_text(str(60 * GIB))
    (root / 'memory.current').write_text(str(5 * GIB))
    (jobs / 'memory.max').write_text('max')
    group = CompilerGroups.__new__(CompilerGroups)
    group.jobs = jobs
    assert group.headroom() == 55 * GIB
    scheduler = pool(tmp_path)
    state = retry(scheduler)
    stop(state)
    scheduler.groups = group
    assert state.admission()[0] and state.data['limit_bytes'] == 51 * GIB
    assert (root / 'memory.max').read_text() == str(60 * GIB)
    (root / 'memory.current').write_text(str(20 * GIB))
    assert not state.admission()[0], 'temporary parent load must not lower the retry below its failed cap'
    assert not state.data.get('serial_exhausted')
    (root / 'memory.current').write_text(str(5 * GIB))
    assert state.admission()[0]


def test_runtime_floor_uses_serial_policy_and_pressure_is_not_hard_exhaustion(tmp_path):
    available = [60 * GIB]
    scheduler = pool(tmp_path, lambda: available[0])
    state = retry(scheduler)
    stop(state)
    class Guard:
        limit_bytes, peak_bytes = 0, 53 * GIB
        def check(self, pid): return None
    with scheduler.acquire('test', **state.lease_options()):
        state.begin()
        available[0] = 5 * GIB
        assert scheduler.memory_stop(Guard(), 1) is None
        available[0] = 3 * GIB
        failure = scheduler.memory_stop(Guard(), 1)
        assert failure.cause == 'host_pressure'
        assert '4.0 GiB floor' in failure.reason
    state.stopped(failure, 1)
    assert not state.data.get('serial_exhausted')
    available[0] = 60 * GIB
    assert state.admission()[0]


def test_pressure_retries_preserve_execution_allowance_and_bound_history(tmp_path):
    scheduler = pool(tmp_path)
    state = retry(scheduler)
    stop(state, 'host_pressure')
    for index in range(9):
        state = retry(scheduler)
        stop(state, 'host_pressure')
        assert state.data['serial_launches'] == index + 1
        assert state.data['attempted_limit_bytes'] == []
    assert retry(scheduler).remaining(20) == 20
    assert len(state.data['attempts']) == state.MAX_RECORDED_ATTEMPTS
    assert state.data['omitted_attempts'] == 6
    assert len(state.path.read_bytes()) < 16384


def test_own_serial_cap_exhaustion_is_durable_and_keeps_attempt_evidence(tmp_path):
    state = retry(pool(tmp_path))
    stop(state)
    stop(state)
    with pytest.raises(memory_retry.ResourceOutcome) as error:
        retry(pool(tmp_path)).remaining(20)
    detail = error.value.detail
    assert detail['kind'] == 'resource_rejected'
    assert detail['attempted_limit_bytes'] == [48 * GIB, 56 * GIB]
    assert [a['serial'] for a in detail['attempts']] == [False, True]
    assert 'No proof invalidity was established' in str(error.value)
    assert 'Splitting files alone' in str(error.value)


@pytest.mark.parametrize('condition', ['execution', 'crash'])
def test_unavailable_recovery_never_rejects_candidate(tmp_path, condition):
    available = [60 * GIB]
    state = retry(pool(tmp_path, lambda: available[0]))
    stop(state)
    if condition == 'execution':
        state.data['hard_execution_seconds'] = 20
    else:
        with state.scheduler.acquire('test', **state.lease_options()):
            state.begin()  # No result persisted before a checker restart.
        state = retry(pool(tmp_path))
    with pytest.raises(memory_retry.ResourceOutcome) as error:
        state.admission()
    assert error.value.detail['kind'] == 'host_pressure_requeue'
    assert not state.data.get('serial_exhausted')


def test_low_headroom_wait_can_resume_without_refilling_executed_budget(tmp_path):
    available = [60 * GIB]
    scheduler = pool(tmp_path, lambda: available[0])
    state = retry(scheduler)
    stop(state)
    available[0] = 3 * GIB
    assert not state.admission()[0]
    state.data['admission_deadline'] = time.time() - 1
    with pytest.raises(memory_retry.ResourceOutcome) as error:
        state.check_wait()
    assert serial_recovery.is_capacity_wait(error.value.detail)
    assert 'terminal_reason' not in state.data
    resumed = retry(scheduler)
    assert resumed.remaining(20) == 19
    assert not resumed.admission()[0]
    available[0] = 10 * GIB
    assert resumed.admission()[0]
    assert resumed.data['attempted_limit_bytes'] == [48 * GIB]


@pytest.mark.parametrize('reason', ['serial_headroom_unavailable', 'serial_pressure_launches_exhausted',
                                  'serial_admission_deadline', 'recovery_execution_deadline'])
def test_old_capacity_terminal_record_resumes_without_refilling_budget(tmp_path, reason):
    scheduler = pool(tmp_path)
    state = retry(scheduler)
    stop(state, elapsed=2)
    state.data.update(version=3, terminal_reason=reason, deadline=time.time() - 1000,
                      admission_deadline=time.time() - 1000)
    state.save()
    resumed = retry(scheduler)
    assert resumed.remaining(20) == 18
    assert resumed.admission()[0]
    assert resumed.data['attempted_limit_bytes'] == [48 * GIB]
    assert resumed.data['version'] == 5
    assert 'terminal_reason' not in resumed.data


def test_exclusive_waiter_drains_only_owned_sessions_and_blocks_new_work(tmp_path):
    scheduler = pool(tmp_path)
    state = retry(scheduler)
    stop(state)
    order, errors = [], []
    resident = dict(owner='idle', priority=1, processes=set(), evict=lambda: order.append('evicted'))
    scheduler.residents['idle'] = resident
    entered = threading.Event()
    def recover():
        try:
            entered.set()
            with scheduler.acquire('recovery', priority=1, **state.lease_options()):
                assert not scheduler.residents
                order.append('serial')
                time.sleep(.02)
        except BaseException as exc: errors.append(exc)
    def later():
        try:
            with scheduler.acquire('later', priority=0): order.append('later')
        except BaseException as exc: errors.append(exc)
    with scheduler.acquire('existing'):
        t = threading.Thread(target=recover); t.start()
        assert entered.wait(1)
        deadline = time.monotonic() + 2
        while not scheduler.waiters and time.monotonic() < deadline: time.sleep(.001)
        assert scheduler.waiters
        u = threading.Thread(target=later); u.start()
        assert 'serial' not in order
    t.join(3); u.join(3)
    assert not t.is_alive() and not u.is_alive() and not errors
    assert order == ['evicted', 'serial', 'later']


def test_authoritative_recovery_survives_new_workspace_and_changed_inputs_get_new_budget(tmp_path, monkeypatch):
    scheduler = pool(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', scheduler)
    repo = tmp_path / 'first'
    (repo / 'Tablet').mkdir(parents=True)
    (repo / 'Tablet/A.lean').write_text('theorem a : True := by trivial')
    calls, outcomes = [], []
    @admission.admitted
    def _run_lake_command(repo, args, *, timeout_secs, bwrap_role):
        calls.append((scheduler.execution_floor(), scheduler.lean_threads()))
        raise memory_retry.MemoryStopped('cgroup_oom', 48 * GIB, 'own cap')
    def run(repo, owner):
        with admission.authoritative_workspace(owner, capacity_failure=outcomes.append):
            with admission.compiler_operation('kernel_replay'), pytest.raises(ProtocolError) as error:
                _run_lake_command(repo, ['env', 'leanchecker', 'Tablet.A'], timeout_secs=20, bwrap_role='lake_compiler')
        assert error.value.kind == 'resource_rejected'
        assert not scheduler.active
    run(repo, 'first-round')
    assert calls == [(8 * GIB, 6), (4 * GIB, 1)]
    fresh = tmp_path / 'second'
    shutil.copytree(repo, fresh)
    run(fresh, 'new-round')
    assert len(calls) == 2
    (fresh / 'Tablet/A.lean').write_text('theorem a : True := by constructor')
    run(fresh, 'new-submission')
    assert len(calls) == 4


def test_cancellation_during_serial_admission_does_not_spend_launch(tmp_path):
    available = [60 * GIB]
    state = retry(pool(tmp_path, lambda: available[0]))
    stop(state)
    available[0] = 56 * GIB
    def cancel(): raise RuntimeError('revoked')
    with pytest.raises(RuntimeError, match='revoked'):
        with state.scheduler.acquire('test', cancelled=cancel, **state.lease_options()):
            pytest.fail('must not launch')
    assert state.data['serial_launches'] == 0 and not state.scheduler.waiters


def test_managed_job_uses_same_snapshot_for_serial_recovery(tmp_path, monkeypatch):
    from test_check_jobs import setup_project, request, wait_done
    repo, runtime, token, service = setup_project(tmp_path)
    service.scheduler = pool(tmp_path)
    calls = []
    def execute(spec, job):
        calls.append((job['snapshot'], service.scheduler.lean_threads()))
        if len(calls) == 1:
            raise memory_retry.MemoryStopped('cgroup_oom', 48 * GIB, 'own cap')
        return dict(returncode=0, stdout='', stderr='')
    service.execute_override = execute
    service.worker.start()
    try:
        done = wait_done(service, token, service.handle(request(token)))
        assert done['state'] == 'succeeded', done
        assert calls[0][0] == calls[1][0] and [c[1] for c in calls] == [6, 1]
        assert done['result']['execution_limit_bytes'] == 56 * GIB
    finally:
        service.stop.set(); service.wake.set(); service.worker.join(3)


def test_real_command_recovery_applies_thread_and_guard_settings(tmp_path, monkeypatch):
    """Small real subprocesses; inject only the first memory-stop observation."""
    from trellis.atomic_actions import observations
    scheduler = pool(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', scheduler)
    monkeypatch.setenv('TRELLIS_LEAN_PARALLELISM', '6')
    monkeypatch.setattr(observations, 'authoritative_env_for_repo', lambda repo: {})
    seen = []
    def memory_stop(guard, pid):
        seen.append((guard.floor_bytes, scheduler.lean_threads()))
        if scheduler.lean_threads() == 6:
            return memory_retry.MemoryStopped('cgroup_oom', 48 * GIB, 'injected local OOM')
    monkeypatch.setattr(scheduler, 'memory_stop', memory_stop)
    with admission.compiler_operation('kernel_replay'):
        result = observations._run_lake_command(tmp_path,
            ['-c', 'printf "%s" "$LEAN_NUM_THREADS"'], timeout_secs=5, lake_executable=Path('/bin/sh'))
    assert result['returncode'] == 0 and result['stdout'] == '1'
    assert seen == [(8 * GIB, 6), (4 * GIB, 1)]
    assert not scheduler.active and not scheduler.cleanup_failed


def test_serial_headroom_rechecked_before_first_spawn(tmp_path):
    available = [60 * GIB]
    state = retry(pool(tmp_path, lambda: available[0]))
    stop(state)
    with state.scheduler.acquire('test', **state.lease_options()):
        state.begin()
        available[0] = 3 * GIB
        with pytest.raises(memory_retry.MemoryStopped) as error:
            state.scheduler.process_command(['never-spawn'])
        assert error.value.cause == 'host_pressure'
    state.stopped(error.value, 0)
    assert not state.data.get('serial_exhausted')


def test_bwrap_thread_override_and_cleanup_after_serial_warm_session(tmp_path, monkeypatch):
    from trellis.checker import job_workspace
    from trellis import sandbox
    monkeypatch.setattr(sandbox, 'wrap_command', lambda args, **kw:
                        ['bwrap', '--chdir', str(kw['work_dir']), *args])
    repo = tmp_path / 'repo'; repo.mkdir()
    command = job_workspace.compiler_command(repo, ['lean', '--server'], lean_threads='1')
    assert command[command.index('LEAN_NUM_THREADS') + 1] == '1'
    state = retry(pool(tmp_path))
    stop(state)
    cleaned = []
    with state.scheduler.acquire('test', **state.lease_options()):
        state.scheduler.retain('test', lambda: cleaned.append(True))
    assert cleaned == [True] and not state.scheduler.residents


def test_cleanup_failure_fences_further_admission(tmp_path):
    state = retry(pool(tmp_path))
    stop(state)
    def fail(): raise RuntimeError('cannot reap')
    with pytest.raises(RuntimeError, match='cannot reap'):
        with state.scheduler.acquire('test', **state.lease_options()):
            state.scheduler.retain('test', fail)
    assert state.scheduler.cleanup_failed
    with pytest.raises(OSError, match='cleanup incomplete'):
        state.scheduler._can_admit('other', 0, 0)


def test_authoritative_cold_repreparation_keeps_pressure_trials_inside_recovery(tmp_path, monkeypatch):
    from types import SimpleNamespace
    from trellis.checker.server import CheckerServer
    scheduler = pool(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', scheduler)
    build = tmp_path / '.lake/build'
    calls, outcomes = [], []
    @admission.admitted
    def _run_lake_command(repo, args, *, timeout_secs, bwrap_role):
        assert not build.exists()
        calls.append(scheduler.lean_threads())
        if len(calls) <= 3:
            build.mkdir(parents=True)
            (build / 'partial.olean').write_text('incomplete')
            raise memory_retry.MemoryStopped('cgroup_oom' if len(calls) == 1 else 'host_pressure',
                                            48 * GIB, 'injected stop')
        return dict(returncode=0)
    def once(request):
        return _run_lake_command(tmp_path, ['build', 'Tablet.A'], timeout_secs=20, bwrap_role='lake_compiler')
    workspace = SimpleNamespace(supervisor_repo=tmp_path, _stop_event=threading.Event(),
        _workspace_lock=threading.RLock(), _sync_lock=threading.RLock(), _handle_request_once=once)
    with admission.authoritative_workspace('sealed', capacity_failure=outcomes.append):
        assert CheckerServer._handle_request(workspace, object()) == dict(returncode=0)
    assert calls == [6, 1, 1, 1] and outcomes == []


def test_matching_input_wait_is_bounded_without_overwriting_owner_ledger(tmp_path):
    scheduler = pool(tmp_path)
    scheduler.serial_wait_seconds = .01
    state = retry(scheduler)
    before = state.path.read_bytes()
    lock = scheduler.retry_lock(state.key)
    with lock, pytest.raises(memory_retry.ResourceOutcome) as error:
        serial_recovery.acquire_lock(scheduler, state.key, state.operation, 20, lambda: None)
    assert error.value.detail['kind'] == 'host_pressure_requeue'
    assert error.value.detail['exhausted_reason'] == 'matching_input_admission_deadline'
    assert state.path.read_bytes() == before


@pytest.mark.parametrize('code', [1, -9])
def test_exit_code_alone_never_requests_memory_escalation(tmp_path, monkeypatch, code):
    scheduler = pool(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', scheduler)
    calls, outcomes = [], []
    @admission.admitted
    def _run_lake_command(repo, args, *, timeout_secs):
        calls.append(scheduler.lean_threads())
        return dict(returncode=code, stderr='out of memory')
    with admission.authoritative_workspace('test', capacity_failure=outcomes.append):
        assert _run_lake_command(tmp_path, ['build'], timeout_secs=2)['returncode'] == code
    assert calls == [6] and outcomes == []


def test_cancellation_cannot_select_or_evict_another_runtime(tmp_path):
    own, other = pool(tmp_path / 'own'), pool(tmp_path / 'other')
    untouched = []
    other.residents['other-runtime'] = dict(evict=lambda: untouched.append(False))
    state = retry(own)
    stop(state)
    with own.acquire('test', **state.lease_options()):
        assert not own.residents
    assert list(other.residents) == ['other-runtime'] and untouched == []


def test_successful_ordinary_probes_do_not_fill_failure_history(tmp_path):
    state = retry(pool(tmp_path))
    with state.scheduler.acquire('test', **state.lease_options()):
        state.begin()
    assert state.path.exists()
    state.succeeded()
    assert not state.path.exists() and not state.path.with_suffix('.targets.json').exists()


def test_successful_recovery_retains_measured_peaks(tmp_path):
    scheduler = pool(tmp_path)
    state = retry(scheduler)
    stop(state)
    class Guard:
        limit_bytes, peak_bytes = 0, 53 * GIB
        def check(self, pid): return None
    with scheduler.acquire('test', **state.lease_options()):
        state.begin()
        assert scheduler.memory_stop(Guard(), 1) is None
        scheduler.observe_peak(0, completed=True)
        scheduler.complete_execution()
    state.succeeded()
    saved = json.loads(state.path.read_text())
    assert saved['attempts'][-1]['sampled_peak_bytes'] == 53 * GIB
    assert saved['attempts'][-1]['outcome'] == 'completed'


def test_managed_serial_cleanup_failure_cannot_publish_success(tmp_path):
    from test_check_jobs import setup_project, request, wait_done
    repo, runtime, token, service = setup_project(tmp_path)
    service.scheduler = pool(tmp_path)
    def fail(): raise RuntimeError('serial cleanup failed')
    def execute(spec, job):
        if service.scheduler.lean_threads() == 6:
            raise memory_retry.MemoryStopped('cgroup_oom', 48 * GIB, 'first stop')
        service.scheduler.retain(job['owner'], fail)
        return dict(returncode=0, stdout='', stderr='')
    service.execute_override = execute
    service.worker.start()
    try:
        done = wait_done(service, token, service.handle(request(token)))
        assert done['state'] == 'infrastructure_error', done
        assert service.scheduler.cleanup_failed
        records = list((tmp_path / 'retries').glob('*/*.json'))
        states = [json.loads(p.read_text()) for p in records if not p.name.endswith('.targets.json')]
        assert len(states) == 1 and states[0]['in_flight'] and not states[0].get('completed')
    finally:
        service.stop.set(); service.wake.set(); service.worker.join(3)
