from contextlib import contextmanager
import copy
import json
import os
from pathlib import Path

import pytest

from test_dispatch_terminal_retention import setup, put, git
from test_dispatch_worker_source_retention import worker_setup
from test_dispatch_accepted_source_retention import accepted, checkpoint
from trellis.dispatch import candidate, publication_retention as retention, terminal_retention as terminal
from trellis.dispatch.publication import Publisher, atomic
from trellis.dispatch.publication_artifacts import paths


@pytest.fixture
def published(accepted):
    c, owner, observed, state = accepted
    artifact = b'compiled checked cache'
    environment = b'{"portable":"checked environment"}'
    files = dict.fromkeys(paths('A'))
    files['lib/lean/Tablet/A.olean'] = candidate.digest(artifact)
    event = {'task_id': 1, 'candidate_sha256': state['dispatch']['publications']['1']['candidate_sha256'], 'response': {'summary':'Accepted ∀ n ≥ 0'}}
    receipt = state['dispatch']['publications']['1']
    receipt['payload_sha256'] = retention._native_payload_sha(event)
    record = dict(version=2, task_id=1, candidate_sha256=receipt['candidate_sha256'], repo=str(c.repo),
        before={}, after={}, changes={}, kernel_event=event,
        artifacts=dict(version=1, nodes={'A': dict(source_closure_sha256='f'*64, artifact_bundle=[], files=files)},
                       portable_environments=[candidate.digest(environment)]))
    raw = candidate.canonical_json(record)
    folder = c.root/'publication'/candidate.digest(raw)
    atomic(folder/'record.json', raw)
    put(folder/'committed.json', receipt)
    targets = {folder/'artifacts/lib/lean/Tablet/A.olean':artifact,
               folder/f'.trellis/kernel-replay-environments/{candidate.digest(environment)}.json':environment}
    for p,data in targets.items():atomic(p,data)
    atomic(folder/'files/Tablet/A.lean', b'accepted source')
    atomic(folder/'artifacts/unlisted', b'unknown future evidence')
    observed = checkpoint(c,state)
    return c,owner,observed,state,folder,targets


def run(data, *, apply=True, admission=None):
    c,owner,observed,state,folder,targets=data
    return retention.Collector(c,admission).retire(owner,observed,
        lambda:terminal.publication_cache_candidates(state).get('1'),apply=apply)


def test_exact_optional_cache_retirement_preserves_journal_sources_and_native_receipt(published):
    c,owner,observed,state,folder,targets=published
    preserved={p:p.read_bytes() for p in folder.rglob('*') if p.is_file() and p not in targets}
    assert run(published,apply=False)['candidates']==len(targets)
    result=run(published)
    assert result['removed']==2 and result['journals']==1
    assert result['removed_allocated_bytes']-result['marker_allocated_bytes']==sum(4096 for _ in targets)-4096
    assert all(not p.exists() for p in targets)
    assert all(p.read_bytes()==value for p,value in preserved.items())
    marker=json.loads((folder/retention.MARKER).read_text())
    assert marker['complete'] and marker['receipt']==state['dispatch']['publications']['1']
    assert run(published)['removed']==0


@pytest.mark.parametrize('change',['active','no_receipt','retry','child_retry','event','payload'])
def test_current_and_git_terminal_receipt_required(published,change):
    c,owner,observed,state,folder,targets=published
    task=state['dispatch']['tasks']['1']
    if change=='active':task['status']['status']='checking'
    elif change=='no_receipt':state['dispatch']['publications'].clear()
    elif change=='retry':task['pending_retry']={}
    elif change=='child_retry':
        child=copy.deepcopy(task);child.update(id=2,retry_of=1);state['dispatch']['tasks']['2']=child
    elif change=='event':state['dispatch']['publications']['1']['event_id']=42
    else:state['dispatch']['publications']['1']['payload_sha256']='b'*64
    assert run(published)['removed']==0
    assert all(p.exists() for p in targets)


@pytest.mark.parametrize('change',['pending','pin','live','cleanup','prepared','corrupt','symlink','hardlink','parent_alias'])
def test_fail_closed_before_any_retirement(published,monkeypatch,change):
    c,owner,observed,state,folder,targets=published;p=next(iter(targets))
    if change=='pending':put(c.root/'publication/pending.json',{'identity':folder.name})
    elif change=='pin':put(owner/'acceptance/1/retain-builds.json',{})
    elif change=='live':monkeypatch.setattr('trellis.dispatch.process.alive',lambda _:True)
    elif change=='cleanup':(owner/'execution/cleanup.json').unlink()
    elif change=='prepared':
        v=json.loads((owner/'prepared.json').read_text());v['source_repo']=str(owner);put(owner/'prepared.json',v)
    elif change=='corrupt':p.write_bytes(b'wrong')
    elif change=='symlink':p.rename(p.with_name('saved'));p.symlink_to('saved')
    elif change=='hardlink':os.link(p,folder/'alias')
    else:p.parent.rename(p.parent.with_name('saved'));p.parent.symlink_to('saved',target_is_directory=True)
    with pytest.raises((ValueError,FileNotFoundError)):run(published)
    assert all(p.exists() for p in targets)


def test_partial_unlink_crash_replays_only_exact_journal_and_resyncs(published,monkeypatch):
    c,owner,observed,state,folder,targets=published
    original=Path.unlink;removed=[]
    def crash(path,*args,**kwargs):
        if path in targets and removed:raise RuntimeError('crash')
        original(path,*args,**kwargs)
        if path in targets:removed.append(path)
    with monkeypatch.context() as m:
        m.setattr(Path,'unlink',crash)
        with pytest.raises(RuntimeError,match='crash'):run(published)
    assert len(removed)==1 and not json.loads((folder/retention.MARKER).read_text())['complete']
    assert run(published)['removed']==1
    assert json.loads((folder/retention.MARKER).read_text())['complete']


def test_queued_runtime_gate_and_pin_stop_before_destructive_lease(published):
    calls=[]
    def admission():
        calls.append(1)
        if len(calls)==3:raise ValueError('reader stopped')
    with pytest.raises(ValueError,match='reader stopped'):run(published,admission=admission)
    assert all(p.exists() for p in published[-1])
    assert run(published)['removed']==2


def test_pin_inserted_after_first_unlink_stops_second(published,monkeypatch):
    original=Path.unlink;c,owner,observed,state,folder,targets=published
    def insert(path,*args,**kwargs):
        original(path,*args,**kwargs)
        if path in targets:put(owner/'retain-builds.json',{})
    monkeypatch.setattr(Path,'unlink',insert)
    with pytest.raises(ValueError,match='pinned'):run(published)
    assert sum(p.exists() for p in targets)==1


def test_completed_native_replay_skips_missing_cache_and_preserves_newer_sources(published,monkeypatch):
    c,owner,observed,state,folder,targets=published;run(published)
    publisher=Publisher(c.root.parent,c.repo)
    put(publisher.root/'pending.json',{'identity':folder.name})
    (c.repo/'Tablet/A.lean').write_text('newer accepted source')
    monkeypatch.setattr('trellis.dispatch.publication_artifacts.install_via_checker',lambda *_:pytest.fail('cache reinstall'))
    with publisher.locked():
        result=publisher.recover(lambda *_:pytest.fail('new commit'),
            publication_receipt=lambda *_:state['dispatch']['publications']['1'])
    assert result==state['dispatch']['publications']['1']
    assert (c.repo/'Tablet/A.lean').read_text()=='newer accepted source'
    assert not (publisher.root/'pending.json').exists()


@pytest.mark.parametrize('level',['root','handoff','node','part'])
def test_unknown_schemas_preserved_before_marker(published,level):
    c,owner,observed,state,folder,targets=published
    record=json.loads((folder/'record.json').read_text())
    if level=='root':record['future']=True
    elif level=='handoff':record['artifacts']['future']=True
    elif level=='node':record['artifacts']['nodes']['A']['future']=True
    else:record['artifacts']['nodes']['A']['artifact_bundle']=[dict(level='exported',sha256='a'*64,size_bytes=1,future=True)]
    raw=candidate.canonical_json(record);new=folder.with_name(candidate.digest(raw));folder.rename(new);atomic(new/'record.json',raw)
    with pytest.raises(ValueError):run((c,owner,observed,state,new,{new/p.relative_to(folder):v for p,v in targets.items()}))
    assert not (new/retention.MARKER).exists()


def test_background_default_off_and_explicit_optin(tmp_path):
    from trellis.dispatch.storage import BackgroundCollector
    default=BackgroundCollector(tmp_path,git_maintenance_threshold_bytes=None)
    assert default._publication_retention is None and not default.retire_publication_caches
    default.close()
    with pytest.raises(ValueError):BackgroundCollector(tmp_path,retire_publication_caches=1)
    with pytest.raises(ValueError):BackgroundCollector(tmp_path,publication_cache_admission=True)


@pytest.mark.parametrize('change',['cancel','prepared','checkpoint','current','marker'])
def test_queued_changes_refuse_unlink(published,monkeypatch,change):
    c,owner,observed,state,folder,targets=published
    original=c.retirement_lock;calls=[]
    @contextmanager
    def lock():
        calls.append(1)
        if len(calls)==2:
            if change=='cancel':monkeypatch.setattr(c,'cancelled',lambda:True)
            elif change=='prepared':put(owner/'prepared.json',{})
            elif change=='checkpoint':monkeypatch.setattr(c,'checkpoint_current',lambda _:False)
            elif change=='current':state['dispatch']['publications'].clear()
            else:put(folder/retention.MARKER,{})
        with original() as acquired:yield acquired
    monkeypatch.setattr(c,'retirement_lock',lock)
    if change=='cancel':
        assert run(published)['removed']==0
    else:
        with pytest.raises((ValueError,InterruptedError)):run(published)
    assert all(p.exists() for p in targets)


def test_directory_sync_failure_keeps_incomplete_marker_and_retry_reestablishes_durability(published,monkeypatch):
    c,owner,observed,state,folder,targets=published;original=retention._sync_dir
    def fail(parent):
        if parent!=folder:raise OSError('sync failed')
        original(parent)
    with monkeypatch.context() as m:
        m.setattr(retention,'_sync_dir',fail)
        with pytest.raises(OSError,match='sync failed'):run(published)
    assert not any(p.exists() for p in targets)
    assert not json.loads((folder/retention.MARKER).read_text())['complete']
    assert run(published)['journals']==1


def test_duplicate_json_keys_are_rejected_before_retirement():
    with pytest.raises(ValueError,match='duplicate'):
        retention._json(b'{"version":1,"version":2}')


def test_background_actual_optin_dispatch_and_counters(published,monkeypatch):
    from trellis.dispatch.storage import BackgroundCollector
    c,owner,observed,state,folder,targets=published
    worker=BackgroundCollector(c.root,repo=c.repo,retire_publication_caches=True,
        git_maintenance_threshold_bytes=None)
    worker._directories={owner};worker._terminal_tasks=observed
    worker._publication_candidates=terminal.publication_cache_candidates(state)
    worker._accepted_sources=terminal.accepted_source_candidates(state)
    # Exercise actual owner dispatch after ordinary retention; unrelated scope
    # methods are no-ops to keep this test about the new integration boundary.
    empty={'removed':0,'removed_bytes':0,'removed_allocated_bytes':0}
    for name in ('retire','retire_execution','retire_history','retire_compiled_probes','retire_checker_caches'):
        monkeypatch.setattr(worker._terminal_retention,name,lambda *a,**kw:dict(empty))
    for module,method,extra in [('framework_retention','retire',{}),('source_retention','retire',{}),
        ('auxiliary_retention','retire',{}),('provider_retention','collect',{'archived':0}),
        ('git_retention','collect',{'stores':0,'retained_stores':0,'removed_links':0,'unlinked_allocated_bytes':0,
            'pack_stores':0,'removed_pack_files':0,'new_pack_allocated_bytes':0}),
        ('worker_source_retention','retire',{}),('accepted_source_retention','retire',{}),
        ('audit_copy_retention','retire',{}),('audit_copy_retention','retire_empty_directories',{'removed_directories':0}),
        ('host_response_retention','collect',{'archived':0}),('redundant_host_result_retention','collect',{}),
        ('assignment_retention','collect',{'archived':0})]:
        monkeypatch.setattr(f'trellis.dispatch.{module}.{method}',lambda *a,_extra=extra,**kw:dict(empty,**_extra))
    worker._collect_owner(owner)
    assert worker.summary['publication_cache_files']==2
    assert worker.summary['publication_cache_journals']==1
    assert worker.summary['publication_cache_marker_allocated_bytes']>0
    assert not any(p.exists() for p in targets)
    worker.close()


def test_completed_receipt_does_not_reread_large_immutable_record(published,monkeypatch):
    run(published);original=retention._read
    def read(path,limit):
        if path.name=='record.json':pytest.fail('completed cache read large journal again')
        return original(path,limit)
    monkeypatch.setattr(retention,'_read',read)
    assert run(published)['removed']==0


def test_missing_catalog_entries_rechecked_after_new_observation(published):
    c,owner,observed,state,folder,targets=published
    index=retention.Collector(c);receipt=state['dispatch']['publications']['1']
    committed=folder/'committed.json';raw=committed.read_bytes();committed.unlink()
    assert index._journals(receipt)==()
    atomic(committed,raw)
    assert index._journals(receipt)==()  # Finite negative cache within one drain.
    index.invalidate_catalog()
    assert index._journals(receipt)==(folder,)


def test_old_source_only_journals_are_preserved_without_errors(published):
    c,owner,observed,state,folder,targets=published
    value=json.loads((folder/'record.json').read_text());value['version']=1;del value['artifacts']
    raw=candidate.canonical_json(value);new=folder.with_name(candidate.digest(raw));folder.rename(new);atomic(new/'record.json',raw)
    result=run((c,owner,observed,state,new,{}))
    assert result['status']=='no_optional_cache' and result['removed']==0
    assert not (new/retention.MARKER).exists()


def test_git_tracking_arriving_before_unlink_preserves_publication(published, monkeypatch):
    c, owner, observed, state, folder, targets = published
    for key in list(os.environ):
        if key.startswith('GIT_'):
            monkeypatch.delenv(key)
    real = c.retirement_lock
    @contextmanager
    def lock():
        with real() as acquired:
            if (folder / 'artifacts-retired.json').exists():
                git(folder, 'init')
                git(folder, 'add', *(str(path) for path in targets))
            yield acquired
    monkeypatch.setattr(c, 'retirement_lock', lock)
    with pytest.raises(ValueError, match='changed before unlink'):
        run(published)
    assert all(path.read_bytes() == data for path, data in targets.items())
