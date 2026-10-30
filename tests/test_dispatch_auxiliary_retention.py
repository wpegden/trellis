from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import threading

import pytest

from test_dispatch_terminal_retention import setup, put, git
from trellis.dispatch import auxiliary_retention as auxiliary, terminal_retention as terminal, retained_files as retained, storage


@pytest.fixture(params=['worker', 'review', 'verifier'])
def auxiliary_setup(setup, request):
    collector, directory, _, _, state = setup
    kind = request.param
    if kind != 'worker':
        target = directory.with_name(f'{kind}-2');directory.rename(target);directory = target
        if kind == 'review':
            state['dispatch'].update(reviewer=None, receipts={'2':dict(review_id=2,response_sha256='a'*64,decisions=[])})
        else:
            state['dispatch']['verifiers']['jobs']['2'] = dict(id=2,identity={'lane':'sound'},
                status=dict(status='resolved',result_sha256='a'*64,stale_reason=None),request={'id':7})
    data = {'paper/paper.tex':b'Canonical paper\n', 'process-memory/INDEX.md':b'Entry index\n',
            'process-memory/global/pm-1.md':b'Historical settled finding\n'}
    for name,value in data.items():
        p=collector.repo/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(value)
    put(collector.repo/terminal.HISTORY,{'state':state})
    git(collector.repo,'add','.');git(collector.repo,'commit','-qm','Recoverable auxiliary files')
    prepared=json.loads((directory/'prepared.json').read_text())
    prepared.update(source_head=git(collector.repo,'rev-parse','HEAD'),request_id=2 if kind=='review' else 7)
    put(directory/'prepared.json',prepared)
    # Deliberately no auxiliary entries: this manifest proves nothing about them.
    put(directory/'base-manifest.json',{'Tablet/A.lean':'unrelated'})
    for prefix in ('repo','repo/.trellis/supervisor/repo'):
        for name,value in data.items():
            p=directory/prefix/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(value)
    private=directory/'repo';git(private,'init','-q');git(private,'add','paper','process-memory')
    # Real prepared clones have a committed source HEAD, including when their
    # sparse working index only contains the surfaces exercised by this test.
    git(private,'fetch','-q',str(collector.repo),prepared['source_head'])
    git(private,'update-ref','HEAD',prepared['source_head'])
    return collector,directory,terminal.terminal_owners(state),data,state


def targets(directory):
    return [directory/prefix/name for prefix in ('repo','repo/.trellis/supervisor/repo')
            for name in ('paper/paper.tex','process-memory/INDEX.md','process-memory/global/pm-1.md')]


def test_exact_git_matching_files_retire_for_every_supported_owner(auxiliary_setup):
    collector,directory,observed,data,_=auxiliary_setup
    paths=targets(directory);sizes=sum(p.stat().st_blocks*512 for p in paths)
    keep={p:p.read_bytes() for p in directory.rglob('*') if p.is_file() and p not in paths}
    assert not retained._untracked(paths[0])
    assert auxiliary.retire(collector,directory,observed)['files']==6
    result=auxiliary.retire(collector,directory,observed,apply=True)
    assert result['removed']==6 and result['removed_allocated_bytes']==sizes
    assert not any(p.exists() for p in paths)
    assert all(p.read_bytes()==b for p,b in keep.items())
    assert (directory/'repo/process-memory/global').is_dir()
    assert auxiliary.retire(collector,directory,observed,apply=True)['removed']==0
    head=json.loads((directory/'prepared.json').read_text())['source_head']
    for name,value in data.items():
        assert subprocess.check_output(['git','-C',str(collector.repo),'show',head+':'+name])==value


@pytest.mark.parametrize('difference',['extra','changed','same_size','mode','missing'])
def test_different_and_extra_files_stay_while_matching_files_retire(auxiliary_setup,difference):
    collector,directory,observed,_,_=auxiliary_setup
    path=directory/'repo/process-memory/global/pm-1.md'
    if difference=='extra':path=put(path.with_name('unique.md'),{'advice':'retain'})
    elif difference=='changed':path.write_text('unique private words')
    elif difference=='same_size':path.write_bytes(b'x'*path.stat().st_size)
    elif difference=='mode':path.chmod(0o755)
    else:path.unlink()
    before=path.read_bytes() if path.exists() else None
    result=auxiliary.retire(collector,directory,observed,apply=True)
    assert result['removed']==(6 if difference=='extra' else 5)
    if before is not None:assert path.read_bytes()==before
    assert result['retained_files']==(0 if difference=='missing' else 1)


@pytest.mark.parametrize('guard',['active','uncommitted','pin','live','cleanup','symlink','directory_alias','hardlink','other_project','invalid_head','publication'])
def test_auxiliary_owner_and_recovery_guards(auxiliary_setup,monkeypatch,guard):
    collector,directory,observed,_,_=auxiliary_setup;paths=targets(directory)
    if guard=='active':observed={}
    elif guard=='uncommitted':observed[directory.name]='other'
    elif guard=='pin':put(directory/'retain-builds.json',{})
    elif guard in ('live','cleanup'):
        put(directory/'execution/process.json',{'pid':123});put(directory/'execution/cleanup.json',{'pid':123 if guard=='live' else 456})
        monkeypatch.setattr('trellis.dispatch.process.alive',lambda _:guard=='live')
    elif guard=='symlink':paths[0].rename(paths[0].with_name('saved'));paths[0].symlink_to('saved')
    elif guard=='directory_alias':
        p=directory/'repo/process-memory';p.rename(p.with_name('saved'));p.symlink_to('saved')
    elif guard=='hardlink':os.link(paths[0],directory/'alias')
    elif guard in ('other_project','invalid_head'):
        p=directory/'prepared.json';v=json.loads(p.read_text());v['source_repo' if guard=='other_project' else 'source_head']=str(directory) if guard=='other_project' else 'f'*40;put(p,v)
    else:monkeypatch.setattr(collector,'_owner_pending',lambda _:True)
    if guard in ('active','uncommitted','publication'):
        assert auxiliary.retire(collector,directory,observed,apply=True)['removed']==0
    else:
        with pytest.raises((ValueError,subprocess.CalledProcessError)):
            auxiliary.retire(collector,directory,observed,apply=True)
    assert all(p.exists() for p in paths)


@pytest.mark.parametrize('change',['bytes','parents','prepared','pin','checkpoint','cancel'])
def test_final_queue_checks_preserve_inputs(auxiliary_setup,monkeypatch,change):
    collector,directory,observed,_,_=auxiliary_setup;paths=targets(directory)
    @contextmanager
    def locked():
        if change=='bytes':paths[0].write_text('changed after proof')
        elif change=='parents':
            p=paths[0].parent;p.rename(p.with_name('old-paper'));shutil.copytree(p.with_name('old-paper'),p)
        elif change=='prepared':
            p=directory/'prepared.json';p.write_bytes(p.read_bytes()+b' ')
        elif change=='pin':put(directory/'retain-builds.json',{})
        elif change=='checkpoint':monkeypatch.setattr(collector,'checkpoint_current',lambda _:False)
        else:collector.stop_event=threading.Event();collector.stop_event.set()
        yield True
    monkeypatch.setattr(collector,'retirement_lock',locked)
    if change in ('checkpoint','cancel'):
        assert auxiliary.retire(collector,directory,observed,apply=True)['removed']==0
    else:
        with pytest.raises(ValueError):auxiliary.retire(collector,directory,observed,apply=True)
    assert all(p.exists() for p in paths)


@pytest.mark.parametrize('change',['interrupt','pin','mutate_next'])
def test_partial_unlink_is_receipted_and_fsynced_outside_lock(auxiliary_setup,monkeypatch,change):
    collector,directory,observed,_,_=auxiliary_setup;paths=targets(directory)
    original=Path.unlink;removed=[];locked=[False];synced=[]
    @contextmanager
    def lock():
        locked[0]=True
        try:yield True
        finally:locked[0]=False
    def unlink(path,*args,**kwargs):
        if path in paths:
            if removed:raise OSError('interrupted second unlink')
            original(path,*args,**kwargs);removed.append(path)
            if change=='pin':put(directory/'retain-builds.json',{})
            if change=='mutate_next':paths[1].write_text('changed second entry')
        else:original(path,*args,**kwargs)
    def sync(path):assert not locked[0];synced.append(path)
    monkeypatch.setattr(collector,'retirement_lock',lock);monkeypatch.setattr(Path,'unlink',unlink);monkeypatch.setattr(auxiliary,'_sync_dir',sync)
    with pytest.raises((OSError,ValueError)):auxiliary.retire(collector,directory,observed,apply=True)
    assert len(removed)==1 and synced==[removed[0].parent] and (directory/auxiliary.RECEIPT).exists()
    monkeypatch.setattr(Path,'unlink',original)
    if change=='pin':(directory/'retain-builds.json').unlink()
    if change=='mutate_next':
        with pytest.raises(ValueError):auxiliary.retire(collector,directory,observed,apply=True)
        assert paths[1].exists()
    else:
        assert auxiliary.retire(collector,directory,observed,apply=True)['removed']==5
        assert not any(p.exists() for p in paths)


@pytest.mark.parametrize('change',['owner','escape','binding','proof','parents'])
def test_receipt_cannot_expand_or_rebind(auxiliary_setup,monkeypatch,change):
    collector,directory,observed,_,_=auxiliary_setup;paths=targets(directory)
    @contextmanager
    def busy():yield False
    monkeypatch.setattr(collector,'retirement_lock',busy)
    assert auxiliary.retire(collector,directory,observed,apply=True)['status']=='coordinator_busy'
    p=directory/auxiliary.RECEIPT;v=json.loads(p.read_text())
    if change=='owner':v['owner']['id']=999
    elif change=='escape':v['files'][0]['git_path']='../prepared.json'
    elif change=='binding':v['bindings']['.']['source_head']='f'*40
    elif change=='proof':v['files'][0]['proof']['oid']='f'*40
    else:v['files'][0]['parents']={}
    put(p,v)
    @contextmanager
    def acquired():yield True
    monkeypatch.setattr(collector,'retirement_lock',acquired)
    with pytest.raises(ValueError):auxiliary.retire(collector,directory,observed,apply=True)
    assert all(path.exists() for path in paths)


def test_background_auxiliary_scope_is_independent_of_source_scope(auxiliary_setup):
    collector,directory,_,_,state=auxiliary_setup
    daemon=storage.BackgroundCollector(collector.root,repo=collector.repo);daemon._review_snapshots=None
    assert daemon.schedule(state) and daemon.wait_idle(15)
    assert daemon.summary['auxiliary_files']==6 and daemon.summary['errors']==0
    assert not any(p.exists() for p in targets(directory))


def test_previous_worker_and_reviewer_handoffs_use_preserved_surfaces(auxiliary_setup,monkeypatch):
    from test_dispatch_transcripts import completed
    from trellis.dispatch.candidate import source_manifest
    from trellis.dispatch.workspace import prepare
    collector,directory,observed,data,_=auxiliary_setup
    saved=json.loads((directory/'prepared.json').read_text())
    kind='review' if directory.name.startswith('review-') else 'worker'
    completed(collector.root.parent,directory.name,saved['request_id'],role=kind)
    put(directory/'prepared.json',saved)
    monkeypatch.setattr('trellis.dispatch.process.alive',lambda _:False)
    advice=put(directory/'repo/.trellis/scratch/retained-note.md',{'exact':'old producer advice'})
    assert auxiliary.retire(collector,directory,observed,apply=True)['removed']==6
    config=put(collector.repo/'trellis.config.json',{'repo_path':str(collector.repo)})
    put(collector.root.parent/'runtime_metadata.json',{'repo_path':str(collector.repo),'config_path':str(config)})
    destination=collector.root/'99'
    previous={'previous':directory} if directory.name.isdigit() else {}
    prepare(collector.root.parent,collector.repo,config,destination,
        {'in_flight_request':{'id':99,'kind':'Worker','reviewer_comments':'Read .trellis/scratch/retained-note.md'}},
        expected_manifest=source_manifest(collector.repo),seed_packages=False,
        handoff_repo=directory/'repo',**previous)
    for name,value in data.items():assert (destination/'repo'/name).read_bytes()==value
    assert (destination/'repo/.trellis/scratch/retained-note.md').read_bytes()==advice.read_bytes()
    if previous:
        assert (destination/'repo/last_invalid/Tablet/A.lean').read_bytes()==(directory/'repo/Tablet/A.lean').read_bytes()
        from trellis.history_artifacts import LAST_INVALID_METADATA_FILENAME
        metadata=json.loads((destination/'repo/last_invalid'/LAST_INVALID_METADATA_FILENAME).read_text())
        assert metadata['report_available'] is True


from test_dispatch_workspace import project


def test_committed_batch_recovery_does_not_need_terminal_auxiliary_checkouts(project):
    from test_dispatch_batch_cache import make_batch
    from trellis.dispatch import batch_storage
    repo,_,_=project
    put(repo/'paper/paper.tex',{'paper':'canonical'})
    put(repo/'process-memory/global/pm-1.md',{'memory':'canonical'})
    git(repo,'add','paper','process-memory');git(repo,'commit','-qm','Add auxiliary inputs')
    driver,path,workers=make_batch(project)
    driver._materialize_batch(workers,path=path,committed=False)
    state=dict(dispatch=dict(tasks={str(id):dict(id=id,review_id=7,reserved_at_ms=1,
        assignment={'assignment_id':f'assignment-{id}'},status={'status':'resolved'}) for id in (1,2)},verifiers={'jobs':{}}))
    put(repo/terminal.HISTORY,{'state':state});git(repo,'add',terminal.HISTORY);git(repo,'commit','-qm','Resolve batch')
    put(driver.root/'coordinator.lock',{})
    collector=terminal.Collector(driver.root,repo)
    for id in ('1','2'):
        directory=driver.root/id
        assert auxiliary.retire(collector,directory,terminal.terminal_owners(state),apply=True)['removed']==2
        assert not (directory/'repo/paper/paper.tex').exists()
        batch_storage.validate_capture_seal(directory)
        batch_storage.adopt_capture(directory,workers[id])
    driver._materialize_batch(workers,path=path,committed=True)
    driver._materialize(workers,committed=True)


@pytest.mark.parametrize('change',['checkpoint','publication','cancel'])
def test_each_natural_root_rechecks_controls_before_next_lease(auxiliary_setup,monkeypatch,change):
    collector,directory,observed,_,_=auxiliary_setup;paths=targets(directory);leases=[]
    @contextmanager
    def lock():
        leases.append(len(leases)+1)
        if len(leases)==2:
            if change=='checkpoint':monkeypatch.setattr(collector,'checkpoint_current',lambda _:False)
            elif change=='publication':monkeypatch.setattr(collector,'_owner_pending',lambda _:True)
            else:collector.stop_event=threading.Event();collector.stop_event.set()
        yield True
    monkeypatch.setattr(collector,'retirement_lock',lock)
    result=auxiliary.retire(collector,directory,observed,apply=True)
    assert leases==[1,2] and result['removed']==1
    assert result['status']=={'checkpoint':'checkpoint_changed','publication':'publication_pending','cancel':'cancelled'}[change]
    assert not paths[0].exists() and all(path.exists() for path in paths[1:])
