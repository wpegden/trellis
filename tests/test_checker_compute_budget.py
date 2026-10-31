"""CPU grants and pressure reduction preserve independent checked work."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import subprocess
import sys
import threading

import pytest

from trellis.checker import admission, compute_budget, memory_retry, serial_recovery
from trellis.memory_guard import GIB


def pool(tmp_path, **kwargs):
    return admission.Admission(6, readonly_slots=6, cpu_budget=6, adaptive=True,
        available_memory=True, optimistic=True, available=kwargs.pop('available', lambda: 60 * GIB),
        floor_bytes=8 * GIB, retry_root=tmp_path / 'retries', **kwargs)


def test_six_same_workspace_probes_execute_and_nested_work_inherits_grant(tmp_path):
    scheduler = pool(tmp_path)
    barrier = threading.Barrier(6)
    def run(_):
        with scheduler.acquire('one-workspace', readonly=True, operation='local_closure'):
            barrier.wait(3)
            with scheduler.acquire('one-workspace', readonly=True):
                assert scheduler.lean_threads() == 1
                assert scheduler.snapshot()['cpu_units_in_use'] == 6
            barrier.wait(3)
    with ThreadPoolExecutor(6) as workers:
        list(workers.map(run, range(6)))
    assert scheduler.snapshot()['executing'] == 0


def test_build_uses_only_remaining_cpu_units(tmp_path):
    scheduler = pool(tmp_path)
    with scheduler.acquire('reader', readonly=True, operation='semantic'):
        with ThreadPoolExecutor(1) as workers:
            def build():
                with scheduler.acquire('build'):
                    assert scheduler.lean_threads() == 5
                    assert scheduler.snapshot()['cpu_units_in_use'] == 6
                    assert not scheduler._can_admit('another', 0, 0)[0]
            workers.submit(build).result(3)


def test_pressure_reduces_one_slot_and_recovers_only_after_stability_and_completion(tmp_path, monkeypatch):
    available, now = [7 * GIB], [0.0]
    scheduler = pool(tmp_path, available=lambda: available[0], clock=lambda: now[0])
    scheduler.active = {n: dict(owner=str(n), priority=0, admission_sequence=n,
        processes=set(), completed=False, peak_bytes=0) for n in range(6)}
    monkeypatch.setattr(admission.threading, 'get_ident', lambda: 0)
    assert scheduler.pressure_reason(None) is None
    assert scheduler.pressure_victim == 5
    assert scheduler.concurrency_limit == 5
    assert not scheduler._can_admit('queued', 0, 0)[0]
    del scheduler.active[5]
    scheduler.pressure_victim = None
    available[0] = 11 * GIB
    scheduler._sample_recovery()
    now[0] = 4
    scheduler.complete_execution()
    assert scheduler.concurrency_limit == 5
    now[0] = 6
    scheduler._sample_recovery()  # Passing time alone does not raise concurrency.
    assert scheduler.concurrency_limit == 5
    scheduler.complete_execution()
    assert scheduler.concurrency_limit == 5  # Duplicate completion is not another success.
    monkeypatch.setattr(admission.threading, 'get_ident', lambda: 1)
    scheduler.complete_execution()
    assert scheduler.concurrency_limit == 6
    assert [c['after'] for c in scheduler.concurrency_changes] == [5, 6]


def test_ancestor_headroom_stops_admission_without_reserving_job_ceiling(tmp_path):
    class Groups:
        free = 7 * GIB
        def headroom(self): return self.free
    groups = Groups()
    scheduler = pool(tmp_path, groups=groups)
    assert not scheduler._can_admit('one', 0, 48 * GIB)[0]
    groups.free = 9 * GIB
    assert scheduler._can_admit('one', 0, 48 * GIB)[0]


def test_alone_pressure_is_queued_and_hysteresis_does_not_block_ordinary_readmission(tmp_path, monkeypatch):
    available = [7 * GIB]
    scheduler = pool(tmp_path, available=lambda: available[0])
    scheduler.active = {1: dict(owner='only', priority=0, admission_sequence=0,
        processes=set(), completed=False, peak_bytes=0)}
    monkeypatch.setattr(admission.threading, 'get_ident', lambda: 1)
    assert scheduler.pressure_reason(None)
    assert scheduler.concurrency_limit == 1
    scheduler.active.clear(); scheduler.pressure_victim = None
    available[0] = 9 * GIB  # Below reopening margin, above ordinary floor.
    assert scheduler._can_admit('only', 0, 48 * GIB)[0]
    assert scheduler.concurrency_limit == 1


def test_preempted_job_is_protected_from_fresh_arrivals(tmp_path, monkeypatch):
    scheduler = pool(tmp_path, available=lambda: 7 * GIB)
    scheduler.active = {n: dict(owner=str(n), priority=0, admission_sequence=n,
        processes=set(), completed=False, peak_bytes=0, pressure_requeues=int(n == 2)) for n in range(3)}
    monkeypatch.setattr(admission.threading, 'get_ident', lambda: 0)
    scheduler.pressure_reason(None)
    assert scheduler.pressure_victim == 1


def stop(state, cause):
    with state.scheduler.acquire('one', **state.lease_options()):
        state.begin()
    state.stopped(memory_retry.MemoryStopped(cause, GIB, cause), 2)


def test_pressure_stays_shared_but_own_cap_still_escalates(tmp_path):
    scheduler = pool(tmp_path)
    state = serial_recovery.SerialRetry(scheduler, 'a' * 64, 'local_closure', 48 * GIB)
    state.initialize(10)
    stop(state, 'host_pressure')
    assert not state.data['serial']
    assert state.remaining(10) == 10
    assert state.lease_options()['cpu_units'] == 1
    stop(state, 'cgroup_oom')
    assert state.data['serial'] and state.lease_options()['exclusive']
    assert state.remaining(10) == 8
    assert state.data['attempted_limit_bytes'] == [48 * GIB]


def test_old_pressure_only_record_migrates_without_discarding_history(tmp_path):
    scheduler = pool(tmp_path)
    state = serial_recovery.SerialRetry(scheduler, 'a' * 64, 'local_closure', 48 * GIB)
    state.initialize(10)
    stop(state, 'host_pressure')
    state.data.update(version=4, serial=True)
    state.save()
    resumed = serial_recovery.SerialRetry(scheduler, 'a' * 64, 'local_closure', 48 * GIB)
    assert not resumed.data['serial']
    assert resumed.data['pressure_requeues'] == 1
    assert resumed.data['discarded_seconds'] == 2
    assert resumed.remaining(10) == 10


def test_host_pool_is_shared_between_processes_and_released_after_exit(tmp_path):
    first = compute_budget.HostCpuPool(tmp_path, 2)
    held = first.acquire(1)
    code = '''
import sys
from trellis.checker.compute_budget import HostCpuPool
pool = HostCpuPool(sys.argv[1], 2)
leases = pool.acquire(2)
print(len(leases), flush=True)
sys.stdin.readline()
'''
    child = subprocess.Popen([sys.executable, '-c', code, str(tmp_path)],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        assert child.stdout.readline().strip() == '1'
        assert first.acquire(1) == []
        child.stdin.close(); child.wait(timeout=5)
        recovered = first.acquire(1)
        assert len(recovered) == 1
        first.release(recovered)
    finally:
        if child.poll() is None: child.kill(); child.wait()
        first.release(held)


def test_host_cpu_tokens_stay_held_until_cgroup_cleanup(tmp_path):
    host = compute_budget.HostCpuPool(tmp_path / 'cpu', 1)
    scheduler = pool(tmp_path, host_cpu_pool=host)
    class Group:
        def close(self): assert host.acquire(1) == []
    with scheduler.acquire('work'):
        scheduler.active[threading.get_ident()]['cgroup'] = Group()
        assert scheduler.lean_threads() == 1
    held = host.acquire(1)
    assert len(held) == 1
    host.release(held)


def test_conflicting_host_budgets_are_reported(tmp_path):
    compute_budget.HostCpuPool(tmp_path, 6)
    with pytest.raises(ValueError, match='disagree'):
        compute_budget.HostCpuPool(tmp_path, 12)


def test_topology_counts_physical_cores_and_walks_ancestor_quota(tmp_path):
    for cpu in range(16):
        d = tmp_path / f'cpu{cpu}' / 'topology'; d.mkdir(parents=True)
        (d / 'core_id').write_text(str(cpu % 8)); (d / 'physical_package_id').write_text('0')
    parent = tmp_path / 'cgroup'; child = parent / 'manager'; child.mkdir(parents=True)
    (parent / 'cpu.max').write_text('350000 100000')
    c = compute_budget.capacity(affinity=range(16), sys_cpu=tmp_path, cgroup=child)
    assert c == dict(physical_cores=8, logical_cpus=16, quota_cpus=3.5, default_cpu_budget=3)


def test_shared_process_snapshot_avoids_repeated_scans(monkeypatch):
    from trellis import memory_guard
    calls, now = [], [0]
    monkeypatch.setattr(memory_guard, '_read_proc_table', lambda root:
        calls.append(root) or {1: (0, 10, 'parent'), 2: (1, 20, 'child')})
    snapshot = memory_guard.ProcessSnapshot(clock=lambda: now[0])
    a = memory_guard.MemoryGuard(100, process_snapshot=snapshot)
    b = memory_guard.MemoryGuard(100, process_snapshot=snapshot)
    a.check(1); b.check(2)
    assert len(calls) == 1 and a.peak_bytes == 30 and b.peak_bytes == 20
    now[0] = .2; b.check(2)
    assert len(calls) == 2


def test_warm_group_freezes_before_releasing_cpu_and_thaws_after_reacquiring(tmp_path):
    host = compute_budget.HostCpuPool(tmp_path / 'cpu', 1)
    scheduler = pool(tmp_path, host_cpu_pool=host)
    events = []
    class Group:
        cpu_control = True
        def limit(self, value): pass
        def cpu_limit(self, units): assert units == 1
        def freeze(self, frozen):
            assert host.acquire(1) == []
            events.append(('freeze', frozen))
        def close(self): events.append(('drain', True))
    group = Group()
    def evict():
        events.append(('reap', True))
        scheduler.track_process(123, remove=True)
        scheduler.drop_residency('warm')
    with scheduler.acquire('warm'):
        record = scheduler.active[threading.get_ident()]
        record['cgroup'] = group
        scheduler.track_process(123)
        scheduler.retain('warm', evict)
    assert events == [('freeze', True)]
    assert scheduler.snapshot()['cpu_units_in_use'] == 0
    held = host.acquire(1); assert len(held) == 1; host.release(held)
    with scheduler.acquire('warm'):
        assert events[-1] == ('freeze', False)
    scheduler.evict_idle('warm')
    assert events[-2:] == [('drain', True), ('reap', True)]
    assert not scheduler.residents


def test_cooperative_hash_pressure_releases_grant_without_delivering_partial_digest(tmp_path, monkeypatch):
    from trellis.checker import portable_replay
    available = [60 * GIB]
    scheduler = pool(tmp_path, available=lambda: available[0])
    monkeypatch.setattr(admission, 'ACTIVE', scheduler)
    original = admission.cooperative_poll
    calls = []
    def poll():
        calls.append(True)
        if len(calls) == 2:
            available[0] = 7 * GIB
        return original()
    monkeypatch.setattr(admission, 'cooperative_poll', poll)
    path = tmp_path / 'input'; path.write_bytes(b'x' * (2 << 20))
    budget = portable_replay.HashBudget()
    with pytest.raises(portable_replay.CaptureUnavailable, match='deferred'):
        budget.file(path)
    assert not scheduler.active and not budget.memo
    available[0] = 60 * GIB
    assert portable_replay.HashBudget().file(path)['size_bytes'] == 2 << 20


def test_exclusive_recovery_drains_other_checker_services(tmp_path):
    one = compute_budget.HostCpuPool(tmp_path / 'host', 6)
    two = compute_budget.HostCpuPool(tmp_path / 'host', 6)
    shared = one.acquire(1)
    assert not two.acquire(1, exclusive=True)
    one.release(shared)
    serial = two.acquire(1, exclusive=True)
    assert len(serial) == 1
    assert not one.acquire(1)
    two.release(serial)
    shared = one.acquire(6)
    assert len(shared) == 6
    one.release(shared)
