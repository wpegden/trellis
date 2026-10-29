"""Regression coverage for cleanup starvation behind protocol lock waiters."""
from concurrent.futures import ThreadPoolExecutor
import fcntl
import threading

import pytest

from test_dispatch_terminal_retention import setup, put, git
from test_dispatch_framework_retention import trees
from trellis.dispatch import framework_retention as framework
from trellis.dispatch import storage


@pytest.mark.parametrize('kind', ['private', 'framework'])
@pytest.mark.parametrize('change', ['none', 'append', 'rewind', 'pin', 'publication'])
def test_queued_cleanup_progresses_and_rechecks_after_wait(trees, monkeypatch, kind, change):
    collector, directory, observed = trees
    collector.wait_for_lock = True
    if change == 'rewind':
        before = git(collector.repo, 'rev-parse', 'HEAD')
        git(collector.repo, 'commit', '--allow-empty', '-qm', 'Checkpoint to be rewound')
    budget = {'files': 1}
    waiting = threading.Event()
    original = fcntl.flock
    def flock(stream, operation):
        if operation == fcntl.LOCK_EX:
            waiting.set()
        return original(stream, operation)
    monkeypatch.setattr(fcntl, 'flock', flock)
    def retire():
        if kind == 'private':
            return collector.retire(directory, observed, apply=True, budget=budget)
        return framework.retire(collector, directory, observed, apply=True, budget=budget)
    with ThreadPoolExecutor(max_workers=1) as pool:
        with (collector.root / 'coordinator.lock').open('rb') as lock:
            original(lock, fcntl.LOCK_EX)
            future = pool.submit(retire)
            assert waiting.wait(5), 'cleanup must queue at the actual final lock'
            assert not future.done()
            assert budget == {'files': 1}, 'waiting cannot consume a deletion slot'
            if change == 'append':
                git(collector.repo, 'commit', '--allow-empty', '-qm', 'Concurrent checkpoint')
            elif change == 'rewind':
                git(collector.repo, 'reset', '--hard', before)
            elif change == 'pin':
                put(directory / 'retain-builds.json', {})
            elif change == 'publication':
                # The same final check is exercised without inventing a publication schema.
                monkeypatch.setattr(collector, '_same_task_pending', lambda _: True)
        if change == 'pin' or (change in ('publication', 'rewind') and kind == 'private'):
            with pytest.raises(ValueError):
                future.result(timeout=5)
            assert budget == {'files': 1}
        else:
            result = future.result(timeout=5)
            refused = change in ('rewind', 'publication')
            assert result['removed'] == (0 if refused else 1)
            assert budget == {'files': 1 if refused else 0}
            if change == 'rewind':
                assert result['status'] == 'checkpoint_changed'
            if change == 'publication':
                assert result['status'] == 'publication_pending'


@pytest.mark.parametrize('kind', ['private', 'framework'])
def test_direct_busy_cleanup_preserves_budget(trees, kind):
    collector, directory, observed = trees
    budget = {'files': 2}
    with (collector.root / 'coordinator.lock').open('rb') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        result = (collector.retire(directory, observed, apply=True, budget=budget)
                  if kind == 'private' else
                  framework.retire(collector, directory, observed, apply=True, budget=budget))
    assert result['status'] == 'coordinator_busy' and result['removed'] == 0
    assert budget == {'files': 2}


def test_background_enables_queueing_only_for_daemon(setup):
    collector, _, _, _, _ = setup
    background = storage.BackgroundCollector(collector.root, repo=collector.repo)
    assert background._terminal_retention.wait_for_lock
    assert not collector.wait_for_lock


def test_schedule_returns_while_daemon_queues_and_progresses_on_release(setup, monkeypatch):
    collector, directory, observed, paths, state = setup
    background = storage.BackgroundCollector(collector.root, repo=collector.repo)
    background._review_snapshots = None
    waiting = threading.Event()
    original = fcntl.flock
    def flock(stream, operation):
        if operation == fcntl.LOCK_EX:
            waiting.set()
        return original(stream, operation)
    monkeypatch.setattr(fcntl, 'flock', flock)
    with (collector.root / 'coordinator.lock').open('rb') as lock:
        original(lock, fcntl.LOCK_EX)
        assert background.schedule(state)
        assert waiting.wait(5)
        assert background._thread.is_alive()
        assert not background.schedule(state), 'one waiting daemon, no accumulated workers'
        assert all(path.exists() for path in paths)
        assert background.summary['private_files'] == 0
    background._thread.join(5)
    assert not background._thread.is_alive()
    assert not any(path.exists() for path in paths), 'same wake drains all inputs'
