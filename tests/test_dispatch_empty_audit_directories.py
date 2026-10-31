import errno
import json
from contextlib import contextmanager
from pathlib import Path

import pytest

from test_dispatch_terminal_retention import setup, put
from test_dispatch_audit_copy_retention import copies as old_copies
from trellis.dispatch import audit_copy_retention as audit, artifacts, storage


def copies(setup, attempts=('', 'acceptance/1', 'acceptance/1/finalization/1')):
    paths, original = old_copies(setup, attempts)
    for path in [*paths, original]:
        path.parent.rename(path.parent.with_name('cycle-7-request-7'))
    return [p.parent.with_name('cycle-7-request-7') / p.name for p in paths], original.parent.with_name('cycle-7-request-7') / original.name


def test_completed_file_receipts_do_not_hide_empty_descendants_or_change_evidence(setup):
    collector, directory, observed, _, _ = setup
    paths, original = copies(setup)
    unique = put(paths[0].parent / 'unique.json', {'meaningful': True})
    before = {p: p.read_bytes() for p in (original, unique)}
    empty = []
    for path in paths:
        nested = path.parent / 'nested/deep'
        nested.mkdir(parents=True)
        empty.extend([nested, nested.parent])
    audit.retire(collector, directory, observed, apply=True)
    assert json.loads((directory / audit.RECEIPT).read_bytes())['completed']
    expected = sum(p.stat().st_blocks * 512 for p in empty + [p.parent for p in paths[1:]])
    result = audit.retire_empty_directories(collector, directory, observed, apply=True)
    assert result['removed_directories'] == 8
    assert result['removed_allocated_bytes'] == expected
    assert all(not p.exists() for p in empty)
    assert all((attempt / 'repo' / audit.SURFACE).is_dir() for attempt in audit.roots(directory))
    assert all(p.read_bytes() == data for p, data in before.items())
    assert artifacts.probe_bytes(directory / 'repo', str(unique.relative_to(directory / 'repo'))) == before[unique]
    assert artifacts.probe_bytes(collector.repo, str(original.relative_to(collector.repo))) == before[original]
    assert unique.parent.is_dir()
    assert audit.retire_empty_directories(collector, directory, observed, apply=True)['removed_directories'] == 0


@pytest.mark.parametrize('change', ['active', 'pin', 'live', 'cleanup', 'publication', 'checkpoint', 'prepared', 'root_alias', 'parent_alias', 'cancel'])
def test_admission_changes_before_lease_preserve_directories(setup, monkeypatch, change):
    collector, directory, observed, _, _ = setup
    paths, _ = copies(setup, ('',))
    empty = paths[0].parent / 'empty'; empty.mkdir()
    if change in ('live', 'cleanup'):
        monkeypatch.setattr('trellis.dispatch.process.alive',lambda _:change=='live')
    actual = collector.retirement_lock
    @contextmanager
    def lock():
        if change == 'pin':put(directory / 'retain-builds.json', {})
        elif change in ('live', 'cleanup'):
            put(directory/'execution/process.json', {'pid': 123})
            if change == 'live':put(directory/'execution/cleanup.json', {'pid': 123})
        elif change == 'publication':monkeypatch.setattr(collector, '_owner_pending', lambda _: True)
        elif change == 'checkpoint':monkeypatch.setattr(collector, 'checkpoint_current', lambda _: False)
        elif change == 'prepared':put(directory / 'prepared.json', {'changed': True})
        elif change in ('root_alias', 'parent_alias'):
            parent = paths[0].parent if change == 'parent_alias' else paths[0].parent.parent
            parent.rename(parent.with_name(parent.name+'-saved'));parent.symlink_to(parent.name+'-saved')
        elif change == 'cancel':monkeypatch.setattr(collector, 'cancelled', lambda: True)
        with actual() as acquired:yield acquired
    monkeypatch.setattr(collector, 'retirement_lock', lock)
    if change == 'active':observed = {}
    if change in ('pin', 'live', 'cleanup', 'prepared', 'root_alias', 'parent_alias'):
        with pytest.raises((ValueError, FileNotFoundError)):audit.retire_empty_directories(collector, directory, observed, apply=True)
    else:
        assert audit.retire_empty_directories(collector, directory, observed, apply=True)['removed_directories'] == 0
    assert empty.exists()


def test_new_file_race_is_retained_by_atomic_rmdir(setup, monkeypatch):
    collector, directory, observed, _, _ = setup
    paths, _ = copies(setup, ('',));empty = paths[0].parent / 'empty';empty.mkdir()
    actual = Path.rmdir
    def raced(path):
        if path == empty:put(path / 'new-evidence.json', {'keep': True})
        return actual(path)
    monkeypatch.setattr(Path, 'rmdir', raced)
    result = audit.retire_empty_directories(collector, directory, observed, apply=True)
    assert result['removed_directories'] == 0
    assert json.loads((empty / 'new-evidence.json').read_bytes()) == {'keep': True}


def test_mid_root_pin_stops_further_removal(setup, monkeypatch):
    collector, directory, observed, _, _ = setup
    paths, _ = copies(setup, ('',));parent=paths[0].parent
    for name in ('a', 'b'):(parent/name).mkdir()
    actual=Path.rmdir;removed=[]
    def pin_after(path):
        actual(path);removed.append(path);put(directory/'retain-builds.json', {})
    monkeypatch.setattr(Path,'rmdir',pin_after)
    with pytest.raises(ValueError,match='operator pin'):
        audit.retire_empty_directories(collector,directory,observed,apply=True)
    assert len(removed)==1
    assert sum((parent/name).is_dir() for name in ('a','b'))==1


@pytest.mark.parametrize('owner', ['review-1', 'verifier-1'])
def test_review_and_verifier_roots_are_preserved(setup, owner):
    collector,directory,observed,_,_=setup
    paths,_=copies(setup, ('',));(paths[0].parent/'empty').mkdir()
    target=directory.with_name(owner);directory.rename(target)
    result=audit.retire_empty_directories(collector,target,{owner:observed['1']},apply=True)
    assert result['status']=='nonworker_preserved'
    assert (target/'repo'/audit.SURFACE/'cycle-7-request-7/empty').is_dir()


def test_unrecognized_empty_directory_and_source_roots_remain(setup):
    collector,directory,observed,_,_=setup
    paths,_=copies(setup, ('',))
    keep=[paths[0].parent.parent/'operator-notes/empty',directory/'repo/Tablet/empty']
    for path in keep:path.mkdir(parents=True)
    empty=paths[0].parent/'empty';empty.mkdir()
    assert audit.retire_empty_directories(collector,directory,observed,apply=True)['removed_directories']==1
    assert all(p.is_dir() for p in keep)


def test_background_processes_completed_file_receipt_and_counts_directories(setup):
    collector,directory,observed,_,state=setup
    paths,original=copies(setup, ('',))
    empty=paths[0].parent/'nested/deep';empty.mkdir(parents=True)
    audit.retire(collector,directory,observed,apply=True)
    background=storage.BackgroundCollector(collector.root,repo=collector.repo)
    try:
        assert background.schedule(state) and background.wait_idle(30)
        assert background.summary['audit_empty_directories']==3
        assert background.summary['audit_copy_files']==0
        assert background.summary['errors']==0
        assert (directory/'repo'/audit.SURFACE).is_dir() and original.exists()
    finally:background.close()
