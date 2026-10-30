from contextlib import contextmanager
import json
import os
from pathlib import Path
import shutil

import pytest

from test_dispatch_terminal_retention import setup, put, git
from test_dispatch_nonworker_retention import owner_setup
from trellis.dispatch import worker_source_retention as copies, auxiliary_retention as engine, terminal_retention as terminal
from trellis.dispatch.candidate import source_manifest, seal, canonical_json, digest


@pytest.fixture
def worker_setup(setup):
    collector,directory,observed,_,state=setup
    data={'Tablet/A.lean':b'Canonical theorem\n','reference/context.txt':b'Canonical context\n'}
    for name,value in data.items():
        p=collector.repo/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(value)
    git(collector.repo,'add','.');git(collector.repo,'commit','-qm','Recoverable worker inputs')
    prepared=json.loads((directory/'prepared.json').read_text());prepared['source_head']=git(collector.repo,'rev-parse','HEAD')
    for suffix in ('','acceptance/1','acceptance/1/finalization/1','turns/1','turns/1/transports/1'):
        put(directory/suffix/'prepared.json',prepared)
        for root in ('repo','repo/.trellis/supervisor/repo'):
            for name,value in data.items():
                p=directory/suffix/root/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(value)
    put(directory/'base-manifest.json',{name:digest(value) for name,value in data.items()})
    git(directory/'repo','init','-q');git(directory/'repo','add','Tablet','reference')
    return collector,directory,observed,data,state


def eligible(directory):
    return [path for _,repo,surface in copies.roots(directory) for path in (repo/surface).rglob('*') if path.is_file()]


def test_worker_descendants_retire_but_outer_and_turn_sources_and_candidates_remain(worker_setup):
    collector,directory,observed,data,_=worker_setup
    for name in data:put(directory/'candidate/files'/name,{'sealed':'different bytes'})
    paths=eligible(directory);assert len(paths)==14
    keep={p:p.read_bytes() for p in directory.rglob('*') if p.is_file() and p not in paths}
    first=collector.retire(directory,observed,apply=True)
    # Existing private-state receipt is independent of this new source scope.
    for p in list(keep):
        if not p.exists():del keep[p]
    result=copies.retire(collector,directory,observed,apply=True)
    assert result['removed']==14 and not any(p.exists() for p in paths)
    assert all(p.read_bytes()==v for p,v in keep.items())
    assert (directory/'repo/Tablet/A.lean').exists() and (directory/'turns/1/repo/Tablet/A.lean').exists()
    assert copies.retire(collector,directory,observed,apply=True)['removed']==0


@pytest.mark.parametrize('change',['changed','extra','mode','hardlink','symlink'])
def test_mixed_private_sources_preserved(worker_setup,change):
    collector,directory,observed,_,_=worker_setup
    path=directory/'acceptance/1/repo/Tablet/A.lean'
    if change=='changed':path.write_text('private candidate change')
    elif change=='extra':path=put(path.with_name('unique.lean'),{'proof':'uncommitted'})
    elif change=='mode':path.chmod(0o755)
    elif change=='hardlink':os.link(path,directory/'alias')
    else:path.rename(path.with_name('saved'));path.symlink_to('saved')
    if change in ('hardlink','symlink'):
        with pytest.raises(ValueError):copies.retire(collector,directory,observed,apply=True)
    else:
        before=path.read_bytes();result=copies.retire(collector,directory,observed,apply=True)
        assert result['removed']==(14 if change=='extra' else 13) and result['retained_files']==1
        assert path.read_bytes()==before
    assert path.exists()


@pytest.mark.parametrize('guard',['active','publication','pin','live','cleanup','identity','other_project'])
def test_worker_copy_lifecycle_guard(worker_setup,monkeypatch,guard):
    collector,directory,observed,_,_=worker_setup;paths=eligible(directory)
    if guard=='active':observed={}
    elif guard=='publication':monkeypatch.setattr(collector,'_owner_pending',lambda _:True)
    elif guard=='identity':observed[directory.name]='different'
    elif guard=='pin':put(directory/'acceptance/1/retain-builds.json',{})
    elif guard in ('live','cleanup'):
        put(directory/'acceptance/1/execution/process.json',{'pid':123});put(directory/'acceptance/1/execution/cleanup.json',{'pid':123 if guard=='live' else 456})
        monkeypatch.setattr('trellis.dispatch.process.alive',lambda _:guard=='live')
    else:
        p=directory/'acceptance/1/prepared.json';v=json.loads(p.read_text());v['source_repo']=str(directory);put(p,v)
    if guard in ('active','publication','identity'):assert copies.retire(collector,directory,observed,apply=True)['removed']==0
    else:
        with pytest.raises(ValueError):copies.retire(collector,directory,observed,apply=True)
    assert all(p.exists() for p in paths)


def test_nonworkers_remain_in_separate_policy(owner_setup):
    collector,directory,observed,_,_=owner_setup
    assert copies.retire(collector,directory,observed,apply=True)['status']=='nonworker_preserved'


@pytest.mark.parametrize('target',['outer','candidate','turn','arbitrary'])
def test_receipt_cannot_redirect_to_required_worker_sources(worker_setup,monkeypatch,target):
    collector,directory,observed,_,_=worker_setup
    @contextmanager
    def busy():yield False
    monkeypatch.setattr(collector,'retirement_lock',busy)
    copies.retire(collector,directory,observed,apply=True)
    receipt=directory/copies.RECEIPT;record=json.loads(receipt.read_text())
    root={'outer':'repo/Tablet','candidate':'candidate/files/Tablet','turn':'turns/1/repo/Tablet','arbitrary':'random/repo/Tablet'}[target]
    preserved=put(directory/root/'A.lean',{'preserve':target})
    record['files'][0]['root']=root;record['files'][0]['path']=root+'/A.lean';put(receipt,record)
    with pytest.raises(ValueError,match='allowlist'):copies.retire(collector,directory,observed,apply=True)
    assert preserved.exists()


def test_partial_deletion_restarts_and_ancestor_change_stops_next_root(worker_setup,monkeypatch):
    collector,directory,observed,_,_=worker_setup;paths=eligible(directory);calls=[]
    @contextmanager
    def lock():
        calls.append(1)
        if len(calls)==2:monkeypatch.setattr(collector,'checkpoint_current',lambda _:False)
        yield True
    monkeypatch.setattr(collector,'retirement_lock',lock)
    result=copies.retire(collector,directory,observed,apply=True)
    assert result['removed']==1 and result['status']=='checkpoint_changed'
    monkeypatch.setattr(collector,'checkpoint_current',lambda _:True)
    assert copies.retire(collector,directory,observed,apply=True)['removed']==13
    assert not any(p.exists() for p in paths)


@pytest.mark.parametrize('sealed',[False,True])
def test_future_previous_worker_and_findings_use_preserved_sources(worker_setup,monkeypatch,sealed):
    from test_dispatch_transcripts import completed
    from trellis.dispatch import handoffs
    from trellis.dispatch.workspace import prepare
    collector,directory,observed,_,_=worker_setup
    saved=json.loads((directory/'prepared.json').read_text());completed(collector.root.parent,directory.name,7);put(directory/'prepared.json',saved)
    monkeypatch.setattr('trellis.dispatch.process.alive',lambda _:False);monkeypatch.setattr(handoffs,'alive',lambda _:False)
    source=directory/'repo/Tablet/A.lean';source.write_text('Useful unfinished outer proof')
    task={'id':1,'status':{'status':'resolved','outcome':'timed_out'},'assignment':{'scope':{'nodes':['A']}}}
    if sealed:
        seal(directory/'repo',directory/'candidate',task_id=1,
            base_manifest=json.loads((directory/'base-manifest.json').read_text()),inputs={},response={'outcome':'Invalid'})
        source.write_text('Later outer source')
    before=handoffs._findings(directory,task);assert before is not None
    (directory/'unaccepted-findings.json').unlink()
    assert copies.retire(collector,directory,observed,apply=True)['removed']==14
    assert handoffs._findings(directory,task)==before
    config=put(collector.repo/'trellis.config.json',{'repo_path':str(collector.repo)})
    put(collector.root.parent/'runtime_metadata.json',{'repo_path':str(collector.repo),'config_path':str(config)})
    dest=collector.root/'99'
    prepare(collector.root.parent,collector.repo,config,dest,{'in_flight_request':{'id':99,'kind':'Worker'}},
        expected_manifest=source_manifest(collector.repo),previous=directory,seed_packages=False)
    assert (dest/'repo/last_invalid/Tablet/A.lean').read_bytes()==source.read_bytes()
    assert (dest/'repo/last_invalid/reference/context.txt').read_bytes()==(directory/'repo/reference/context.txt').read_bytes()
