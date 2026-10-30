import json
from contextlib import contextmanager
from pathlib import Path
import threading

import pytest

from test_dispatch_auxiliary_retention import auxiliary_setup, targets
from test_dispatch_terminal_retention import setup, put
from trellis.dispatch import auxiliary_retention as auxiliary


def load(directory):
    return json.loads((directory / auxiliary.RECEIPT).read_text())


def test_success_compacts_counts_bindings_and_skip_only_fast_path(auxiliary_setup, monkeypatch):
    collector,directory,observed,_,_=auxiliary_setup
    paths=targets(directory);extra=put(directory/'repo/process-memory/unique.md',{'retain':'unique'})
    total=sum(p.stat().st_size for p in paths)
    assert auxiliary.retire(collector,directory,observed,apply=True)['removed']==6
    record=load(directory)
    assert record['completed'] is True and record['files']==[]
    assert record['retired_files']==6 and record['retired_bytes']==total and record['retained_files']==1
    assert record['checkpoint']['commit'] and record['bindings']['.']['source_head']
    before=(directory/auxiliary.RECEIPT).read_bytes()
    paths[0].write_text('new file at a previously retired path')
    witness=collector.witness();monkeypatch.setattr(collector,'witness',lambda:witness)
    def forbidden(*a,**kw):raise AssertionError('completed receipt must not rescan Git proof')
    monkeypatch.setattr(auxiliary.terminal,'_git',forbidden)
    monkeypatch.setattr(auxiliary,'_git_files',forbidden)
    monkeypatch.setattr(auxiliary,'_sync_dir',forbidden)
    for apply in (False,True):
        assert auxiliary.retire(collector,directory,observed,apply=apply)['status']=='unchanged'
    assert paths[0].read_text()=='new file at a previously retired path' and extra.exists()
    assert (directory/auxiliary.RECEIPT).read_bytes()==before


@pytest.mark.parametrize('failure',['second_unlink','fsync','after_last_unlink','cancel'])
def test_partial_failure_keeps_inventory_and_restart_compacts(auxiliary_setup,monkeypatch,failure):
    collector,directory,observed,_,_=auxiliary_setup;paths=targets(directory)
    original=Path.unlink;count=[0];sync=auxiliary._sync_dir
    def unlink(path,*args,**kw):
        if path in paths:
            count[0]+=1
            if failure=='second_unlink' and count[0]==2:raise OSError('interrupted unlink')
        result=original(path,*args,**kw)
        if path in paths and count[0]==6:
            if failure=='after_last_unlink':raise OSError('crashed after final unlink')
            if failure=='cancel':collector.stop_event=threading.Event();collector.stop_event.set()
        return result
    def fsync(path):
        if failure=='fsync':raise OSError('durability unavailable')
        sync(path)
    monkeypatch.setattr(Path,'unlink',unlink);monkeypatch.setattr(auxiliary,'_sync_dir',fsync)
    if failure=='cancel':assert auxiliary.retire(collector,directory,observed,apply=True)['status']=='cancelled'
    else:
        with pytest.raises(OSError):auxiliary.retire(collector,directory,observed,apply=True)
    record=load(directory);assert 'files' in record and 'completed' not in record
    monkeypatch.setattr(Path,'unlink',original);monkeypatch.setattr(auxiliary,'_sync_dir',sync)
    collector.stop_event=None
    before=(directory/auxiliary.RECEIPT).read_bytes()
    auxiliary.retire(collector,directory,observed,apply=False)
    assert (directory/auxiliary.RECEIPT).read_bytes()==before
    synced=[]
    monkeypatch.setattr(auxiliary,'_sync_dir',lambda p:(synced.append(p),sync(p)))
    auxiliary.retire(collector,directory,observed,apply=True)
    assert load(directory)['completed'] is True and not any(p.exists() for p in paths)
    assert set(p.parent for p in paths)<=set(synced)


@pytest.mark.parametrize('failure',['pin','pending','checkpoint','cancel','receipt_change','reintroduced'])
def test_completion_rechecks_after_unlinks_and_preserves_full_receipt(auxiliary_setup,monkeypatch,failure):
    collector,directory,observed,_,_=auxiliary_setup;paths=targets(directory);calls=[0]
    @contextmanager
    def lock():
        calls[0]+=1
        # Four natural roots, then the compact-record publication lease.
        if calls[0]==5:
            if failure=='pin':put(directory/'retain-builds.json',{})
            elif failure=='pending':monkeypatch.setattr(collector,'_owner_pending',lambda _:True)
            elif failure=='checkpoint':monkeypatch.setattr(collector,'checkpoint_current',lambda _:False)
            elif failure=='cancel':collector.stop_event=threading.Event();collector.stop_event.set()
            elif failure=='receipt_change':
                p=directory/auxiliary.RECEIPT;p.write_bytes(p.read_bytes()+b' ')
            else:paths[0].write_text('new bytes remain')
        yield True
    monkeypatch.setattr(collector,'retirement_lock',lock)
    if failure in ('pin','receipt_change'):
        with pytest.raises(ValueError):auxiliary.retire(collector,directory,observed,apply=True)
    else:auxiliary.retire(collector,directory,observed,apply=True)
    assert calls[0]==5 and 'files' in load(directory) and not load(directory).get('completed')
    if failure=='reintroduced':assert paths[0].read_text()=='new bytes remain'


def test_completed_record_cannot_rebind_owner(auxiliary_setup):
    collector,directory,observed,_,_=auxiliary_setup
    auxiliary.retire(collector,directory,observed,apply=True)
    record=load(directory);record['owner']['id']=999;put(directory/auxiliary.RECEIPT,record)
    with pytest.raises(ValueError):auxiliary.retire(collector,directory,observed,apply=True)


def test_no_inputs_or_receipt_keeps_fast_path(auxiliary_setup,monkeypatch):
    import shutil
    collector,directory,observed,_,_=auxiliary_setup
    for prefix in ('repo','repo/.trellis/supervisor/repo'):
        for surface in ('paper','process-memory'):
            shutil.rmtree(directory/prefix/surface)
    def forbidden(*a,**kw):raise AssertionError('absent scope must not load witness')
    monkeypatch.setattr(collector,'witness',forbidden)
    monkeypatch.setattr(auxiliary.retained,'terminal_owner_guard',forbidden)
    assert auxiliary.retire(collector,directory,observed,apply=True)['status']=='absent'


def test_completed_receipt_is_safe_for_previous_reader(auxiliary_setup):
    import subprocess,types
    collector,directory,observed,_,_=auxiliary_setup
    auxiliary.retire(collector,directory,observed,apply=True)
    path=targets(directory)[0];path.write_text('new file retained across older helper rollback')
    try:
        source=subprocess.check_output(['git','show','3f8c42b467a2326bb7ec58aa6c88f5fe89062d96:trellis/dispatch/auxiliary_retention.py'],text=True,stderr=subprocess.PIPE)
    except subprocess.CalledProcessError:
        pytest.skip('precompaction source object unavailable in shallow checkout')
    old=types.ModuleType('trellis.dispatch.auxiliary_retention_previous');old.__package__='trellis.dispatch'
    exec(compile(source,'previous_auxiliary_retention.py','exec'),old.__dict__)
    assert old.retire(collector,directory,observed,apply=True)['status']=='unchanged'
    assert path.read_text()=='new file retained across older helper rollback'
