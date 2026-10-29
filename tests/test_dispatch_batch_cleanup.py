import fcntl
import json
import threading

import pytest

from trellis.dispatch import batch_cache, storage


def state(*ids):
    return dict(dispatch=dict(tasks={str(id): dict(review_id=7, status=dict(status='resolved')) for id in ids},
                              receipts={'7': {}}, verifiers=dict(jobs={})))


def cache(root, name, workers=('1',)):
    directory = root / 'batch-caches/7' / name
    build = directory / 'repo/.lake/build/cache.olean'
    build.parent.mkdir(parents=True)
    build.write_text('disposable build')
    source = directory / 'repo/Tablet/A.lean'
    source.parent.mkdir()
    source.write_text('retained proof')
    (directory / 'capture.json').write_text(json.dumps(dict(format=batch_cache.FORMAT,
        binding=dict(review_id=7, workers=list(workers)), files={}, sources={})))
    return directory


def queued(root, ledger, monkeypatch):
    collector = storage.BackgroundCollector(root)
    with monkeypatch.context() as patch:
        patch.setattr(threading.Thread, 'start', lambda _: None)
        assert collector.schedule(ledger)
    return collector


def test_scheduler_never_reads_capture_history(tmp_path, monkeypatch):
    cache(tmp_path, 'first')
    def forbidden(*args):
        raise AssertionError('scheduler read capture history')
    monkeypatch.setattr(batch_cache, 'read', forbidden)
    collector = queued(tmp_path, state(1), monkeypatch)
    assert collector._batch_tasks == {'1': 7}
    assert collector._batch_reviews == ('7',)


def test_capture_parsing_happens_only_in_cleanup_thread(tmp_path, monkeypatch):
    donor = cache(tmp_path, 'first')
    original = batch_cache.read
    readers = []
    def read(path):
        readers.append(threading.current_thread().name)
        return original(path)
    monkeypatch.setattr(batch_cache, 'read', read)
    collector = storage.BackgroundCollector(tmp_path)
    assert collector.schedule(state(1))
    collector._thread.join(3)
    assert not collector._thread.is_alive()
    assert readers == ['private-build-cleanup']
    assert not (donor / 'repo/.lake').exists()
    assert (donor / 'repo/Tablet/A.lean').read_text() == 'retained proof'
    assert (donor / 'capture.json').exists()


@pytest.mark.parametrize('fault', ['malformed', 'wrong_review', 'unresolved', 'foreign_worker', 'pin', 'capture_alias'])
def test_bad_or_live_capture_does_not_block_later_cache(tmp_path, monkeypatch, fault):
    first, later = cache(tmp_path, 'first'), cache(tmp_path, 'later', ('2',))
    ledger = state(1, 2)
    if fault == 'malformed':
        (first / 'capture.json').write_text('{broken')
    elif fault == 'wrong_review':
        value = json.loads((first / 'capture.json').read_text())
        value['binding']['review_id'] = 8
        (first / 'capture.json').write_text(json.dumps(value))
    elif fault == 'unresolved':
        ledger['dispatch']['tasks']['1']['status']['status'] = 'checking'
    elif fault == 'foreign_worker':
        ledger['dispatch']['tasks']['1']['review_id'] = 8
    elif fault == 'pin':
        pin = tmp_path / '1/turns/1/retain-builds.json'
        pin.parent.mkdir(parents=True)
        pin.write_text('{}')
    else:
        (first / 'capture.json').unlink()
        (first / 'capture.json').symlink_to('/missing')
    collector = queued(tmp_path, ledger, monkeypatch)
    while collector._collect_batch_cache():
        pass
    assert (first / 'repo/.lake').exists()
    assert not (later / 'repo/.lake').exists()


def test_capture_and_empty_review_scans_drain_without_turn_budget(tmp_path, monkeypatch):
    donors = [cache(tmp_path, str(id)) for id in range(70)]
    for donor in donors[:-1]:
        (donor / 'capture.json').write_text('{}')
    collector = queued(tmp_path, state(1), monkeypatch)
    collector._collect()
    assert not (donors[-1] / 'repo/.lake').exists()
    assert all((donor / 'repo/.lake').exists() for donor in donors[:-1])


def test_completed_markers_only_skip_capture_reads(tmp_path, monkeypatch):
    donor = cache(tmp_path, 'done')
    (donor / 'build-caches-pruned.json').write_text('{"schema":5}')
    def forbidden(*args):
        raise AssertionError('completed donor capture was reparsed')
    monkeypatch.setattr(batch_cache, 'read', forbidden)
    collector = queued(tmp_path, state(1), monkeypatch)
    while collector._collect_batch_cache():
        pass
    # Marker bypass is skip-only, never permission to delete restored content.
    assert (donor / 'repo/.lake').exists()


def test_batch_and_regular_cleanup_both_drain(tmp_path, monkeypatch):
    donors = [cache(tmp_path, str(id)) for id in range(3)]
    collector = queued(tmp_path, state(1, 2, 3), monkeypatch)
    removed = []
    monkeypatch.setattr(storage, 'prune', lambda path, **kwargs: removed.append(path) or True)
    collector._collect()
    assert len(removed) == 7
    assert sum(path in donors for path in removed) == 3
    assert any(path in (tmp_path / '1', tmp_path / '2', tmp_path / '3') for path in removed)


def test_busy_cleanup_lease_preserves_scan_cursor(tmp_path, monkeypatch):
    cache(tmp_path, 'first')
    collector = queued(tmp_path, state(1), monkeypatch)
    with (tmp_path / 'build-cleanup.lock').open('a+b') as held:
        fcntl.flock(held, fcntl.LOCK_EX)
        collector._collect()
    assert collector._batch_iterator is None


def test_parent_alias_created_between_turns_is_retained(tmp_path, monkeypatch):
    cache(tmp_path, 'first')
    collector = queued(tmp_path, state(1), monkeypatch)
    iterator = storage._batch_candidates(tmp_path, ('7',))
    assert next(iterator) is None
    # Obtain a candidate as though yielded before a scheduler pause.
    candidate = next(iterator)
    iterator.close()
    original = tmp_path / 'batch-caches/7'
    elsewhere = tmp_path / 'retained'
    original.rename(elsewhere)
    original.symlink_to(elsewhere, target_is_directory=True)
    collector._batch_iterator = iter([candidate])
    while collector._collect_batch_cache():
        pass
    assert (elsewhere / 'first/repo/.lake').exists()


def test_batch_private_and_framework_all_drain_in_same_pass(tmp_path, monkeypatch):
    from trellis.dispatch import framework_retention
    donor = cache(tmp_path, 'first')
    collector = queued(tmp_path, state(1), monkeypatch)
    collector._terminal_tasks = {'1': {}}
    monkeypatch.setattr('trellis.dispatch.assignment_retention.collect',
                        lambda *args, **kwargs: {'archived': 0})
    class Retention:
        def retire_execution(self, *args, **kwargs):
            return {'removed': 0}
        retire_history = retire_execution
        retire_compiled_probes = retire_execution
        def retire(self, directory, observed, *, apply):
            return dict(status='retired', removed=20, pending=False)
    collector._terminal_retention = Retention()
    monkeypatch.setattr(storage, 'prune', lambda path, **kwargs: path == donor)
    calls = []
    monkeypatch.setattr(framework_retention, 'retire',
        lambda *args, **kwargs: calls.append(args[1]) or dict(removed=4))
    collector._collect()
    assert calls == [tmp_path / '1']
    assert collector.summary['private_files'] == 20
    assert collector.summary['framework_trees'] == 4
    assert collector.summary['batch_caches'] == 1


@pytest.mark.parametrize('change', ['active', 'different_review', 'missing_receipt'])
def test_resumed_cursor_uses_latest_native_snapshot(tmp_path, monkeypatch, change):
    donor = cache(tmp_path, 'first')
    collector = queued(tmp_path, state(1), monkeypatch)
    collector._batch_iterator = storage._batch_candidates(tmp_path, collector._batch_reviews)
    assert next(collector._batch_iterator) is None
    latest = state(1)
    if change == 'active':
        latest['dispatch']['tasks']['1']['status']['status'] = 'running'
    elif change == 'different_review':
        latest['dispatch']['tasks']['1']['review_id'] = 8
    else:
        latest['dispatch']['receipts'] = {}
    with monkeypatch.context() as patch:
        patch.setattr(threading.Thread, 'start', lambda _: None)
        assert not collector.schedule(latest)
    while collector._collect_batch_cache():
        pass
    assert (donor / 'repo/.lake').exists()


def test_new_review_cache_is_seen_after_old_cursor_exhausts(tmp_path, monkeypatch):
    cache(tmp_path, 'old')
    collector = queued(tmp_path, state(1), monkeypatch)
    assert collector._collect_batch_cache()
    fresh = cache(tmp_path, 'new', ('2',))
    target = tmp_path / 'batch-caches/8/new'
    target.parent.mkdir()
    fresh.rename(target)
    value = json.loads((target / 'capture.json').read_text())
    value['binding']['review_id'] = 8
    (target / 'capture.json').write_text(json.dumps(value))
    latest = state(1, 2)
    latest['dispatch']['tasks']['2']['review_id'] = 8
    latest['dispatch']['receipts']['8'] = {}
    with monkeypatch.context() as patch:
        patch.setattr(threading.Thread, 'start', lambda _: None)
        assert not collector.schedule(latest)
    while collector._collect_batch_cache():
        pass
    assert not (target / 'repo/.lake').exists()
