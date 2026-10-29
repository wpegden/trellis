"""Full drains, concurrent scheduler updates, and cooperative shutdown."""
import fcntl
import threading

import pytest

from test_dispatch_storage import terminal_state
from test_dispatch_terminal_retention import setup, put
from trellis.dispatch import storage


def test_one_schedule_drains_more_than_old_owner_and_file_limits(setup):
    original, directory, _, paths, state = setup
    replay = directory / 'repo/.trellis/kernel-replay-environments'
    extra = [put(replay / (f'{n:064x}.json'), {'cached': n}) for n in range(35)]
    collector = storage.BackgroundCollector(original.root, repo=original.repo)
    collector._review_snapshots = None
    assert collector.schedule(state)
    assert collector.wait_idle(10)
    assert all(not path.exists() for path in (*paths, *extra))
    assert collector.summary['private_files'] == 38


def test_busy_schedule_appends_fresh_owners_and_coalesces_repeated_blocked_work(tmp_path, monkeypatch):
    entered, release = threading.Event(), threading.Event()
    calls = []
    def prune(path, **kwargs):
        calls.append(path.name)
        if path.name == '1':
            entered.set(); assert release.wait(5)
        return False  # A pin or completed entry is not a reason to spin.
    monkeypatch.setattr(storage, 'prune', prune)
    collector = storage.BackgroundCollector(tmp_path)
    try:
        assert collector.schedule(terminal_state(1, 2))
        assert entered.wait(2)
        drain_id = collector.summary['drain_id']
        for _ in range(50):
            assert not collector.schedule(terminal_state(1, 2, 3, 4))
        release.set()
        assert collector.wait_idle(5)
        assert calls == ['1', '2', '3', '4']
        assert collector.summary['drain_id'] == drain_id
        assert collector.schedule(terminal_state(1, 2, 3, 4))
        assert collector.wait_idle(5)
        assert calls == ['1', '2', '3', '4'] * 2
        assert collector.summary['drain_id'] != drain_id
    finally:
        release.set(); collector.close()


def test_latest_snapshot_drops_queued_active_owner(tmp_path, monkeypatch):
    entered, release = threading.Event(), threading.Event()
    calls = []
    def prune(path, **kwargs):
        calls.append(path.name)
        if path.name == '1':
            entered.set(); assert release.wait(5)
        return False
    monkeypatch.setattr(storage, 'prune', prune)
    collector = storage.BackgroundCollector(tmp_path)
    try:
        collector.schedule(terminal_state(1, 2)); assert entered.wait(2)
        latest = terminal_state(1, 2, 3)
        latest['dispatch']['tasks']['2']['status']['status'] = 'running'
        assert not collector.schedule(latest)
        release.set(); assert collector.wait_idle(5)
        assert calls == ['1', '3']
    finally:
        release.set(); collector.close()


def test_updated_owner_identity_is_used_without_retaining_previous_snapshot(tmp_path, monkeypatch):
    from trellis.dispatch import terminal_retention, framework_retention
    entered, release = threading.Event(), threading.Event()
    seen = []
    def prune(path, **kwargs):
        if path.name == '1':
            entered.set(); assert release.wait(5)
        return False
    monkeypatch.setattr(storage, 'prune', prune)
    monkeypatch.setattr(terminal_retention, 'terminal_owners', lambda state, **kwargs: state['identities'])
    monkeypatch.setattr(framework_retention, 'retire', lambda *args, **kwargs: {'removed': 0})
    monkeypatch.setattr('trellis.dispatch.assignment_retention.collect',
                        lambda *args, **kwargs: {'archived': 0})
    class Retention:
        def retire_execution(self, *args, **kwargs):
            return {'removed': 0}
        retire_history = retire_execution
        retire_compiled_probes = retire_execution
        def retire(self, directory, observed, **kwargs):
            seen.append((directory.name, observed[directory.name])); return {'removed': 0}
    collector = storage.BackgroundCollector(tmp_path)
    collector._terminal_retention = Retention()
    first = dict(terminal_state(1, 2), identities={'1': 'first', '2': 'old'})
    latest = dict(terminal_state(1, 2), identities={'1': 'first', '2': 'new'})
    try:
        collector.schedule(first); assert entered.wait(2)
        collector.schedule(latest)
        assert collector._terminal_tasks is latest['identities']
        release.set(); assert collector.wait_idle(5)
        assert ('2', 'new') in seen and ('2', 'old') not in seen
    finally:
        release.set(); collector.close()


@pytest.mark.parametrize('wait_kind', ['coordinator', 'cleanup'])
def test_close_during_queued_lock_wait_never_deletes(setup, monkeypatch, wait_kind):
    original, _, _, paths, state = setup
    collector = storage.BackgroundCollector(original.root, repo=original.repo, wait_for_cleanup_lock=True)
    collector._review_snapshots = None
    waiting = threading.Event()
    flock = fcntl.flock
    name = 'coordinator.lock' if wait_kind == 'coordinator' else 'build-cleanup.lock'
    def observed(stream, operation):
        if operation == fcntl.LOCK_EX and str(getattr(stream, 'name', '')).endswith(name):
            waiting.set()
        return flock(stream, operation)
    monkeypatch.setattr(fcntl, 'flock', observed)
    with (original.root / name).open('a+b') as held:
        flock(held, fcntl.LOCK_EX)
        collector.schedule(state)
        assert waiting.wait(5)
        assert not collector.wait_idle(.01)
        collector.close()
        assert not collector.schedule(state)
        assert all(path.exists() for path in paths)
    assert collector.wait_idle(5)
    assert all(path.exists() for path in paths)
    assert collector.summary['private_files'] == 0


def test_close_between_build_trees_preserves_remaining_tree(tmp_path, monkeypatch):
    root = tmp_path / '1'
    targets = [root / name for name in ('repo/.lake', 'acceptance/1/repo/.lake')]
    for target in targets:
        target.mkdir(parents=True); (target / 'cached').write_text('cache')
    collector = storage.BackgroundCollector(tmp_path)
    rmtree = storage.shutil.rmtree
    def remove(path):
        rmtree(path); collector.close()
    monkeypatch.setattr(storage.shutil, 'rmtree', remove)
    collector.schedule(terminal_state(1))
    assert collector.wait_idle(5)
    assert sum(path.exists() for path in targets) == 1
    assert not (root / 'build-caches-pruned.json').exists()


def test_completion_handoff_accepts_new_work_after_idle_before_old_thread_returns(tmp_path, monkeypatch):
    collector = storage.BackgroundCollector(tmp_path)
    calls = []
    monkeypatch.setattr(storage, 'prune', lambda path, **kwargs: calls.append(path.name) or False)
    # set() is called under the completion guard. Queue the next schedule from
    # another thread at exactly that boundary, before the old finally runs.
    original = collector._idle.set
    triggered = threading.Event()
    scheduled = []
    def enqueue():
        assert triggered.wait(5)
        scheduled.append(collector.schedule(terminal_state(1, 2)))
    thread = threading.Thread(target=enqueue)
    thread.start()
    def idle():
        original(); triggered.set()
    monkeypatch.setattr(collector._idle, 'set', idle)
    collector.schedule(terminal_state(1))
    thread.join(5); assert not thread.is_alive()
    assert scheduled == [True]
    assert collector.wait_idle(5)
    assert calls == ['1', '1', '2']
