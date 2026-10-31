"""Controlled memory recovery cannot change proof or native retry outcomes."""
import json
from pathlib import Path
import subprocess
import threading

import pytest

from trellis.checker import admission, memory_retry
from trellis.checker.protocol import ProtocolError
from trellis.memory_guard import GIB, MemoryGuard


def scheduler(tmp_path, **kw):
    return admission.Admission(6, available=lambda: 50 * GIB, floor_bytes=8 * GIB,
        reserve_bytes=24 * GIB, readonly_reserve_bytes=14 * GIB, optimistic=True,
        retry_root=tmp_path / 'history', **kw)


def test_typical_estimates_admit_small_work_and_preserve_cold_tail(tmp_path):
    pool = scheduler(tmp_path)
    assert pool.estimate('axioms', readonly=True) == 8 * GIB
    pool.memory_samples['axioms'] = [4 * GIB] * 7 + [17 * GIB]
    assert pool.estimate('axioms', readonly=True) == 7 * GIB
    assert pool.estimate('kernel_replay') == 16 * GIB
    assert pool.estimate('advisory:incremental') == 24 * GIB
    with pool.acquire('cold', estimate_bytes=24 * GIB):
        assert pool._can_admit('small', 0, 8 * GIB)[0]
        assert not pool._can_admit('cold2', 0, 24 * GIB)[0]


def test_authoritative_retry_releases_lease_and_preserves_callback_boundary(tmp_path, monkeypatch):
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    failed, blocked, seen = [], [], []
    generations = []

    @admission.admitted
    def _run_lake_command(repo, args, **kwargs):
        lease = pool.active[threading.get_ident()]
        seen.append((lease['estimate'], lease['limit_bytes'], lease['depth']))
        generations.append(lease)
        assert not pool.pressure_victim
        if len(seen) == 1:
            assert pool.pressure_reason('low memory', limit=True) == 'low memory'
            raise memory_retry.MemoryStopped('host_pressure', 4 * GIB, 'low memory')
        assert generations[0] is not lease  # reacquired after full outer release
        return dict(returncode=0, stdout='checked')

    with admission.authoritative_workspace('sealed', resource_failure=failed.append, capacity_failure=blocked.append):
        with admission.prepared_readonly('axioms'):
            result = _run_lake_command(tmp_path, ['env', 'lean', 'probe'], timeout_secs=20, bwrap_role='lake_compiler')
    assert result == dict(returncode=0, stdout='checked')
    assert seen == [(8 * GIB, 24 * GIB, 1), (8 * GIB, 24 * GIB, 1)]
    assert not failed and not blocked and not pool.active and pool.pressure_victim is None
    saved = json.loads(next(pool.retry_root.glob('*/*.json')).read_text())
    assert saved['reservation_bytes'] == 8 * GIB and saved['attempt_count'] == 0


def test_exhausted_retry_is_durable_and_new_generation_is_explicit(tmp_path, monkeypatch):
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    blocked, calls = [], []

    @admission.admitted
    def _run_lake_command(repo, args, **kwargs):
        calls.append(admission.ACTIVE.active[threading.get_ident()]['limit_bytes'])
        raise memory_retry.MemoryStopped('cgroup_oom', 4 * GIB, 'kernel confirmed OOM')

    def run():
        with admission.authoritative_workspace('sealed', capacity_failure=blocked.append):
            with admission.prepared_readonly('axioms'):
                with pytest.raises(ProtocolError) as error:
                    _run_lake_command(tmp_path, ['env', 'lean', 'probe'], timeout_secs=20, bwrap_role='lake_compiler')
                assert error.value.kind == 'resource_rejected'
    run()
    assert calls == [24 * GIB, 48 * GIB] and blocked[-1]['drained'] is True
    assert blocked[-1]['attempted_limit_bytes'] == calls
    assert blocked[-1]['attempt_count'] == 2
    assert blocked[-1]['last_attempted_limit_bytes'] == 48 * GIB
    assert blocked[-1]['cause'] == 'cgroup_oom'
    restarted = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', restarted)
    run()
    assert len(calls) == 2  # restart cannot reset the exhausted exact workload
    restarted.generation = 'operator-change-1'
    run()
    assert len(calls) == 4


def test_computed_overshoot_tries_actual_host_maximum_before_rejection(tmp_path):
    pool = scheduler(tmp_path)
    state = memory_retry.RetryState(pool, 'a' * 64, 'axioms', 8 * GIB)
    state.begin()
    state.stopped(memory_retry.MemoryStopped('execution_limit', 45 * GIB, 'limit'), 1)
    assert state.data['reservation_bytes'] == 48 * GIB
    assert state.data['limit_bytes'] == 48 * GIB
    assert state.remaining(20) == 19
    assert state.data['attempted_limit_bytes'] == [24 * GIB]
    state.begin()
    state.stopped(memory_retry.MemoryStopped('execution_limit', 49 * GIB, 'limit'), 1)
    with pytest.raises(memory_retry.ResourceOutcome) as caught:
        state.remaining(20)
    assert caught.value.detail['attempted_limit_bytes'] == [24 * GIB, 48 * GIB]
    assert caught.value.detail['kind'] == 'resource_rejected'


@pytest.mark.parametrize('result', [dict(returncode=1, stderr='Lean error'), dict(returncode=-9),
    dict(returncode=1, stderr='out of memory'),
    dict(returncode=None, timed_out=True), dict(returncode=None, spawn_error='missing')])
def test_ordinary_failures_never_trigger_memory_retry(tmp_path, monkeypatch, result):
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    calls = []
    @admission.admitted
    def _run_lake_command(*args, **kwargs):
        calls.append(1)
        return result
    with admission.authoritative_workspace('sealed', capacity_failure=lambda _: pytest.fail('parked ordinary failure')):
        with admission.prepared_readonly('semantic'):
            assert _run_lake_command(tmp_path, ['env', 'lean'], timeout_secs=1, bwrap_role='lake_compiler') == result
    assert calls == [1]
    assert not list(tmp_path.rglob('*.json'))


def test_canonical_probe_without_capacity_adapter_stays_conservative(tmp_path, monkeypatch):
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    @admission.admitted
    def _run_lake_command(*args, **kwargs):
        lease = pool.active[threading.get_ident()]
        assert not lease['controlled'] and lease['estimate'] == 14 * GIB
        return dict(returncode=0)
    with admission.prepared_readonly('axioms'):
        _run_lake_command(tmp_path, ['env', 'lean'], timeout_secs=1, bwrap_role='lake_compiler')


def test_zero_peak_miss_remains_durable(tmp_path):
    pool = scheduler(tmp_path)
    state = memory_retry.RetryState(pool, 'a' * 64, 'axioms', 8 * GIB)
    state.begin()
    state.stopped(memory_retry.MemoryStopped('host_pressure', 0, 'floor'), .1)
    restored = memory_retry.RetryState(scheduler(tmp_path), 'a' * 64, 'axioms', 8 * GIB)
    assert restored.data['attempt_count'] == 0
    assert restored.data['pressure_requeues'] == 1
    assert restored.data['reservation_bytes'] == 8 * GIB


def test_cancellation_after_memory_stop_does_not_launch_or_park(tmp_path, monkeypatch):
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    calls = []
    def cancel():
        if calls:
            raise RuntimeError('revoked')
    @admission.admitted
    def _run_lake_command(*args, **kwargs):
        calls.append(1)
        raise memory_retry.MemoryStopped('host_pressure', 4 * GIB, 'floor')
    with admission.authoritative_workspace('sealed', cancelled=cancel,
            capacity_failure=lambda _: pytest.fail('parked cancellation')):
        with admission.prepared_readonly('axioms'):
            with pytest.raises(RuntimeError, match='revoked'):
                _run_lake_command(tmp_path, ['env', 'lean'], timeout_secs=1, bwrap_role='lake_compiler')
    assert calls == [1] and not pool.active and not pool.waiters


def test_pressure_elapsed_does_not_consume_command_timeout_or_change_normal_timeout(tmp_path, monkeypatch):
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    seen, blocked = [], []
    @admission.admitted
    def _run_lake_command(*args, **kwargs):
        seen.append(kwargs['timeout_secs'])
        if len(seen) == 1:
            import time
            time.sleep(.01)
            raise memory_retry.MemoryStopped('host_pressure', 4 * GIB, 'floor')
        return dict(returncode=None, timed_out=True)
    with admission.authoritative_workspace('sealed', capacity_failure=blocked.append):
        with admission.prepared_readonly('axioms'):
            result = _run_lake_command(tmp_path, ['env', 'lean'], timeout_secs=1, bwrap_role='lake_compiler')
    assert result == dict(returncode=None, timed_out=True)
    assert seen == [1, 1] and not blocked


def test_nested_lease_cannot_enter_optimistic_retry(tmp_path, monkeypatch):
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    @admission.admitted
    def _run_lake_command(*args, **kwargs):
        lease = pool.active[threading.get_ident()]
        assert lease['depth'] == 2 and not lease['controlled']
        return dict(returncode=0)
    with admission.authoritative_lease():
        with admission.prepared_readonly('axioms'):
            _run_lake_command(tmp_path, ['env', 'lean'], timeout_secs=1, bwrap_role='lake_compiler')


def test_failed_group_cleanup_does_not_release_capacity(tmp_path):
    pool = scheduler(tmp_path)
    class Group:
        def close(self):
            raise RuntimeError('still populated')
    with pytest.raises(RuntimeError, match='still populated'):
        with pool.acquire('one', operation='axioms', controlled=True):
            pool.active[threading.get_ident()]['cgroup'] = Group()
    assert pool.cleanup_failed and pool.active
    with pytest.raises(OSError, match='cleanup incomplete'):
        pool._can_admit('next', 0, 8 * GIB)


def test_limit_excess_is_not_suppressed_by_available_host_memory(tmp_path, monkeypatch):
    pool = scheduler(tmp_path)
    monkeypatch.setattr('trellis.memory_guard.descendant_rss',
        lambda *_: (9 * GIB, [(1, 9 * GIB, 'lean')]))
    with pool.acquire('one', operation='axioms', controlled=True, limit_bytes=8 * GIB):
        failure = pool.memory_stop(MemoryGuard(8 * GIB), 1)
        assert failure.cause == 'execution_limit'
        assert pool.pressure_victim == threading.get_ident()


@pytest.mark.parametrize('cause', ['cgroup_oom', 'execution_limit'])
def test_known_limit_failure_remains_typed_while_another_victim_drains(tmp_path, monkeypatch, cause):
    pool = scheduler(tmp_path)
    monkeypatch.setattr('trellis.memory_guard.descendant_rss',
        lambda *_: (9 * GIB, [(1, 9 * GIB, 'lean')]))
    class Group:
        def sample(self):
            return dict(oom_kills=1, oom_events=1, max_events=1, peak_bytes=19 * GIB)
        def close(self):
            pass
    with pool.acquire('one', operation='axioms', controlled=True, limit_bytes=8 * GIB):
        pool.pressure_victim = -123  # Another live reporting thread owns cleanup.
        if cause == 'cgroup_oom':
            pool.active[threading.get_ident()]['cgroup'] = Group()
        failure = pool.memory_stop(MemoryGuard(8 * GIB if cause == 'execution_limit' else 0), 1)
        assert failure.cause == cause
        assert pool.pressure_victim == -123


def test_advisory_controlled_retry_keeps_job_and_snapshot(tmp_path, monkeypatch):
    from test_check_jobs import setup_project as setup, request, wait_done
    from trellis.checker.jobs import WorkspaceSession
    repo, runtime, token, service = setup(tmp_path)
    service.scheduler = scheduler(tmp_path)
    calls = []
    monkeypatch.setattr(service, '_admission_inputs', lambda *a:
        ('advisory:scratch:dependencies_reused', None))
    def execute(spec, job):
        calls.append((job['id'], job['snapshot'], service.scheduler.active[threading.get_ident()]['estimate']))
        if len(calls) == 1:
            raise memory_retry.MemoryStopped('host_pressure', 4 * GIB, 'sampled floor')
        return dict(returncode=1, backend='incremental', _diagnostics_complete=True,
                    peak_tree_rss_bytes=4 * GIB, stdout='ordinary Lean diagnostic', stderr='')
    service.execute_override = execute
    service.worker.start()
    try:
        job = service.handle(request(token))
        done = wait_done(service, token, job)
    finally:
        service.stop.set(); service.wake.set(); service.worker.join(3)
    assert done['state'] == 'failed'
    assert done['result']['memory_attempt_count'] == 2
    assert calls[0][:2] == calls[1][:2]
    assert [row[2] for row in calls] == [8 * GIB, 8 * GIB]
    assert done['result']['memory_hard_attempt_count'] == 0
    assert done['result']['memory_pressure_requeues'] == 1
    assert not service.scheduler.active and not service.claimed


def test_parent_group_oom_policy_rejected_before_moving_manager(tmp_path):
    from trellis.checker.compiler_cgroup import CompilerGroups
    (tmp_path / 'cgroup.controllers').write_text('memory')
    (tmp_path / 'memory.oom.group').write_text('1')
    with pytest.raises(RuntimeError, match='OOMPolicy=continue'):
        CompilerGroups(tmp_path)
    assert not (tmp_path / 'manager').exists()


def test_configured_maximum_respects_physical_floor_and_operator_ceiling(monkeypatch):
    read_text = Path.read_text
    monkeypatch.setattr(Path, 'read_text', lambda path, *a, **k:
        'MemTotal: 33554432 kB\n' if str(path) == '/proc/meminfo' else read_text(path, *a, **k))
    monkeypatch.delenv('TRELLIS_CHECK_MAX_JOB_GIB', raising=False)
    assert admission._configured_maximum(8 * GIB) == 24 * GIB
    monkeypatch.setenv('TRELLIS_CHECK_MAX_JOB_GIB', '20')
    assert admission._configured_maximum(8 * GIB) == 20 * GIB
    monkeypatch.setenv('TRELLIS_CHECK_MAX_JOB_GIB', 'nan')
    with pytest.raises(ValueError):
        admission._configured_maximum(8 * GIB)


def test_production_optimistic_mode_requires_delegated_limits(monkeypatch):
    monkeypatch.setenv('TRELLIS_CHECK_MEMORY_POLICY', 'optimistic')
    monkeypatch.setenv('TRELLIS_CHECK_CGROUP', 'off')
    with pytest.raises(ValueError, match='TRELLIS_CHECK_CGROUP=delegated'):
        admission.configured()


def test_carrier_failure_is_infrastructure_without_memory_retry(tmp_path):
    from types import SimpleNamespace
    pool = scheduler(tmp_path)
    class Group:
        def close(self):
            pass
    with pool.acquire('one', operation='axioms', controlled=True):
        pool.active[threading.get_ident()]['cgroup'] = Group()
        with pytest.raises(OSError, match='group launch failed'):
            pool.check_carrier(SimpleNamespace(poll=lambda: 125))
        pool.check_carrier(SimpleNamespace(poll=lambda: 1))
    assert not pool.active


def test_many_pressure_stops_do_not_spend_hard_attempts_or_raise_limits(tmp_path, monkeypatch):
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    waits, calls = [], []
    def wait(state, cancelled):
        cancelled()
        assert not pool.active and not pool.waiters
        waits.append(state.pressure_delay())
    monkeypatch.setattr(memory_retry.RetryState, 'wait_pressure', wait)
    @admission.admitted
    def _run_lake_command(*args, **kwargs):
        calls.append((pool.active[threading.get_ident()]['limit_bytes'], kwargs['timeout_secs']))
        if len(calls) <= 7:
            raise memory_retry.MemoryStopped('host_pressure', 4 * GIB, 'floor')
        return dict(returncode=0)
    with admission.authoritative_workspace('sealed', capacity_failure=lambda _: pytest.fail('pressure rejected')):
        with admission.prepared_readonly('axioms'):
            assert _run_lake_command(tmp_path, ['env', 'lean'], timeout_secs=2, bwrap_role='lake_compiler')['returncode'] == 0
    assert calls == [(24 * GIB, 2)] * 8
    assert waits == [.25, .5, 1, 2, 4, 5, 5]
    saved = json.loads(next(pool.retry_root.glob('*/*.json')).read_text())
    assert saved['attempt_count'] == 0 and saved['attempted_limit_bytes'] == []
    assert saved['last_success_launches'] == 8


def test_rpc_deadline_after_pressure_returns_automatic_requeue_after_cleanup(tmp_path, monkeypatch):
    pool = scheduler(tmp_path)
    monkeypatch.setattr(admission, 'ACTIVE', pool)
    clock = [100.0]
    monkeypatch.setattr(admission.time, 'monotonic', lambda: clock[0])
    outcomes = []
    @admission.admitted
    def _run_lake_command(*args, **kwargs):
        clock[0] += 4000
        raise memory_retry.MemoryStopped('host_pressure', 3 * GIB, 'floor', cgroup_peak_bytes=5 * GIB)
    def outcome(detail):
        assert not pool.active and not pool.waiters
        outcomes.append(detail)
    with admission.authoritative_workspace('sealed', capacity_failure=outcome):
        with admission.prepared_readonly('axioms'), pytest.raises(ProtocolError) as exc:
            _run_lake_command(tmp_path, ['env', 'lean'], timeout_secs=1, bwrap_role='lake_compiler')
    assert exc.value.kind == 'host_pressure_requeue'
    detail = outcomes[0]
    assert detail['kind'] == 'host_pressure_requeue' and detail['drained']
    assert detail['attempt_count'] == 0 and detail['attempted_limit_bytes'] == []
    assert detail['pressure_requeues'] == 1 and detail['pressure_execution_seconds'] == 4000
    assert detail['sampled_peak_bytes'] == 3 * GIB and detail['cgroup_peak_bytes'] == 5 * GIB


def test_legacy_unknown_attempt_limits_retain_clamped_headroom_not_rejection(tmp_path):
    pool = scheduler(tmp_path)
    state = memory_retry.RetryState(pool, 'b' * 64, 'axioms', 8 * GIB)
    state.data.update(version=1, attempt_count=3, reservation_bytes=80 * GIB, limit_bytes=84 * GIB)
    state.save()
    state = memory_retry.RetryState(pool, 'b' * 64, 'axioms', 80 * GIB)
    assert state.remaining(20) == 20
    assert state.data['attempt_count'] == 0 and state.data['attempted_limit_bytes'] == []
    assert state.data['reservation_bytes'] == state.data['limit_bytes'] == pool.maximum_job_bytes


def test_hard_stops_consume_execution_budget_but_pressure_does_not(tmp_path):
    state = memory_retry.RetryState(scheduler(tmp_path), 'c' * 64, 'axioms', 8 * GIB)
    state.begin()
    state.stopped(memory_retry.MemoryStopped('host_pressure', GIB, 'floor'), 100)
    assert state.remaining(5) == 5
    state.begin()
    state.stopped(memory_retry.MemoryStopped('execution_limit', 25 * GIB, 'limit'), 3)
    assert state.remaining(5) == 2
    with pytest.raises(memory_retry.ExecutionBudgetExpired):
        state.remaining(2)
    assert state.data['discarded_seconds'] == 103
    assert state.data['attempt_count'] == 1 and state.data['pressure_requeues'] == 1


@pytest.mark.parametrize('max_events,oom_events', [(0, 0), (2, 0), (0, 2)])
def test_ancestor_or_global_oom_never_consumes_local_hard_attempt(tmp_path, monkeypatch, max_events, oom_events):
    pool = scheduler(tmp_path)
    monkeypatch.setattr('trellis.memory_guard.descendant_rss', lambda *_: (GIB, []))
    class Group:
        def sample(self):
            return dict(oom_kills=1, oom_events=oom_events, max_events=max_events, peak_bytes=9 * GIB)
        def close(self):
            pass
    with pool.acquire('one', operation='axioms', controlled=True, limit_bytes=24 * GIB):
        pool.pressure_victim = -123  # Kernel killed our group despite another draining victim.
        pool.active[threading.get_ident()]['cgroup'] = Group()
        stopped = pool.memory_stop(MemoryGuard(0), 1)
        assert stopped.cause == 'host_pressure'
        assert stopped.peak_bytes == GIB and stopped.cgroup_peak_bytes == 9 * GIB


def test_reused_group_oom_counters_reset_for_each_lease_even_same_limit(tmp_path):
    from trellis.checker.compiler_cgroup import CompilerGroup
    group = CompilerGroup.__new__(CompilerGroup)
    group.path, group.limit_bytes = tmp_path, 24 * GIB
    group.events_before = dict(max=0, oom=0, oom_kill=0)
    (tmp_path / 'memory.events.local').write_text('max 3\noom 2\noom_kill 1\n')
    for name, value in [('memory.current', GIB), ('memory.peak', 26 * GIB), ('memory.swap.current', 0)]:
        (tmp_path / name).write_text(str(value))
    assert group.sample()['oom_kills'] == 1
    group.limit(24 * GIB)
    assert group.sample()['oom_kills'] == group.sample()['max_events'] == group.sample()['oom_events'] == 0
    assert group.sample()['peak_bytes'] == 26 * GIB  # Explicitly lifetime, not sampled RSS.
    group.limit(48 * GIB)
    (tmp_path / 'memory.events.local').write_text('max 5\noom 3\noom_kill 2\n')
    assert group.sample()['max_events'] == 2 and group.sample()['oom_events'] == 1


def test_advisory_pressure_survives_many_stops_with_same_snapshot(tmp_path, monkeypatch):
    from test_check_jobs import setup_project as setup, request, wait_done
    repo, runtime, token, service = setup(tmp_path)
    service.scheduler = scheduler(tmp_path)
    monkeypatch.setattr(service, '_admission_inputs', lambda *a: ('advisory:scratch:dependencies_reused', None))
    monkeypatch.setattr(memory_retry.RetryState, 'pressure_delay', lambda _: .001)
    calls = []
    def execute(spec, job):
        calls.append((job['id'], job['snapshot']))
        if len(calls) < 6:
            raise memory_retry.MemoryStopped('host_pressure', GIB, 'floor')
        return dict(returncode=0, stdout='', stderr='')
    service.execute_override = execute
    service.worker.start()
    try:
        job = service.handle(request(token))
        done = wait_done(service, token, job)
    finally:
        service.stop.set(); service.wake.set(); service.worker.join(3)
    assert done['state'] == 'succeeded' and len(set(calls)) == 1
    assert done['result']['memory_attempt_count'] == 6
    assert done['result']['memory_hard_attempt_count'] == 0
    assert done['result']['memory_pressure_requeues'] == 5
    assert not service.scheduler.active


@pytest.mark.parametrize('kind,cause,generation,requeued', [
    ('resource_rejected', 'cgroup_oom', 'new', False),
    ('capacity_blocked', 'host_pressure', '0', True),
    ('capacity_blocked', 'execution_limit', '0', False),
    ('capacity_blocked', 'execution_limit', 'new', False),
])
def test_advisory_restart_preserves_rejection_and_recovers_legacy_pressure(tmp_path, monkeypatch, kind, cause, generation, requeued):
    from test_check_jobs import setup_project as setup, request
    from trellis.checker.jobs import JobService
    repo, runtime, token, service = setup(tmp_path)
    job = service.handle(request(token))
    service._finish(job['job_id'], 'resource_exhausted', {kind: dict(kind=kind, cause=cause, drained=True, policy_generation='0')})
    service.db.close()
    pool = scheduler(tmp_path)
    pool.generation = generation
    restarted = JobService(runtime, scheduler=pool)
    row = restarted.db.execute('SELECT state,snapshot,result FROM jobs WHERE id=?', (job['job_id'],)).fetchone()
    assert row['state'] == ('queued' if requeued else 'resource_exhausted')
    assert row['snapshot'] == job['input_sha256']
    if kind == 'capacity_blocked' and cause == 'execution_limit':
        result = json.loads(row['result'])
        assert result['message'].endswith('this advisory check is complete.')
        assert 'capacity_blocked' not in result and result['legacy_capacity_detail']['drained']
    restarted.db.close()


def test_nested_lease_does_not_reset_outer_cgroup_event_baseline(tmp_path):
    pool = scheduler(tmp_path)
    class Group:
        def limit(self, value):
            pytest.fail('nested lease reset an active command event baseline')
        def close(self):
            pass
    with pool.acquire('one', operation='axioms', controlled=True, limit_bytes=24 * GIB):
        pool.active[threading.get_ident()]['cgroup'] = Group()
        with pool.acquire('one'):
            assert pool.active[threading.get_ident()]['depth'] == 2


def test_advisory_hard_exhaustion_retains_snapshot_and_reports_actual_limits(tmp_path, monkeypatch):
    from test_check_jobs import setup_project as setup, request, wait_done
    repo, runtime, token, service = setup(tmp_path)
    service.scheduler = scheduler(tmp_path)
    monkeypatch.setattr(service, '_admission_inputs', lambda *a: ('advisory:scratch:dependencies_reused', None))
    calls = []
    def execute(spec, job):
        calls.append((job['id'], job['snapshot'], service.scheduler.active[threading.get_ident()]['limit_bytes']))
        raise memory_retry.MemoryStopped('cgroup_oom', 3 * GIB, 'limit', cgroup_peak_bytes=24 * GIB)
    service.execute_override = execute
    service.worker.start()
    try:
        job = service.handle(request(token))
        done = wait_done(service, token, job)
    finally:
        service.stop.set(); service.wake.set(); service.worker.join(3)
    assert done['state'] == 'resource_exhausted'
    detail = done['result']['resource_rejected']
    assert detail['attempted_limit_bytes'] == [24 * GIB, 48 * GIB]
    assert detail['attempt_count'] == 2 and detail['pressure_requeues'] == 0
    assert detail['sampled_peak_bytes'] == 3 * GIB and detail['cgroup_peak_bytes'] == 24 * GIB
    assert calls[0][:2] == calls[1][:2]
    assert (service.root / 'snapshots' / (calls[0][1] + '.json')).is_file()
    assert not service.scheduler.active and not service.claimed


def test_raised_probe_headroom_crosses_workspace_but_never_rejection_history(tmp_path):
    pool = scheduler(tmp_path)
    old = tmp_path / 'old'; new = tmp_path / 'new'
    old.mkdir(); new.mkdir()
    (old / 'probe-a.lean').write_text('import Tablet.A\n#print axioms A\n')
    (new / 'probe-b.lean').write_text((old / 'probe-a.lean').read_text())
    kwargs = dict(bwrap_role='lake_compiler')
    shared = memory_retry.command_key(None, 'axioms', (old, ['env', 'lean', 'probe-a.lean']), kwargs, shared_headroom=True)
    moved = memory_retry.command_key(None, 'axioms', (new, ['env', 'lean', 'probe-b.lean']), kwargs, shared_headroom=True)
    assert shared == moved
    old_key = memory_retry.command_key('old-owner', 'axioms', (old, ['env', 'lean', 'probe-a.lean']), kwargs)
    new_key = memory_retry.command_key('new-owner', 'axioms', (new, ['env', 'lean', 'probe-b.lean']), kwargs)
    assert old_key != new_key
    state = memory_retry.RetryState(pool, old_key, 'axioms', 8 * GIB, headroom_key=shared)
    state.begin(); state.stopped(memory_retry.MemoryStopped('cgroup_oom', 4 * GIB, 'cap'), .1)
    state.begin(); state.stopped(memory_retry.MemoryStopped('cgroup_oom', 4 * GIB, 'cap'), .1)
    with pytest.raises(memory_retry.ResourceOutcome):
        state.remaining(20)
    fresh = memory_retry.RetryState(pool, new_key, 'axioms', 8 * GIB, headroom_key=moved)
    assert fresh.data['limit_bytes'] == 48 * GIB and fresh.data['reservation_bytes'] == 32 * GIB
    assert fresh.remaining(20) == 20 and fresh.data['attempted_limit_bytes'] == []
    (new / 'probe-b.lean').write_text('import Tablet.B\n#print axioms B\n')
    different = memory_retry.command_key(None, 'axioms', (new, ['env', 'lean', 'probe-b.lean']), kwargs, shared_headroom=True)
    assert different != shared
    assert memory_retry.RetryState(pool, 'd' * 64, 'axioms', 8 * GIB, headroom_key=different).data['limit_bytes'] == 24 * GIB
    assert pool.estimate('axioms', readonly=True) == 8 * GIB  # No global operation-floor increase.


def test_ancestor_kill_keeps_noncontrolled_infrastructure_callback(tmp_path, monkeypatch):
    pool = scheduler(tmp_path)
    monkeypatch.setattr('trellis.memory_guard.descendant_rss', lambda *_: (GIB, []))
    seen = []
    class Group:
        def sample(self):
            return dict(oom_kills=1, oom_events=0, max_events=0, peak_bytes=9 * GIB)
        def close(self):
            pass
    with admission.authoritative_workspace('one', resource_failure=seen.append):
        with pool.acquire('one', operation='axioms', controlled=False, limit_bytes=24 * GIB):
            pool.pressure_victim = -123
            pool.active[threading.get_ident()]['cgroup'] = Group()
            assert pool.memory_stop(MemoryGuard(0), 1).cause == 'host_pressure'
    assert seen == ['compiler killed by host or ancestor memory pressure']


def test_last_bounded_hard_trial_gets_host_maximum_even_on_large_host(tmp_path):
    pool = scheduler(tmp_path)
    pool.maximum_job_bytes = 100 * GIB
    state = memory_retry.RetryState(pool, 'e' * 64, 'axioms', 8 * GIB)
    caps = []
    for _ in range(3):
        assert state.remaining(20) > 0
        caps.append(state.data['limit_bytes'])
        state.begin(); state.stopped(memory_retry.MemoryStopped('cgroup_oom', GIB, 'cap'), .1)
    assert caps == [24 * GIB, 48 * GIB, 100 * GIB]
    with pytest.raises(memory_retry.ResourceOutcome) as error:
        state.remaining(20)
    assert error.value.detail['attempted_limit_bytes'] == caps
