import copy
import fcntl
import shutil
import threading
import time

import pytest

from test_dispatch_terminal_retention import setup, put, git
from trellis.dispatch import storage, terminal_retention as retention


def wait_for(predicate):
    deadline = time.monotonic() + 10
    while not predicate():
        assert time.monotonic() < deadline
        threading.Event().wait(.01)


def test_new_terminal_builds_prune_while_historical_retirement_waits_for_lock(setup):
    original, directory, _, paths, state = setup
    newer = original.root / '2'
    shutil.copytree(directory, newer)
    task = copy.deepcopy(state['dispatch']['tasks']['1']); task['id'] = 2
    task['assignment']['assignment_id'] = 'new-owner'
    task['status'] = {'status': 'checking'}
    state['dispatch']['tasks']['2'] = task
    build = put(newer / 'repo/.lake/build/large.olean', {})
    collector = storage.BackgroundCollector(original.root, repo=original.repo)
    collector._review_snapshots = None
    with (original.root / 'coordinator.lock').open('rb') as held:
        fcntl.flock(held, fcntl.LOCK_EX)
        assert collector.schedule(state)
        wait_for(lambda: (directory / retention.RECEIPT).exists())
        assert build.exists() and all(p.exists() for p in paths)
        changed = copy.deepcopy(state); changed['dispatch']['tasks']['2']['status'] = {'status': 'resolved'}
        put(original.repo / retention.HISTORY, {'state': changed})
        git(original.repo, 'add', '.'); git(original.repo, 'commit', '-qm', 'Resolve new owner')
        assert not collector.schedule(changed)
        wait_for(lambda: not build.exists())
        assert all(p.exists() for p in paths)  # Still blocked on publication lock.
        assert not collector.wait_idle(.01)
    assert collector.wait_idle(30)
    assert collector.summary['errors'] == 0


def test_build_lane_progresses_during_blocked_legacy_review_retirement(setup, monkeypatch):
    original, directory, _, _, state = setup
    newer = original.root / '2'
    shutil.copytree(directory, newer)
    build = put(newer / 'repo/.lake/build/large.olean', {})
    collector = storage.BackgroundCollector(original.root, repo=original.repo)
    entered, release = threading.Event(), threading.Event()
    def review():
        entered.set(); assert release.wait(10)
        return False
    monkeypatch.setattr(collector, '_collect_review', review)
    try:
        collector.schedule(state); assert entered.wait(10)
        assert build.exists()
        changed = copy.deepcopy(state)
        changed['dispatch']['tasks']['2'] = dict(changed['dispatch']['tasks']['1'], id=2)
        collector.schedule(changed)
        wait_for(lambda: not build.exists())
    finally:
        release.set(); collector.close()
    assert collector.wait_idle(10)


@pytest.mark.parametrize('guard', ['pin', 'live'])
def test_build_lane_preserves_existing_guards(setup, monkeypatch, guard):
    original, directory, _, _, state = setup
    build = put(directory / 'repo/.lake/build/large.olean', {})
    if guard == 'pin': put(directory / 'retain-builds.json', {})
    else:
        put(directory / 'execution/process.json', {'pid': 123})
        put(directory / 'execution/cleanup.json', {'pid': 123})
        monkeypatch.setattr(storage, 'alive', lambda _: True)
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: True)
    collector = storage.BackgroundCollector(original.root, repo=original.repo)
    collector._review_snapshots = None
    collector.schedule(state); assert collector.wait_idle(10)
    assert build.exists()


def test_cache_queue_rechecks_latest_owner_eligibility(setup, monkeypatch):
    original, directory, _, _, state = setup
    task = copy.deepcopy(state['dispatch']['tasks']['1']); task['id'] = 2
    state['dispatch']['tasks']['2'] = task
    entered, release = threading.Event(), threading.Event()
    visited = []
    def prune(path, **kwargs):
        visited.append(path.name)
        if path.name == '1':
            entered.set(); assert release.wait(10)
        return False
    monkeypatch.setattr(storage, 'prune', prune)
    collector = storage.BackgroundCollector(original.root, repo=original.repo)
    collector._review_snapshots = None
    try:
        collector.schedule(state); assert entered.wait(10)
        changed = copy.deepcopy(state); changed['dispatch']['tasks']['2']['status']['status'] = 'checking'
        collector.schedule(changed); release.set()
        assert collector.wait_idle(10)
        assert visited == ['1']
    finally:
        release.set(); collector.close()


@pytest.mark.parametrize('ending', ['stop', 'exception'])
def test_build_lease_survives_until_inflight_cache_mutation_finishes(setup, monkeypatch, ending):
    original, directory, _, _, state = setup
    entered, release = threading.Event(), threading.Event()
    collector = storage.BackgroundCollector(original.root, repo=original.repo)
    collector._review_snapshots = None
    def prune(*args, **kwargs):
        entered.set(); assert release.wait(10)
        return False
    monkeypatch.setattr(storage, 'prune', prune)
    def owner(*args):
        assert entered.wait(10)
        if ending == 'exception': raise KeyboardInterrupt('injected collector interruption')
    monkeypatch.setattr(collector, '_collect_owner', owner)
    # Capture the intentional BaseException without pytest's thread warning;
    # the real collector's finally still has to protect its build lease.
    collect = collector._collect
    def wrapped():
        try: collect()
        except KeyboardInterrupt: pass
    monkeypatch.setattr(collector, '_collect', wrapped)
    try:
        collector.schedule(state); assert entered.wait(10)
        if ending == 'stop': collector.close()
        assert not collector.wait_idle(.05)
        with (original.root / 'build-cleanup.lock').open('rb') as contender:
            with pytest.raises(BlockingIOError):
                fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
        release.set()
        assert collector.wait_idle(10)
        with (original.root / 'build-cleanup.lock').open('rb') as contender:
            fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
    finally:
        release.set(); collector.close()
