"""One lock handoff per terminal owner, with unchanged per-file guards."""
from contextlib import contextmanager
import json
import fcntl
from pathlib import Path
import threading
import time

import pytest

from test_dispatch_terminal_retention import setup, put
from trellis.dispatch import terminal_retention as retention
from trellis.dispatch import retained_files as retained


def observe_lock(collector, monkeypatch):
    state = dict(held=False, acquisitions=0, syncs=[], hold_seconds=[])
    original_lock = collector.retirement_lock
    @contextmanager
    def locked():
        with original_lock() as acquired:
            if acquired:
                state['acquisitions'] += 1
                state['held'] = True
                started = time.monotonic()
            try:
                yield acquired
            finally:
                if acquired:
                    state['hold_seconds'].append(time.monotonic() - started)
                state['held'] = False
    monkeypatch.setattr(collector, 'retirement_lock', locked)
    original_display = collector._display
    def display(*args, **kwargs):
        assert not state['held'], 'display parsing/writes must remain off-lock'
        return original_display(*args, **kwargs)
    monkeypatch.setattr(collector, '_display', display)
    original_sync = retention._sync_dir
    def sync(path):
        assert not state['held'], 'directory fsync must remain off-lock'
        with (collector.root / 'coordinator.lock').open('rb') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        state['syncs'].append(path)
        return original_sync(path)
    monkeypatch.setattr(retention, '_sync_dir', sync)
    return state


def test_twenty_files_use_one_lock_with_display_and_fsync_outside(setup, monkeypatch):
    collector, directory, observed, paths, _ = setup
    replay = directory / 'repo/.trellis/kernel-replay-environments'
    extra = [put(replay / (format(i, '064x') + '.json'), {'input': i}) for i in range(17)]
    watched = observe_lock(collector, monkeypatch)
    original_unlink = Path.unlink
    removed = []
    def unlink(path, *args, **kwargs):
        if path in (*paths, *extra):
            assert watched['held']
            # Every display is durable before the first input disappears.
            assert (directory / 'runtime/display-state.json').is_file()
            removed.append(path)
        return original_unlink(path, *args, **kwargs)
    monkeypatch.setattr(Path, 'unlink', unlink)
    result = collector.retire(directory, observed, apply=True)
    assert result['removed'] == len(removed) == 20
    assert watched['acquisitions'] == 1
    assert set(watched['syncs']) == {path.parent for path in removed}
    assert len(watched['syncs']) == len(set(watched['syncs']))
    print(json.dumps({'files': 20, 'lock_hold_seconds': watched['hold_seconds'][0]}))


@pytest.mark.parametrize('fault', ['signature', 'pin', 'publication', 'checkpoint', 'tracked', 'cancel'])
def test_mid_batch_change_preserves_remaining_files_and_flushes_partial_work(setup, monkeypatch, fault):
    collector, directory, observed, paths, _ = setup
    collector.retire(directory, observed, apply=True, budget={'files': 0})
    record = json.loads((directory / retention.RECEIPT).read_bytes())
    first, second, third = [directory / item['path'] for item in record['files']]
    watched = observe_lock(collector, monkeypatch)
    collector.stop_event = threading.Event()
    original_unlink = Path.unlink
    original_untracked = retained._untracked
    original_checkpoint = collector.checkpoint_current
    removed = []
    def unlink(path, *args, **kwargs):
        result = original_unlink(path, *args, **kwargs)
        if path == first:
            removed.append(path)
            if fault == 'signature':
                second.write_text('changed after first unlink')
            elif fault == 'pin':
                put(directory / 'retain-builds.json', {})
            elif fault == 'publication':
                put(collector.root / 'publication/pending.json', {'identity': 'b' * 64})
                put(collector.root / ('publication/' + 'b' * 64 + '/record.json'), {'task_id': 1})
            elif fault == 'cancel':
                collector.stop_event.set()
        return result
    monkeypatch.setattr(Path, 'unlink', unlink)
    monkeypatch.setattr(retained, '_untracked', lambda path:
        False if fault == 'tracked' and removed and path == second else original_untracked(path))
    monkeypatch.setattr(collector, 'checkpoint_current', lambda witness:
        False if fault == 'checkpoint' and removed else original_checkpoint(witness))
    if fault in ('tracked', 'cancel'):
        result = collector.retire(directory, observed, apply=True)
        assert result['removed'] == (2 if fault == 'tracked' else 1)
        assert result['status'] == ('retired' if fault == 'tracked' else 'cancelled')
    else:
        with pytest.raises(ValueError):
            collector.retire(directory, observed, apply=True)
    assert not first.exists() and second.exists()
    assert third.exists() == (fault != 'tracked')
    assert watched['acquisitions'] == 1
    assert first.parent in watched['syncs']
    assert (directory / retention.RECEIPT).read_bytes() == retention.canonical_json(record)


def test_bounded_partial_batch_restarts_with_one_new_lock(setup, monkeypatch):
    collector, directory, observed, paths, _ = setup
    watched = observe_lock(collector, monkeypatch)
    budget = {'files': 1}
    first = collector.retire(directory, observed, apply=True, budget=budget)
    assert first['removed'] == 1 and first['pending'] and budget == {'files': 0}
    assert watched['acquisitions'] == 1
    receipt = (directory / retention.RECEIPT).read_bytes()
    second = collector.retire(directory, observed, apply=True)
    assert second['removed'] == 2 and watched['acquisitions'] == 2
    assert not any(path.exists() for path in paths)
    assert (directory / retention.RECEIPT).read_bytes() == receipt


def test_all_dirty_parents_are_flushed_after_one_fsync_failure(setup, monkeypatch):
    collector, directory, observed, paths, _ = setup
    parents = sorted({path.parent for path in paths})
    flushed = []
    def sync(path):
        with (collector.root / 'coordinator.lock').open('rb') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        flushed.append(path)
        if path == parents[0]:
            raise OSError('fixture fsync failure')
    monkeypatch.setattr(retention, '_sync_dir', sync)
    with pytest.raises(OSError, match='fixture fsync failure'):
        collector.retire(directory, observed, apply=True)
    assert flushed == parents and not any(path.exists() for path in paths)
    assert (directory / retention.RECEIPT).exists()
    assert collector.retire(directory, observed, apply=True)['status'] == 'absent'
