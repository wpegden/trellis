import copy
from contextlib import contextmanager
import json,os,threading,time
from pathlib import Path
import pytest
from test_dispatch_terminal_retention import setup,put,git
from trellis.dispatch import retry_retention as r, terminal_retention as terminal
from trellis.dispatch.candidate import canonical_json,digest
from trellis.dispatch.native import NativeLedger
from trellis.dispatch.storage import BackgroundCollector


def commit(c,state):
    put(c.repo/terminal.HISTORY,{'state':state});git(c.repo,'add','.');git(c.repo,'commit','-qm','Retry custody')
    put(c.root.parent/'protocol_state.json',state)

@pytest.fixture
def retry(setup):
    c,owner,_,_,state=setup
    prior=state['dispatch']['tasks']['1'];prior.update(pending_retry=None,retry_of=None,retry_refusal=None)
    child=copy.deepcopy(prior);child.update(id=2,retry_of=1,reserved_at_ms=13)
    child['assignment']['base_snapshot']='c'*64;state['dispatch']['tasks']['2']=child
    commit(c,state)
    for key in ('1','2'):
        put(c.root/key/'prepared.json',dict(source_repo=str(c.repo),source_head=git(c.repo,'rev-parse','HEAD'),request_id=int(key)))
    assignment=dict(assignment_id=child['assignment']['assignment_id'],base_snapshot='c'*64,request={'large':'immutable request'*2000})
    saved=dict(base_sha256='b'*64,prepared=dict(task_id=2,worker=dict(assignment=assignment,private_state={'large':'private state'*2000}),event=dict(kind='retry_prepared',predecessor=1,assignment=assignment,refusal=None,graph={'large':'historical graph'*2000},now_ms=13)))
    path=put(c.root/'retries/1/prepared.json',saved)
    return c,state,path,r.Collector(c)

def current(state):return lambda:r.observations(state).get('1')
def run(retry,**kwargs):
    c,state,path,collector=retry
    return collector.retire(1,current(state),apply=True,**kwargs)

def refusal(retry):
    c,state,path,collector=retry
    del state['dispatch']['tasks']['2'];state['dispatch']['tasks']['1']['retry_refusal']='inputs changed'
    value=json.loads(path.read_bytes());value['prepared'].update(task_id=None,worker=None)
    value['prepared']['event'].update(assignment=None,refusal='inputs changed');put(path,value);commit(c,state)

@pytest.mark.parametrize('refused',[False,True])
def test_consumed_preparation_deleted_without_archive_and_actual_retry_reader_skips_it(retry,refused):
    c,state,path,collector=retry
    if refused:refusal(retry)
    preserved={p:p.read_bytes() for owner in (c.root/'1',c.root/'2') for p in owner.rglob('*') if p.is_file()}
    info=path.stat();result=run(retry)
    assert result['removed']==1 and result['removed_allocated_bytes']==info.st_blocks*512
    assert not path.exists() and list(path.parent.iterdir())==[]
    assert all(p.read_bytes()==data for p,data in preserved.items())
    ledger=NativeLedger(c.root.parent,kernel=lambda _:pytest.fail('native retry call'),materialize=lambda _:pytest.fail('materialize'))
    ledger._require_recovered_publication=lambda:None
    assert ledger.prepare_retries(c.repo)==[]
    assert run(retry)['status']=='absent'

@pytest.mark.parametrize('change',['pending','child_active','child_pending','no_child','two_children','wrong_retry_of','refusal_and_child','uncommitted','bool_id'])
def test_live_or_uncommitted_retry_custody_retains_original(retry,change):
    c,state,path,collector=retry;before=path.read_bytes();tasks=state['dispatch']['tasks']
    if change=='pending':tasks['1']['pending_retry']={}
    elif change=='child_active':tasks['2']['status']['status']='running'
    elif change=='child_pending':tasks['2']['pending_retry']={}
    elif change=='no_child':del tasks['2']
    elif change=='two_children':tasks['3']=copy.deepcopy(tasks['2']);tasks['3']['id']=3
    elif change=='wrong_retry_of':tasks['2']['retry_of']=3
    elif change=='refusal_and_child':tasks['1']['retry_refusal']='different event'
    elif change=='bool_id':tasks['1']['id']=True
    else:
        old=copy.deepcopy(state);old['dispatch']['tasks']['1']['pending_retry']={};commit(c,old)
    assert run(retry)['removed']==0 and path.read_bytes()==before

@pytest.mark.parametrize('payload',[b'not json',b'{"legacy":"unknown","duplicate":1,"duplicate":2}',b''])
@pytest.mark.parametrize('pending',[False,True])
def test_fixed_consumed_cache_payload_is_never_read_but_pending_is_preserved(retry,monkeypatch,payload,pending):
    c,state,path,collector=retry;path.write_bytes(payload)
    sibling=path.with_name('unknown.json');sibling.write_bytes(b'preserved unique sibling')
    if pending:
        state['dispatch']['tasks']['1']['pending_retry']={};commit(c,state)
    original=Path.open
    def open_file(self,*args,**kwargs):
        if self==path:pytest.fail('derived retry payload must never be opened')
        return original(self,*args,**kwargs)
    monkeypatch.setattr(Path,'open',open_file)
    assert run(retry)['removed']==int(not pending)
    assert path.exists() is pending
    assert sibling.read_bytes()==b'preserved unique sibling'

@pytest.mark.parametrize('change',['gate','state','git','prepared','file','pin','cancel','pending_publication'])
def test_final_lease_revalidates_custody_and_owned_inputs(retry,monkeypatch,change):
    c,state,path,collector=retry;allowed=True
    def admission():
        if not allowed:raise ValueError('reader changed')
    collector.admission=admission
    @contextmanager
    def lock():
        nonlocal allowed
        if change=='gate':allowed=False
        elif change=='state':state['dispatch']['tasks']['1']['pending_retry']={}
        elif change=='git':monkeypatch.setattr(c,'checkpoint_current',lambda _:False)
        elif change=='prepared':put(c.root/'2/prepared.json',{'source_repo':'elsewhere'})
        elif change=='file':path.write_text(path.read_text()+' ')
        elif change=='pin':put(path.parent/'retain-state.json',{})
        elif change=='cancel':monkeypatch.setattr(c,'cancelled',lambda:True)
        else:monkeypatch.setattr(c,'_same_task_pending',lambda _:True)
        yield True
    monkeypatch.setattr(c,'retirement_lock',lock)
    try:assert run(retry)['removed']==0
    except (ValueError,RuntimeError):pass
    assert path.exists()

@pytest.mark.parametrize('change',['alias','hardlink','owner_pin','owner_live','owner_unreaped','owner_source'])
def test_path_owner_and_process_guards(retry,monkeypatch,change):
    c,state,path,collector=retry
    if change=='alias':path.rename(path.with_suffix('.saved'));path.symlink_to(path.with_suffix('.saved'))
    elif change=='hardlink':os.link(path,path.with_suffix('.keeper'))
    elif change=='owner_pin':put(c.root/'2/retain-builds.json',{})
    elif change=='owner_source':put(c.root/'2/prepared.json',dict(source_repo='/other',source_head='c'*40))
    else:
        put(c.root/'2/execution/process.json',{'pid':1})
        if change=='owner_live':put(c.root/'2/execution/cleanup.json',{'pid':1})
        monkeypatch.setattr('trellis.dispatch.process.alive',lambda _:change=='owner_live')
    with pytest.raises((ValueError,RuntimeError,FileNotFoundError)):run(retry)
    assert path.exists()


def test_fsync_failure_keeps_other_evidence_and_restart_is_idempotent(retry,monkeypatch):
    c,state,path,collector=retry;source=c.root/'1/repo/Tablet/A.lean';before=source.read_bytes()
    monkeypatch.setattr(r,'_sync_dir',lambda _:(_ for _ in ()).throw(OSError('sync failed')))
    with pytest.raises(OSError):run(retry)
    assert not path.exists() and source.read_bytes()==before
    assert run(retry)['status']=='absent'

@pytest.mark.parametrize('enabled',[False,True])
def test_actual_background_default_off_and_enabled_queue(retry,monkeypatch,enabled):
    c,state,path,_=retry
    background=BackgroundCollector(c.root,repo=c.repo,retire_retry_preparations=enabled,git_maintenance_threshold_bytes=None)
    monkeypatch.setattr(background,'_collect_owner',lambda _:None);background._cache_enabled=False
    background.schedule(state)
    try:assert background.wait_idle(10)
    finally:background.close()
    assert path.exists() is (not enabled)
    assert background.summary['retry_preparation_files']==int(enabled)


def test_refusal_arriving_during_existing_drain_is_processed(retry,monkeypatch):
    c,state,path,_=retry;initial=copy.deepcopy(state);initial['dispatch']['tasks']['1']['pending_retry']={}
    initial['dispatch']['tasks'].pop('2');commit(c,initial)
    background=BackgroundCollector(c.root,repo=c.repo,retire_retry_preparations=True,git_maintenance_threshold_bytes=None)
    hold=threading.Event();entered=threading.Event()
    def owner(_):entered.set();hold.wait(10)
    monkeypatch.setattr(background,'_collect_owner',owner);background._cache_enabled=False
    background.schedule(initial);assert entered.wait(5)
    try:
        refusal(retry);assert background.schedule(state) is False
        deadline=time.monotonic()+8
        while path.exists() and time.monotonic()<deadline:time.sleep(.01)
        assert not path.exists()
    finally:hold.set();background.close();background.wait_idle(10)


def test_explicit_opt_in_and_admission_types(tmp_path):
    for value in (1,None,'true'):
        with pytest.raises(ValueError):BackgroundCollector(tmp_path,retire_retry_preparations=value)
    with pytest.raises(ValueError):BackgroundCollector(tmp_path,retry_preparation_admission=True)


def test_pending_materialize_commit_crash_window_uses_saved_preparation_then_can_retire(retry):
    c,state,path,collector=retry
    child=state['dispatch']['tasks'].pop('2')
    state['dispatch']['tasks']['1']['pending_retry']={'attempts':1}
    commit(c,state)
    saved=json.loads(path.read_bytes());saved['base_sha256']=digest(canonical_json(state));put(path,saved)
    assert run(retry)['removed']==0 and path.exists()
    materialized=[];events=[]
    def kernel(payload):
        assert payload['action']=='dispatch_control'
        events.append(payload['event'])
        state['dispatch']['tasks']['1']['pending_retry']=None
        state['dispatch']['tasks']['2']=child
        put(c.root.parent/'protocol_state.json',state)
        return dict(status='dispatch_ok',output={})
    ledger=NativeLedger(c.root.parent,kernel=kernel,materialize=materialized.append)
    ledger._require_recovered_publication=lambda:None
    ledger.observe_sources=lambda *_:dict(graph={},input_hashes={},manifest={})
    answer=ledger.prepare_retries(c.repo,now_ms=999)
    assert answer==[saved['prepared']] and materialized==[{'2':saved['prepared']['worker']}]
    assert events==[saved['prepared']['event']]
    commit(c,state)
    assert run(retry)['removed']==1
    assert ledger.prepare_retries(c.repo)==[]
