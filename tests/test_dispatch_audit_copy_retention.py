import json
import os
from contextlib import contextmanager

import pytest

from test_dispatch_terminal_retention import setup, put, git
from trellis.dispatch import audit_copy_retention as audit, storage


def copies(setup, attempts=('', 'acceptance/1', 'acceptance/1/finalization/1')):
    collector, directory, _, _, _ = setup
    head = git(collector.repo, 'rev-parse', 'HEAD')
    paths = []
    canonical = put(collector.repo / audit.SURFACE / 'cycle-7/note.md', {'evidence': 'original'})
    for relative in attempts:
        attempt = directory / relative
        put(attempt / 'prepared.json', dict(source_repo=str(collector.repo), source_head=head, request_id=7))
        paths.append(put(attempt / 'repo' / audit.SURFACE / 'cycle-7/note.md', {'evidence': 'original'}))
    return paths, canonical


def test_terminal_copies_removed_original_unique_and_other_evidence_preserved(setup):
    collector, directory, observed, _, _ = setup
    paths, canonical = copies(setup)
    keep = [canonical, put(directory / 'repo' / audit.SURFACE / 'cycle-7/unique.md', {'unique': True}),
            put(directory / 'repo/.trellis/scratch/notes.md', {'needed': True}),
            put(directory / 'acceptance/diagnostic/repo' / audit.SURFACE / 'cycle-7/note.md', {})]
    before = {p: p.read_bytes() for p in keep}
    allocated = sum(p.stat().st_blocks * 512 for p in paths)
    assert audit.retire(collector, directory, observed)['files'] == 3
    result = audit.retire(collector, directory, observed, apply=True)
    assert result['removed'] == 3 and result['removed_allocated_bytes'] == allocated
    assert result['retained_files'] == 1
    assert all(not p.exists() for p in paths)
    assert all(p.read_bytes() == value for p, value in before.items())
    record = json.loads((directory / audit.RECEIPT).read_text())
    assert record['completed'] and record['files'] == [] and record['retired_files'] == 3
    assert audit.retire(collector, directory, observed, apply=True)['removed'] == 0


@pytest.mark.parametrize('change', ['active', 'identity', 'pin', 'live', 'cleanup', 'tracked',
    'private_alias', 'canonical_alias', 'canonical_parent_alias', 'private_hardlink',
    'canonical_hardlink', 'missing', 'different', 'foreign_prepared', 'publication'])
def test_guards_and_canonical_mismatch_preserve_copies(setup, monkeypatch, change):
    collector, directory, observed, _, _ = setup
    paths, canonical = copies(setup, ('',))
    if change == 'active': observed = {}
    elif change == 'identity': observed['1'] = 'other'
    elif change == 'pin': put(directory / 'retain-builds.json', {})
    elif change in ('live', 'cleanup'):
        put(directory / 'execution/process.json', {'pid': 123})
        put(directory / 'execution/cleanup.json', {'pid': 123 if change == 'live' else 456})
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: change == 'live')
    elif change == 'tracked':
        git(directory / 'repo', 'init', '-q'); git(directory / 'repo', 'add', audit.SURFACE)
    elif change in ('private_alias', 'canonical_alias'):
        path = paths[0] if change == 'private_alias' else canonical
        path.rename(path.with_name('saved')); path.symlink_to('saved')
    elif change == 'canonical_parent_alias':
        parent = canonical.parent; parent.rename(parent.with_name('saved')); parent.symlink_to('saved')
    elif change in ('private_hardlink', 'canonical_hardlink'):
        os.link(paths[0] if change == 'private_hardlink' else canonical, directory / 'alias')
    elif change == 'missing': canonical.unlink()
    elif change == 'different': canonical.write_text('different evidence')
    elif change == 'foreign_prepared':
        prepared = json.loads((directory / 'prepared.json').read_text()); prepared['source_repo'] = str(directory)
        put(directory / 'prepared.json', prepared)
    elif change == 'publication':
        put(collector.root / 'publication/pending.json', {'identity': 'a' * 64})
        put(collector.root / 'publication' / ('a' * 64) / 'record.json', {'task_id': 1})
    if change in ('pin', 'live', 'cleanup', 'private_alias', 'canonical_alias',
                  'canonical_parent_alias', 'private_hardlink', 'canonical_hardlink', 'foreign_prepared'):
        with pytest.raises(ValueError): audit.retire(collector, directory, observed, apply=True)
    else:
        assert audit.retire(collector, directory, observed, apply=True)['removed'] == 0
    assert paths[0].exists()


@pytest.mark.parametrize('name', ['review-1', 'verifier-1', 'exclusive-1'])
def test_nonworkers_never_retire_audit_handoff_sources(setup, name):
    collector, directory, observed, _, _ = setup
    paths, _ = copies(setup, ('',))
    target = directory.with_name(name); directory.rename(target)
    if name.startswith('exclusive'):
        with pytest.raises(ValueError, match='unsupported'):
            audit.retire(collector, target, observed, apply=True)
    else:
        assert audit.retire(collector, target, observed, apply=True)['removed'] == 0
    assert (target / paths[0].relative_to(directory)).exists()


@pytest.mark.parametrize('change', ['rewrite', 'missing', 'alias'])
def test_canonical_change_while_queued_preserves_private_copy_and_restart(setup, monkeypatch, change):
    collector, directory, observed, _, _ = setup
    paths, canonical = copies(setup, ('',)); original = canonical.read_bytes()
    actual_lock = collector.retirement_lock
    @contextmanager
    def lock():
        if change == 'rewrite': canonical.write_text('changed original')
        elif change == 'missing': canonical.unlink()
        else:
            canonical.rename(canonical.with_name('saved')); canonical.symlink_to('saved')
        yield True
    monkeypatch.setattr(collector, 'retirement_lock', lock)
    if change == 'alias':
        with pytest.raises(ValueError): audit.retire(collector, directory, observed, apply=True)
    else:
        assert audit.retire(collector, directory, observed, apply=True)['removed'] == 0
    assert paths[0].exists()
    monkeypatch.setattr(collector, 'retirement_lock', actual_lock)
    if canonical.is_symlink(): canonical.unlink()
    canonical.write_bytes(original)
    assert audit.retire(collector, directory, observed, apply=True)['removed'] == 1


def test_pin_mid_root_stops_next_unlink_and_restart_finishes(setup, monkeypatch):
    collector, directory, observed, _, _ = setup
    paths, canonical = copies(setup, ('',))
    second = put(paths[0].with_name('second.md'), {'second': True})
    put(canonical.with_name('second.md'), {'second': True})
    real = type(second).unlink
    def unlink(path, *args, **kwargs):
        result = real(path, *args, **kwargs)
        if path == paths[0]: put(directory / 'retain-builds.json', {})
        return result
    monkeypatch.setattr(type(second), 'unlink', unlink)
    with pytest.raises(ValueError, match='pin'):
        audit.retire(collector, directory, observed, apply=True)
    assert not paths[0].exists() and second.exists()
    monkeypatch.setattr(type(second), 'unlink', real)
    (directory / 'retain-builds.json').unlink()
    assert audit.retire(collector, directory, observed, apply=True)['removed'] == 1


def test_cancelled_before_final_fence_and_tampered_receipt(setup, monkeypatch):
    collector, directory, observed, _, _ = setup
    paths, canonical = copies(setup, ('',))
    @contextmanager
    def lock():
        monkeypatch.setattr(collector, 'cancelled', lambda: True)
        yield True
    monkeypatch.setattr(collector, 'retirement_lock', lock)
    assert audit.retire(collector, directory, observed, apply=True)['status'] == 'cancelled'
    assert paths[0].exists()
    monkeypatch.setattr(collector, 'cancelled', lambda: False)
    marker = directory / audit.RECEIPT; value = json.loads(marker.read_text())
    value['files'][0]['path'] = 'repo/.trellis/scratch/evidence'
    put(marker, value)
    with pytest.raises(ValueError, match='escapes'):
        audit.retire(collector, directory, observed, apply=True)


def test_receipt_digest_cannot_substitute_for_actual_private_bytes(setup, monkeypatch):
    collector, directory, observed, _, _ = setup
    paths, canonical = copies(setup, ('',))
    actual_lock = collector.retirement_lock
    @contextmanager
    def busy():
        yield False
    monkeypatch.setattr(collector, 'retirement_lock', busy)
    audit.retire(collector, directory, observed, apply=True)
    # A restart inventory is not independent authority to discard different
    # private evidence merely because a supplied signature matches its stat.
    paths[0].write_text('unique changed private evidence')
    from trellis.dispatch import retained_files
    marker = directory / audit.RECEIPT; value = json.loads(marker.read_text())
    value['files'][0]['signature'] = list(retained_files._signature(paths[0].stat()))
    put(marker, value)
    monkeypatch.setattr(collector, 'retirement_lock', actual_lock)
    with pytest.raises(ValueError, match='copy changed'):
        audit.retire(collector, directory, observed, apply=True)
    assert paths[0].exists() and canonical.exists()


@pytest.mark.parametrize('changed', ['prepared', 'private'])
def test_changed_private_preparation_or_bytes_at_fence_preserved(setup, monkeypatch, changed):
    collector, directory, observed, _, _ = setup
    paths, _ = copies(setup, ('',))
    @contextmanager
    def lock():
        (directory / 'prepared.json' if changed == 'prepared' else paths[0]).write_text('changed')
        yield True
    monkeypatch.setattr(collector, 'retirement_lock', lock)
    with pytest.raises(ValueError, match='changed'):
        audit.retire(collector, directory, observed, apply=True)
    assert paths[0].exists()


def test_real_background_hook_counts_audit_retirement(setup):
    collector, directory, observed, _, state = setup
    paths, canonical = copies(setup, ('',))
    background = storage.BackgroundCollector(collector.root, repo=collector.repo)
    try:
        assert background.schedule(state)
        assert background.wait_idle(30)
        assert background.summary['audit_copy_files'] == 1
        assert not paths[0].exists() and canonical.exists()
    finally:
        background.close()
