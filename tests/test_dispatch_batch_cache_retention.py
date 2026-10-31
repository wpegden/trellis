from contextlib import contextmanager
import copy
import json
import os
from pathlib import Path

import pytest

from test_dispatch_terminal_retention import setup, put, git
from test_dispatch_terminal_batches import batch, stored, current
from trellis.dispatch import batch_cache, batch_cache_retention as r, terminal_batches as b
from trellis.dispatch import terminal_retention as terminal
from trellis.dispatch.candidate import canonical_json, digest
from trellis.dispatch.native import NativeLedger
from trellis.dispatch.publication import atomic


@pytest.fixture
def cache(batch):
    c,state,ack=batch;stored(batch)
    original=json.loads(ack.read_text());refs=original['prepared']['workers']
    b.retire(c,2,current(state),apply=True)
    root=c.root/'batch-caches/2'/original['base_sha256']
    files={}
    for name,data in {'Tablet/A.lean':b'derived source','reference/context.txt':b'frozen context',
                      '.trellis/supervisor/repo/.trellis/artifacts/state.json':b'{"derived":true}',
                      '.lake/config/lakefile.olean':b'derived config'}.items():
        atomic(root/'repo'/name,data);files[name]=dict(bytes=len(data),sha256=digest(data))
    files['.lake/build/pruned.olean']=dict(bytes=7,sha256=digest(b'old cached build'))
    capture=dict(format=batch_cache.FORMAT,binding=dict(base_sha256=root.name,review_id=2,
        receipt=state['dispatch']['receipts']['2'],workers=refs),files=files,sources={},
        allocated_copy_bytes=4096,inodes=10,seeded_oleans=0)
    put(root/'capture.json',capture)
    put(root/'build-caches-pruned.json',{'schema':5})
    return c,state,ack,root,files


def run(cache,**kwargs):
    c,state,_,_,_=cache
    return r.retire(c,2,current(state),apply=True,**kwargs)


def test_terminal_cache_retirement_keeps_ack_and_worker_evidence_then_actual_native_replay(cache):
    c,state,ack,root,files=cache
    preserved={p:p.read_bytes() for p in (c.root/'1').rglob('*') if p.is_file()}
    ack_before=ack.read_bytes();prune_before=(root/'build-caches-pruned.json').read_bytes()
    result=run(cache)
    assert result['caches']==1 and result['removed_files']==5 and result['removed_directories']>0
    assert not (root/'repo').exists() and not (root/'capture.json').exists()
    assert root.is_dir() and (root/'build-caches-pruned.json').read_bytes()==prune_before
    assert ack.read_bytes()==ack_before and all(p.read_bytes()==v for p,v in preserved.items())
    ledger=NativeLedger(c.root.parent,kernel=lambda _:pytest.fail('native RPC'),
        materialize=lambda *a,**kw:pytest.fail('materialize'),materialize_batch=lambda *a,**kw:pytest.fail('cache donor'))
    ledger._require_recovered_publication=lambda:None
    value=ledger._dispatch_batch(2,{'assignments':[]},[],{},state=state)
    assert value['terminal'] is True and value['receipt']==state['dispatch']['receipts']['2']
    assert run(cache)['removed_files']==0


def test_ready_worker_hydration_never_reads_retired_donor(cache):
    c,state,ack,root,files=cache
    ref={'path':str(root.relative_to(c.root)),'sha256':digest(canonical_json(json.loads((root/'capture.json').read_text())))}
    owner=c.root/'1';prepared=json.loads((owner/'prepared.json').read_text());prepared['deferred_build_cache']=ref
    put(owner/'prepared.json',prepared);put(owner/'build-cache-ready.json',{'capture':ref})
    run(cache)
    before=(owner/'repo/Tablet/A.lean').read_bytes()
    batch_cache.hydrate(owner,c.root,None)
    assert (owner/'repo/Tablet/A.lean').read_bytes()==before


@pytest.mark.parametrize('change',['bytes','size','extra','hardlink','symlink'])
def test_changed_unknown_and_linked_members_are_preserved(cache,change):
    c,state,ack,root,files=cache;path=root/'repo/Tablet/A.lean'
    if change=='bytes':path.write_bytes(b'changed source')
    elif change=='size':path.write_bytes(b'unique')
    elif change=='extra':path=atomic(root/'repo/Tablet/Unique.lean',b'unique') or root/'repo/Tablet/Unique.lean'
    elif change=='hardlink':os.link(path,root/'keeper')
    else:path.rename(root/'keeper');path.symlink_to(root/'keeper')
    before=path.read_bytes();run(cache)
    assert path.read_bytes()==before
    assert not (root/'capture.json').exists()


@pytest.mark.parametrize('change',['active','receipt','worker_identity','review_active','git_state','uncompacted'])
def test_without_exact_terminal_ack_and_current_git_authority_no_deletion(cache,change):
    c,state,ack,root,files=cache
    if change=='active':state['dispatch']['tasks']['1']['status']['status']='running'
    elif change=='receipt':state['dispatch']['receipts']['2']['response_sha256']='f'*64
    elif change=='worker_identity':state['dispatch']['tasks']['1']['reserved_at_ms']+=1
    elif change=='review_active':state['dispatch']['reviewer']={'request_id':2}
    elif change=='git_state':
        old=copy.deepcopy(state);old['dispatch']['tasks']['1']['status']['status']='running'
        put(c.repo/terminal.HISTORY,{'state':old});git(c.repo,'add','.');git(c.repo,'commit','-qm','Not terminal')
    else:put(ack,{'base_sha256':'b'*64,'prepared':{}})
    try:assert run(cache)['removed_files']==0
    except ValueError:pass
    assert (root/'repo/Tablet/A.lean').exists() and (root/'capture.json').exists()


@pytest.mark.parametrize('change',['worker_pin','review_pin','cache_pin','live','reap','pending','source','alias','capture_hardlink'])
def test_existing_owner_guards_and_capture_paths_remain(cache,monkeypatch,change):
    c,state,ack,root,files=cache
    if change=='worker_pin':put(c.root/'1/retain-builds.json',{})
    elif change=='review_pin':put(ack.parent/'retain-state.json',{})
    elif change=='cache_pin':put(root/'retain-builds.json',{})
    elif change in ('live','reap'):
        put(c.root/'1/execution/process.json',{'pid':1});put(c.root/'1/execution/cleanup.json',{'pid':1 if change=='live' else 2})
        monkeypatch.setattr('trellis.dispatch.process.alive',lambda _:change=='live')
    elif change=='pending':
        put(c.root/'publication/pending.json',{'identity':'c'*64});put(c.root/'publication'/('c'*64)/'record.json',{'task_id':1})
    elif change=='source':put(c.root/'review-2/prepared.json',{'source_repo':str(root),'source_head':'c'*40,'request_id':2})
    elif change=='alias':
        original=root/'repo/Tablet';original.rename(root/'saved');original.symlink_to(root/'saved',target_is_directory=True)
    else:os.link(root/'capture.json',root/'capture-keeper')
    with pytest.raises((ValueError,RuntimeError)):run(cache)
    assert (root/'repo/Tablet/A.lean').exists() and (root/'capture.json').exists()


@pytest.mark.parametrize('change',['base','review','workers','receipt','path','git','packages','duplicate'])
def test_capture_cannot_redirect_deletion(cache,change):
    c,state,ack,root,files=cache;path=root/'capture.json';value=json.loads(path.read_text())
    if change=='base':value['binding']['base_sha256']='c'*64
    elif change=='review':value['binding']['review_id']=3
    elif change=='workers':value['binding']['workers']['1']['sha256']='c'*64
    elif change=='receipt':value['binding']['receipt']['response_sha256']='c'*64
    elif change in ('path','git','packages'):
        key={'path':'../outside','git':'.git/config','packages':'.lake/packages/a/file'}[change]
        value['files'][key]={'bytes':0,'sha256':digest(b'')}
    else:
        path.write_bytes(b'{"format":1,"format":2}');value=None
    if value is not None:put(path,value)
    with pytest.raises(ValueError):run(cache)
    assert (root/'repo/Tablet/A.lean').exists()


def test_partial_unlinks_and_directory_sync_failure_resume_without_new_archive(cache,monkeypatch):
    c,state,ack,root,files=cache;original=Path.unlink;removed=[]
    def crash(path,*args,**kwargs):
        if path.is_relative_to(root/'repo'):
            if removed:raise RuntimeError('crash')
            removed.append(path)
        original(path,*args,**kwargs)
    with monkeypatch.context() as m:
        m.setattr(Path,'unlink',crash)
        with pytest.raises(RuntimeError,match='crash'):run(cache)
    assert (root/'capture.json').exists() and len(removed)==1
    run(cache)
    assert not (root/'capture.json').exists() and not (root/'repo').exists()


@pytest.mark.parametrize('change',['gate','checkpoint','prepared','capture','ack','pin','cancel'])
def test_queued_lease_changes_preserve_cache(cache,monkeypatch,change):
    c,state,ack,root,files=cache;allowed=True
    def admission():
        if not allowed:raise ValueError('reader stopped')
    @contextmanager
    def lock():
        nonlocal allowed
        if change=='gate':allowed=False
        elif change=='checkpoint':monkeypatch.setattr(c,'checkpoint_current',lambda _:False)
        elif change=='prepared':put(c.root/'review-2/prepared.json',{})
        elif change=='capture':put(root/'capture.json',{})
        elif change=='ack':put(ack,{})
        elif change=='pin':put(c.root/'1/retain-builds.json',{})
        else:monkeypatch.setattr(c,'cancelled',lambda:True)
        yield True
    monkeypatch.setattr(c,'retirement_lock',lock)
    with pytest.raises((ValueError,InterruptedError)):run(cache,runtime_admission=admission)
    assert (root/'repo/Tablet/A.lean').exists()


def test_new_file_in_directory_survives_atomic_rmdir(cache,monkeypatch):
    c,state,ack,root,files=cache;original=Path.rmdir;new=root/'repo/Tablet/new-evidence'
    def create(path):
        if path==new.parent:atomic(new,b'operator evidence')
        original(path)
    monkeypatch.setattr(Path,'rmdir',create)
    run(cache)
    assert new.read_bytes()==b'operator evidence'


@pytest.mark.parametrize('enabled',[False,True])
def test_actual_background_hook_is_separate_optin_and_keeps_old_build_marker_from_bypassing(cache,monkeypatch,enabled):
    from trellis.dispatch.storage import BackgroundCollector
    c,state,ack,root,files=cache
    admissions=[];worker=BackgroundCollector(c.root,repo=c.repo,retire_terminal_batch_caches=enabled,
        terminal_batch_admission=lambda:admissions.append(1),git_maintenance_threshold_bytes=None)
    monkeypatch.setattr(worker,'_collect_owner',lambda _:None)
    try:
        assert worker.schedule(state) and worker.wait_idle(20)
        assert worker.summary['errors']==0
        assert worker.summary['terminal_batch_caches']==int(enabled)
        assert worker.summary['terminal_batch_cache_files']==5*int(enabled)
        assert bool(admissions)==enabled and (root/'capture.json').exists()!=enabled
    finally:worker.close()


def test_directory_fsync_failure_keeps_capture_for_safe_restart(cache,monkeypatch):
    c,state,ack,root,files=cache;original=r._sync_dir
    def fail(path):
        if path.is_relative_to(root/'repo'):raise OSError('directory sync failed')
        original(path)
    with monkeypatch.context() as m:
        m.setattr(r,'_sync_dir',fail)
        with pytest.raises(OSError,match='sync failed'):run(cache)
    assert (root/'capture.json').exists()
    assert not any((root/'repo'/name).exists() for name in files)
    assert run(cache)['caches']==1
    assert not (root/'capture.json').exists() and not (root/'repo').exists()


@pytest.mark.parametrize('change',['file','pin'])
def test_mutation_after_first_unlink_is_refused(cache,monkeypatch,change):
    c,state,ack,root,files=cache;original=Path.unlink;count=0
    target=root/'repo/reference/context.txt'
    def mutate(path,*args,**kwargs):
        nonlocal count
        original(path,*args,**kwargs)
        if path.is_relative_to(root/'repo'):
            count+=1
            if count==1:
                if change=='file':target.write_bytes(b'unique new evidence')
                else:put(c.root/'1/retain-builds.json',{})
    monkeypatch.setattr(Path,'unlink',mutate)
    with pytest.raises(ValueError):run(cache)
    assert (root/'capture.json').exists()
    if change=='file':assert target.read_bytes()==b'unique new evidence'


def test_uncompacted_large_capture_is_skip_only_not_full_payload_read(cache,monkeypatch):
    c,state,ack,root,files=cache
    with ack.open('wb') as stream:stream.truncate(b.MAX_METADATA_BYTES+1)
    original=r._read
    def read(path,limit):
        if path==ack:pytest.fail('large original batch was read')
        return original(path,limit)
    monkeypatch.setattr(r,'_read',read)
    assert run(cache)['status']=='no_terminal_ack'
    assert (root/'capture.json').exists()


def test_pending_retry_uses_worker_evidence_and_terminal_ack_not_original_donor(cache):
    c,state,ack,root,files=cache
    task=state['dispatch']['tasks']['1']
    task['status']['outcome']={'kind':'stuck','reason':'ordinary retry'}
    task['pending_retry']={'last_invalid':'preserved native retry context'}
    record=json.loads(ack.read_text());record.update(b.observation(state,2));put(ack,record)
    put(c.repo/terminal.HISTORY,{'state':state});git(c.repo,'add','.');git(c.repo,'commit','-qm','Terminal predecessor awaiting retry')
    before={p:p.read_bytes() for p in (c.root/'1').rglob('*') if p.is_file()}
    run(cache)
    assert all(p.read_bytes()==data for p,data in before.items())
    ledger=NativeLedger(c.root.parent,kernel=lambda _:pytest.fail('native'),
        materialize_batch=lambda *a,**kw:pytest.fail('rehydrate terminal donor'))
    assert ledger._dispatch_batch(2,{'assignments':[]},[],{},state=state)['terminal']


def test_optin_rejects_non_boolean(tmp_path):
    from trellis.dispatch.storage import BackgroundCollector
    with pytest.raises(ValueError):BackgroundCollector(tmp_path,retire_terminal_batch_caches=1)


def test_each_directory_group_rechecks_reader_admission_and_can_resume(cache,monkeypatch):
    c,state,ack,root,files=cache;real=c.retirement_lock;leases=0;compatible=True
    def admission():
        if not compatible:raise ValueError('reader replaced between groups')
    @contextmanager
    def lock():
        nonlocal leases,compatible
        leases+=1
        if leases==2:compatible=False
        with real() as acquired:yield acquired
    with monkeypatch.context() as m:
        m.setattr(c,'retirement_lock',lock)
        with pytest.raises(ValueError,match='between groups'):run(cache,runtime_admission=admission)
    assert not (root/'repo/Tablet/A.lean').exists()
    assert (root/'repo/reference/context.txt').exists() and (root/'capture.json').exists()
    compatible=True
    assert run(cache,runtime_admission=admission)['caches']==1


def test_large_flat_directory_yields_without_capping_total_work(cache,monkeypatch):
    c,state,ack,root,files=cache;record=json.loads((root/'capture.json').read_text())
    for number in range(r.FILES_PER_LEASE*2+1):
        name=f'flat/{number}.json';path=root/'repo'/name
        atomic(path,b'donor');record['files'][name]={'bytes':5,'sha256':digest(b'donor')}
    put(root/'capture.json',record)
    real=c.retirement_lock;unlink=Path.unlink;sections=[];active=False
    @contextmanager
    def lock():
        nonlocal active
        with real() as acquired:
            active=acquired;sections.append(0)
            try:yield acquired
            finally:active=False
    def remove(path,*args,**kwargs):
        if path.is_relative_to(root/'repo'):
            assert active
            sections[-1]+=1
        return unlink(path,*args,**kwargs)
    monkeypatch.setattr(c,'retirement_lock',lock);monkeypatch.setattr(Path,'unlink',remove)
    result=run(cache)
    assert max(sections)<=r.FILES_PER_LEASE
    assert sum(sections)==r.FILES_PER_LEASE*2+5
    assert result['caches']==1 and not (root/'repo').exists()


def test_nested_git_tracking_added_after_preparation_prevents_unlink(cache, monkeypatch):
    c, state, ack, root, files = cache
    for key in list(os.environ):
        if key.startswith('GIT_'):
            monkeypatch.delenv(key)
    real = c.retirement_lock
    target = root / 'repo/Tablet/A.lean'
    @contextmanager
    def lock():
        with real() as acquired:
            git(root / 'repo', 'init')
            git(root / 'repo', 'add', str(target))
            yield acquired
    monkeypatch.setattr(c, 'retirement_lock', lock)
    with pytest.raises(ValueError, match='changed before unlink'):
        run(cache)
    assert target.read_bytes() == b'derived source'
    assert (root / 'capture.json').exists()
