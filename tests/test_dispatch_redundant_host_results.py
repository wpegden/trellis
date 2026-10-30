from contextlib import contextmanager
import json
import os
from pathlib import Path

import pytest
from test_dispatch_terminal_retention import setup, put
from trellis.dispatch import redundant_host_result_retention as copies, retained_files as retained
from trellis.dispatch.candidate import digest


def host(directory, relative='acceptance/1', *, value=None):
    attempt=directory/relative
    put(attempt/'prepared.json',json.loads((directory/'prepared.json').read_text()))
    value=value or {'ok':True,'response':{'summary':'exact retained diagnostics'*50000},'errors':[]}
    path=put(attempt/'runtime/acceptance-result.json',value)
    response=put(attempt/'execution/response.json',dict(value,checked_sources={'A':'sha'}))
    put(attempt/'execution/result.json',{'returncode':0,'response_sha256':digest(response.read_bytes())})
    return path,response


@pytest.mark.parametrize('archive',[False,True])
def test_exact_duplicate_removed_only_with_durable_completed_host_counterpart(setup,archive):
    collector,directory,observed,_,_=setup
    selected=[host(directory),host(directory,'acceptance/1/finalization/2')]
    excluded=[host(directory,'turns/1'),host(directory,'acceptance/latest'),host(directory,'finalization/1')]
    originals={r:r.read_bytes() for _,r in selected+excluded}
    for _,response in selected:
        if archive: assert retained.archive(response)
    assert copies.collect(collector,directory,observed,apply=False)['eligible']==2
    result=copies.collect(collector,directory,observed)
    assert result['removed']==2 and result['removed_allocated_bytes']>2000000
    assert all(not p.exists() for p,_ in selected)
    assert all(p.exists() for p,_ in excluded)
    assert all(retained.read_bytes(p)==data for p,data in originals.items())
    assert copies.collect(collector,directory,observed)['removed']==0


@pytest.mark.parametrize('failure',['missing','incomplete','hash','returncode','different','boolean_integer','extra','duplicate_keys','nonfinite','oversize','archive_corrupt'])
def test_unverified_or_semantically_distinct_results_retained(setup,monkeypatch,failure):
    collector,directory,observed,_,_=setup
    path,response=host(directory,value={'ok':True,'response':{'n':1}})
    if failure=='missing':response.unlink()
    elif failure=='incomplete':(response.parent/'result.json').unlink()
    elif failure=='hash':put(response.parent/'result.json',{'returncode':0,'response_sha256':'wrong'})
    elif failure=='returncode':put(response.parent/'result.json',{'returncode':1,'response_sha256':digest(response.read_bytes())})
    elif failure=='different':put(path,{'ok':False})
    elif failure=='boolean_integer':put(path,{'ok':1})
    elif failure=='extra':put(path,{'missing':'unique'})
    elif failure=='duplicate_keys':path.write_text('{"ok":false,"ok":true}')
    elif failure=='nonfinite':path.write_text('{"value":NaN}')
    elif failure=='oversize':monkeypatch.setattr(copies,'MAX_BYTES',2)
    elif failure=='archive_corrupt':
        put(response,{'ok':True,'response':{'n':1},'padding':'repeat'*300000})
        put(response.parent/'result.json',{'returncode':0,'response_sha256':digest(response.read_bytes())})
        assert retained.archive(response)
        retained._paths(response)[0].write_bytes(b'bad gzip')
    original=path.read_bytes()
    assert copies.collect(collector,directory,observed)['removed']==0
    assert path.read_bytes()==original


@pytest.mark.parametrize('failure',['active','uncommitted','pending','pin','host_pin','live','unreaped','tracked','project','alias','hardlink','nonworker'])
def test_existing_terminal_and_path_guards_preserve_diagnostic(setup,monkeypatch,failure):
    collector,directory,observed,_,_=setup
    path,response=host(directory)
    if failure=='active':observed={}
    elif failure=='uncommitted':observed={'1':'different'}
    elif failure=='pending':monkeypatch.setattr(collector,'_owner_pending',lambda _:True)
    elif failure=='pin':put(directory/'retain-builds.json',{})
    elif failure=='host_pin':put(path.parent.parent/'retain-builds.json',{})
    elif failure in ('live','unreaped'):
        put(response.parent/'process.json',{'pid':12})
        if failure=='live':put(response.parent/'cleanup.json',{'pid':12})
        monkeypatch.setattr('trellis.dispatch.process.alive',lambda _:failure=='live')
    elif failure=='tracked':monkeypatch.setattr(retained,'_untracked',lambda _:False)
    elif failure=='project':put(path.parent.parent/'prepared.json',{'source_repo':str(directory)})
    elif failure=='alias':
        original=path.with_name('original');path.rename(original);path.symlink_to(original)
    elif failure=='hardlink':path.with_name('alias').hardlink_to(path)
    elif failure=='nonworker':
        old=directory;directory=directory.with_name('review-1');old.rename(directory);path=directory/path.relative_to(old)
        observed={'review-1':observed['1']}
    original=path.read_bytes()
    try: result=copies.collect(collector,directory,observed)
    except (ValueError,FileNotFoundError):pass
    else:assert result['removed']==0
    assert path.read_bytes()==original


@pytest.mark.parametrize('change',['pin','response','completion','diagnostic','prepared','cancel','pending','checkpoint','busy'])
def test_actual_final_lease_rechecks_changed_evidence(setup,monkeypatch,change):
    collector,directory,observed,_,_=setup
    path,response=host(directory)
    original=collector.retirement_lock
    @contextmanager
    def lease():
        if change=='pin':put(directory/'retain-builds.json',{})
        elif change=='response':response.write_text('{}')
        elif change=='completion':put(response.parent/'result.json',{})
        elif change=='diagnostic':path.write_text('{"unique":true}')
        elif change=='prepared':put(path.parent.parent/'prepared.json',{})
        elif change=='cancel':monkeypatch.setattr(collector,'cancelled',lambda:True)
        elif change=='pending':monkeypatch.setattr(collector,'_owner_pending',lambda _:True)
        elif change=='checkpoint':monkeypatch.setattr(collector,'checkpoint_current',lambda _:False)
        if change=='busy':yield False
        else:
            with original() as acquired:yield acquired
    monkeypatch.setattr(collector,'retirement_lock',lease)
    try:result=copies.collect(collector,directory,observed)
    except ValueError:pass
    else:assert result['removed']==0
    assert path.exists()


def test_comparison_and_preserved_response_fsync_happen_offlock(setup,monkeypatch):
    collector,directory,observed,_,_=setup
    path,response=host(directory);held=False
    original=collector.retirement_lock
    @contextmanager
    def lease():
        nonlocal held
        with original() as acquired:
            held=True
            try:yield acquired
            finally:held=False
    monkeypatch.setattr(collector,'retirement_lock',lease)
    read=copies._read
    def checked_read(p):
        assert not held
        return read(p)
    monkeypatch.setattr(copies,'_read',checked_read)
    assert copies.collect(collector,directory,observed)['removed']==1


def test_background_hook_removes_duplicate_after_host_archive(setup,monkeypatch):
    from trellis.dispatch.storage import BackgroundCollector
    collector,directory,observed,_,state=setup
    path,response=host(directory)
    monkeypatch.setattr('trellis.dispatch.auxiliary_retention.retire',lambda *a,**k:{'removed':0})
    monkeypatch.setattr('trellis.dispatch.worker_source_retention.retire',lambda *a,**k:{'removed':0})
    background=BackgroundCollector(collector.root,repo=collector.repo,git_maintenance_threshold_bytes=None)
    try:
        assert background.schedule(state) and background.wait_idle(30)
        assert background.summary['errors']==0
        assert background.summary['redundant_host_results']==1
        assert not path.exists() and not response.exists()
        assert retained.read_bytes(response)
    finally:background.close()


def test_preparation_swap_after_source_repo_read_cannot_become_new_baseline(setup,monkeypatch):
    collector,directory,observed,_,_=setup
    path,response=host(directory)
    prepared=path.parent.parent/'prepared.json'
    read=copies._json
    def replace_after_read(p):
        value=read(p)
        if p==prepared: put(prepared,{'source_repo':str(directory)})
        return value
    monkeypatch.setattr(copies,'_json',replace_after_read)
    with pytest.raises(ValueError,match='inputs changed'):
        copies.collect(collector,directory,observed)
    assert path.exists()
