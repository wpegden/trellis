from contextlib import contextmanager
import copy
import fcntl
import json
from pathlib import Path
import shutil
import threading
import time

import pytest

from test_dispatch_terminal_retention import setup, put, git
from trellis.dispatch.retirement_batch import Broker
from trellis.dispatch.storage import BackgroundCollector
from trellis.dispatch import terminal_retention as retention


def wait_for(predicate, seconds=5):
    deadline = time.monotonic() + seconds
    while not predicate():
        assert time.monotonic() < deadline, 'condition did not become true'
        threading.Event().wait(.005)


def launch(function):
    errors = []
    def run():
        try: function()
        except BaseException as error: errors.append(error)
    thread = threading.Thread(target=run, daemon=True)
    thread.start()
    return thread, errors


def test_one_lease_serializes_many_ready_sections_and_bounds_preparation():
    gate, waiting = threading.Event(), threading.Event()
    held, active, prep, max_prep = False, 0, 0, 0
    guard = threading.Lock()
    @contextmanager
    def lock():
        nonlocal held
        waiting.set(); assert gate.wait(5)
        held = True
        try: yield True
        finally: held = False
    broker = Broker(lock, threading.Event(), preparers=2, quantum=2)
    calls = []
    def work(index):
        nonlocal active, prep, max_prep
        with broker.preparing():
            with guard: prep += 1; max_prep = max(prep, max_prep)
            threading.Event().wait(.002)
            with guard: prep -= 1
            with broker.critical() as acquired:
                assert acquired and held and active == 0
                active += 1; calls.append(index)
                threading.Event().wait(.002)
                active -= 1
    threads = [launch(lambda i=i: work(i)) for i in range(20)]
    assert waiting.wait(5)
    wait_for(lambda: len(broker._pending) == 20)
    gate.set()
    for thread, errors in threads: thread.join(5); assert not thread.is_alive() and not errors
    broker.close(); assert broker.join(5)
    assert len(calls) == 20 and broker.leases == 1 and broker.sections == 20
    assert max_prep <= 2 and not held


def test_quantum_yields_only_after_active_body_finishes():
    gate, entered, finish = threading.Event(), threading.Event(), threading.Event()
    releases = []
    @contextmanager
    def lock():
        assert gate.wait(5)
        try: yield True
        finally: releases.append(True)
    broker = Broker(lock, threading.Event(), quantum=.01)
    def first():
        with broker.critical() as acquired:
            assert acquired; entered.set(); assert finish.wait(5)
    one = launch(first)
    wait_for(lambda: len(broker._pending) == 1)
    def second():
        with broker.critical() as acquired: assert acquired
    two = launch(second)
    wait_for(lambda: len(broker._pending) == 2)
    gate.set(); assert entered.wait(5)
    threading.Event().wait(.03)
    assert releases == []  # Never drop flock in a still-running final fence.
    finish.set()
    for thread, errors in (one, two): thread.join(5); assert not errors
    broker.close(); assert broker.join(5)
    assert len(releases) == 2 and broker.leases == 2


def test_default_lease_finishes_ready_batch_but_yields_before_later_arrivals(monkeypatch):
    from types import SimpleNamespace
    from trellis.dispatch import retirement_batch
    now = [0.0]
    monkeypatch.setattr(retirement_batch, 'time', SimpleNamespace(monotonic=lambda: now[0]))
    admit, entered, finish = (threading.Event() for _ in range(3))
    acquisitions, releases, calls = [], [], []
    @contextmanager
    def lock():
        assert admit.wait(5)
        acquisitions.append(True)
        try: yield True
        finally: releases.append(True)
    broker = Broker(lock, threading.Event())
    def work(index):
        with broker.critical() as acquired:
            assert acquired
            calls.append((index, len(acquisitions)))
            if index == 0:
                entered.set(); assert finish.wait(5)
    threads = []
    for index in range(3):
        threads.append(launch(lambda index=index: work(index)))
        wait_for(lambda: len(broker._pending) == index + 1)
    admit.set(); assert entered.wait(5)
    threads.append(launch(lambda: work(3)))
    wait_for(lambda: len(broker._pending) == 3)
    now[0] = 3600.0  # No elapsed-time cutoff discards the already ready batch.
    finish.set()
    for thread, errors in threads:
        thread.join(5); assert not thread.is_alive() and not errors
    broker.close(); assert broker.join(5)
    assert calls == [(0, 1), (1, 1), (2, 1), (3, 2)]
    assert len(releases) == 2


def test_close_rejects_queued_requests_but_does_not_revoke_active_lease():
    entered, finish = threading.Event(), threading.Event()
    released = threading.Event()
    @contextmanager
    def lock():
        try: yield True
        finally: released.set()
    broker = Broker(lock, threading.Event())
    def active():
        with broker.critical() as acquired:
            assert acquired; entered.set(); assert finish.wait(5)
    first = launch(active); assert entered.wait(5)
    def queued():
        with broker.critical() as acquired: assert not acquired
    second = launch(queued)
    wait_for(lambda: len(broker._pending) == 1)
    broker.close(); second[0].join(5)
    assert not second[1] and not released.is_set() and not broker.join(.01)
    finish.set(); first[0].join(5)
    assert broker.join(5) and released.is_set() and not first[1]


def test_body_exception_releases_grant_and_next_waiter_runs():
    @contextmanager
    def lock(): yield True
    broker = Broker(lock, threading.Event())
    with pytest.raises(RuntimeError, match='body'):
        with broker.preparing():
            with broker.critical() as acquired:
                assert acquired; raise RuntimeError('body failure')
    with broker.preparing():
        with broker.critical() as acquired: assert acquired
    broker.close(); assert broker.join(5) and broker.sections == 2


def test_lock_failure_propagates_and_releases_preparation_permit():
    @contextmanager
    def lock():
        raise OSError('lock unavailable')
        yield
    broker = Broker(lock, threading.Event(), preparers=1)
    for _ in range(2):
        with pytest.raises(OSError, match='lock unavailable'):
            with broker.preparing():
                with broker.critical(): pytest.fail('failed lock granted')
    broker.close(); assert broker.join(5)


def test_timing_reports_in_progress_wait_and_hold_and_freezes_after_release(monkeypatch):
    from types import SimpleNamespace
    from trellis.dispatch import retirement_batch
    now = [10.0]
    monkeypatch.setattr(retirement_batch, 'time', SimpleNamespace(monotonic=lambda: now[0]))
    waiting, admit, entered, finish = (threading.Event() for _ in range(4))
    @contextmanager
    def lock():
        waiting.set(); assert admit.wait(5)
        yield True
    broker = Broker(lock, threading.Event())
    def work():
        with broker.critical() as acquired:
            assert acquired; entered.set(); assert finish.wait(5)
    thread, errors = launch(work)
    assert waiting.wait(5)
    now[0] = 20.0
    assert broker.timing == dict(coordinator_wait_seconds=10.0, coordinator_hold_seconds=0.0)
    admit.set(); assert entered.wait(5)
    now[0] = 23.0
    assert broker.timing == dict(coordinator_wait_seconds=10.0, coordinator_hold_seconds=3.0)
    finish.set(); thread.join(5)
    assert not thread.is_alive() and not errors
    broker.close(); assert broker.join(5)
    now[0] = 30.0
    assert broker.timing == dict(coordinator_wait_seconds=10.0, coordinator_hold_seconds=3.0)


def test_timing_finalizes_a_failed_lock_wait(monkeypatch):
    from types import SimpleNamespace
    from trellis.dispatch import retirement_batch
    now = [10.0]
    monkeypatch.setattr(retirement_batch, 'time', SimpleNamespace(monotonic=lambda: now[0]))
    @contextmanager
    def lock():
        now[0] = 15.0
        raise OSError('lock unavailable')
        yield
    broker = Broker(lock, threading.Event())
    with pytest.raises(OSError, match='lock unavailable'):
        with broker.critical(): pytest.fail('failed lock granted')
    broker.close(); assert broker.join(5)
    now[0] = 20.0
    assert broker.timing == dict(coordinator_wait_seconds=5.0, coordinator_hold_seconds=0.0)


def population(setup, count):
    original, directory, _, _, state = setup
    template = copy.deepcopy(state['dispatch']['tasks']['1'])
    for id in range(2, count + 1):
        shutil.copytree(directory, original.root / str(id))
        task = copy.deepcopy(template); task['id'] = id
        task['assignment']['assignment_id'] = f'owner-{id}'
        state['dispatch']['tasks'][str(id)] = task
    put(original.repo / retention.HISTORY, {'state': state})
    git(original.repo, 'add', '.'); git(original.repo, 'commit', '-qm', 'Resolve all test owners')
    return original, state


def test_full_drain_exceeds_pending_bound_and_coalesces_fences(setup):
    original, state = population(setup, 41)
    collector = BackgroundCollector(original.root, repo=original.repo, owner_workers=8, preparers=2, lease_seconds=2)
    collector._review_snapshots = None
    with (original.root / 'coordinator.lock').open('rb') as held:
        fcntl.flock(held, fcntl.LOCK_EX)
        assert collector.schedule(state)
        wait_for(lambda: collector._broker is not None and len(collector._broker._pending) == 8)
    assert collector.wait_idle(30)
    summary = collector.summary
    assert summary['owners'] == 41 and summary['private_files'] == 123 and summary['errors'] == 0
    assert summary['retirement_sections'] == 41
    assert summary['coordinator_leases'] < 41
    assert summary['coordinator_wait_seconds'] > 0
    assert summary['coordinator_hold_seconds'] > 0
    assert all(not (original.root / str(id) / 'runtime/protocol_state.json').exists() for id in range(1, 42))


def test_pin_added_while_queued_is_rechecked(setup):
    original, directory, _, paths, state = setup
    collector = BackgroundCollector(original.root, repo=original.repo)
    collector._review_snapshots = None
    with (original.root / 'coordinator.lock').open('rb') as held:
        fcntl.flock(held, fcntl.LOCK_EX)
        collector.schedule(state)
        wait_for(lambda: collector._broker is not None and len(collector._broker._pending) == 1)
        put(directory / 'retain-builds.json', {})
    assert collector.wait_idle(10)
    assert all(path.exists() for path in paths)
    assert collector.summary['errors'] == 1


def test_schedule_during_pool_shutdown_is_drained_without_lost_owner(setup, monkeypatch):
    original, state = population(setup, 2)
    first = copy.deepcopy(state); del first['dispatch']['tasks']['2']
    collector = BackgroundCollector(original.root, repo=original.repo)
    collector._review_snapshots = None
    close = Broker.close
    submitted = False
    def enqueue(broker):
        nonlocal submitted
        if not submitted:
            submitted = True
            assert not collector.schedule(state)
        close(broker)
    monkeypatch.setattr(Broker, 'close', enqueue)
    assert collector.schedule(first) and collector.wait_idle(20)
    assert submitted and collector.summary['owners'] == 2
    assert collector.summary['private_files'] == 6 and collector.summary['errors'] == 0


@pytest.mark.parametrize('quantum', [None, .01])
def test_real_flock_native_waiter_runs_between_batches(tmp_path, quantum):
    path = tmp_path / 'coordinator.lock'; path.touch()
    entered, finish, native_waiting, native_acquired, native_release = (threading.Event() for _ in range(5))
    @contextmanager
    def lock():
        with path.open('rb') as stream:
            fcntl.flock(stream, fcntl.LOCK_EX)
            yield True
    broker = Broker(lock, threading.Event(), quantum=quantum)
    def first():
        with broker.critical() as acquired:
            assert acquired; entered.set(); assert finish.wait(5)
    one = launch(first); assert entered.wait(5)
    def native():
        with path.open('rb') as stream:
            native_waiting.set(); fcntl.flock(stream, fcntl.LOCK_EX)
            native_acquired.set(); assert native_release.wait(5)
    other = launch(native); assert native_waiting.wait(5)
    def second():
        with broker.critical() as acquired: assert acquired and native_acquired.is_set()
    two = launch(second); wait_for(lambda: len(broker._pending) == 1)
    threading.Event().wait(.03); finish.set()
    assert native_acquired.wait(5)
    native_release.set()
    for thread, errors in (one, two, other): thread.join(5); assert not errors
    broker.close(); assert broker.join(5)


def test_process_shutdown_does_not_join_blocked_daemon_pool(setup):
    import subprocess
    import sys
    original, _, _, paths, _ = setup
    code = '''import json,time
from pathlib import Path
from trellis.dispatch.storage import BackgroundCollector
root=Path(%r)
collector=BackgroundCollector(root,repo=Path(%r))
collector._review_snapshots=None
collector.schedule(json.loads((root.parent/'protocol_state.json').read_text()))
end=time.monotonic()+10
while collector._broker is None or not collector._broker._pending:
 assert time.monotonic()<end
 time.sleep(.01)
collector.close()
assert not collector.wait_idle(.05)
print('bounded shutdown')
''' % (str(original.root), str(original.repo))
    with (original.root / 'coordinator.lock').open('rb') as held:
        fcntl.flock(held, fcntl.LOCK_EX)
        result = subprocess.run([sys.executable, '-c', code], capture_output=True, text=True, timeout=15)
        assert result.returncode == 0, result.stderr
        assert 'bounded shutdown' in result.stdout
        assert all(path.exists() for path in paths)


def test_checkpoint_rewind_during_queued_fence_preserves_inputs(setup):
    original, directory, _, paths, state = setup
    put(original.repo / 'later.json', {})
    git(original.repo, 'add', '.'); git(original.repo, 'commit', '-qm', 'Later committed history')
    collector = BackgroundCollector(original.root, repo=original.repo)
    collector._review_snapshots = None
    with (original.root / 'coordinator.lock').open('rb') as held:
        fcntl.flock(held, fcntl.LOCK_EX)
        collector.schedule(state)
        wait_for(lambda: collector._broker is not None and len(collector._broker._pending) == 1)
        git(original.repo, 'reset', '--hard', 'HEAD^')
    assert collector.wait_idle(10)
    assert all(path.exists() for path in paths) and collector.summary['errors'] == 1


def test_concurrent_preparers_share_one_checkpoint_decode(setup, monkeypatch):
    collector, _, _, _, _ = setup
    original = retention.subprocess.check_output
    entered, release = threading.Event(), threading.Event()
    calls = []
    def decode(command, *args, **kwargs):
        if '--checkpoint' in command:
            calls.append(command)
            entered.set()
            assert release.wait(5)
        return original(command, *args, **kwargs)
    monkeypatch.setattr(retention.subprocess, 'check_output', decode)
    values = []
    workers = [launch(lambda: values.append(collector.witness())) for _ in range(8)]
    assert entered.wait(5)
    release.set()
    for worker, errors in workers:
        worker.join(10)
        assert not worker.is_alive() and not errors
    assert len(calls) == 1
    assert len(values) == 8 and all(value is values[0] for value in values)
