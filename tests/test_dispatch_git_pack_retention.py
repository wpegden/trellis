import json
from pathlib import Path
import subprocess
from contextlib import contextmanager

import pytest

from test_dispatch_terminal_retention import setup, put, git
from test_dispatch_git_retention import clone, run, loose
from trellis.dispatch import git_retention as retention
from trellis.dispatch import retained_files as retained


def packed_clone(setup):
    collector,_,_,_,_=setup
    run(collector.repo,'repack','-a','-d')
    return clone(setup)


def replace_canonical(collector):
    (collector.repo/'later').write_text('later canonical checkpoint\n')
    run(collector.repo,'add','later');run(collector.repo,'commit','-qm','later checkpoint')
    run(collector.repo,'repack','-a','-d')


def pack_snapshot(repo):
    return {p.name:(p.stat().st_dev,p.stat().st_ino,p.stat().st_size,p.stat().st_mtime_ns)
            for p in (repo/'.git/objects/pack').iterdir()}


def metadata(repo):
    return {str(p.relative_to(repo/'.git')):p.read_bytes()
            for p in (repo/'.git').rglob('*') if p.is_file()
            and not str(p.relative_to(repo/'.git')).startswith('objects/pack/')
            and p.name!=retention.RECEIPT and p.name!='alternates'}


def test_packed_only_store_releases_last_links_preserving_all_git_views(setup):
    collector,directory,observed,_,_=setup;repo=packed_clone(setup)
    expected=run(repo,'show','HEAD:.trellis-history/supervisor_state.json')
    replace_canonical(collector)
    assert not loose(repo)
    originals=metadata(repo);canonical=pack_snapshot(collector.repo)
    oldfiles=retention._pack_files(repo/'.git/objects')
    assert all(info.st_nlink==1 for info in oldfiles.values())
    result=retention.collect(collector,directory,observed)
    assert result['pack_stores']==1 and result['removed_pack_files']>=2
    assert result['removed_allocated_bytes']==sum(info.st_blocks*512 for info in oldfiles.values())
    assert result['new_pack_allocated_bytes']==0
    assert not retention._pack_files(repo/'.git/objects')
    assert pack_snapshot(collector.repo)==canonical
    assert run(repo,'show','HEAD:.trellis-history/supervisor_state.json')==expected
    assert all((repo/'.git'/name).read_bytes()==raw for name,raw in originals.items())
    assert retention.collect(collector,directory,observed)['pack_stores']==0


def test_current_canonical_pack_links_are_left_until_obsolete(setup):
    collector,directory,observed,_,_=setup;repo=packed_clone(setup)
    before=pack_snapshot(repo)
    assert retention.collect(collector,directory,observed)['pack_stores']==0
    assert pack_snapshot(repo)==before
    replace_canonical(collector)
    assert retention.collect(collector,directory,observed)['pack_stores']==1


def test_old_loose_only_finished_receipt_cannot_hide_obsolete_packs(setup):
    collector,directory,observed,_,_=setup;repo=packed_clone(setup);replace_canonical(collector)
    head=json.loads((directory/'prepared.json').read_text())['source_head'];canonical=collector.repo/'.git/objects'
    retention._pin(collector.repo,head)
    (repo/'.git/objects/info/alternates').write_text(str(canonical)+'\n')
    put(repo/'.git'/retention.RECEIPT,dict(format=retention.FORMAT,owner=retained.owner_identity(directory),
        identity=observed[directory.name],source_head=head,alternate=str(canonical),
        finished_pack_generation=retention._pack_generation(canonical)))
    assert retention.collect(collector,directory,observed)['pack_stores']==1


def test_packed_outside_closure_evidence_preserves_entire_store(setup):
    collector,directory,observed,_,_=setup;repo=packed_clone(setup);replace_canonical(collector)
    oid=run(repo,'hash-object','-w','--stdin',data=b'private-only evidence')
    subprocess.run(['git','-C',str(repo),'pack-objects',str(repo/'.git/objects/pack/pack')],
        input=(oid+'\n').encode(),stdout=subprocess.PIPE,check=True)
    run(repo,'prune-packed')
    before=pack_snapshot(repo)
    result=retention.collect(collector,directory,observed)
    assert result['retained_stores']==1 and result['pack_stores']==0
    assert before==pack_snapshot(repo) and run(repo,'cat-file','blob',oid)=='private-only evidence'


def test_private_unreachable_packed_object_recoverable_from_pinned_source(setup):
    collector,directory,observed,_,_=setup
    # The second checkpoint's blob is no longer reachable from the private
    # HEAD/index/reflog, but remains in its pack and in protected source history.
    first=run(collector.repo,'rev-parse','HEAD')
    (collector.repo/'extra').write_text('protected later blob')
    run(collector.repo,'add','extra');run(collector.repo,'commit','-qm','source update')
    oid=run(collector.repo,'rev-parse','HEAD:extra')
    repo=packed_clone(setup)
    run(repo,'update-ref','refs/heads/master',first)
    run(repo,'update-ref','refs/remotes/origin/master',first)
    run(repo,'read-tree',first);run(repo,'reflog','expire','--expire=now','--all')
    assert oid not in run(repo,'rev-list','--objects','--all','--reflog')
    replace_canonical(collector)
    assert retention.collect(collector,directory,observed)['pack_stores']==1
    assert run(repo,'cat-file','blob',oid)=='protected later blob'


def test_explicit_pack_keep_is_preserved(setup):
    collector,directory,observed,_,_=setup;repo=packed_clone(setup);replace_canonical(collector)
    next((repo/'.git/objects/pack').glob('*.pack')).with_suffix('.keep').write_text('operator retain')
    result=retention.collect(collector,directory,observed)
    assert result['pack_stores']==0 and result['retained_stores']==1
    assert list((repo/'.git/objects/pack').glob('*.pack'))


def test_interruption_after_native_repack_retries_without_losing_objects(setup,monkeypatch):
    collector,directory,observed,_,_=setup;repo=packed_clone(setup);replace_canonical(collector)
    original=retention._verify_objects
    def interrupted(*a):raise OSError('injected after native repack')
    monkeypatch.setattr(retention,'_verify_objects',interrupted)
    with pytest.raises(OSError):retention.collect(collector,directory,observed)
    assert run(repo,'show','HEAD:.trellis-history/supervisor_state.json')
    record=json.loads((repo/'.git'/retention.RECEIPT).read_text())
    assert 'finished_pack_generation' not in record
    monkeypatch.setattr(retention,'_verify_objects',original)
    assert retention.collect(collector,directory,observed)['pack_stores']==0


def test_multiple_prepared_siblings_progress_without_shared_metadata_churn(setup):
    collector,directory,observed,_,_=setup;repo=packed_clone(setup)
    peers=[]
    for ordinal in range(1,4):
        peer=clone(setup,relative=f'acceptance/{ordinal}/repo');peers.append(peer)
        attempt=peer.parent
        for name in ('prepared.json','process-cleanup.json'):
            source=directory/name
            if source.exists(): (attempt/name).write_bytes(source.read_bytes())
    replace_canonical(collector)
    snapshots=[metadata(peer) for peer in [repo,*peers]]
    infos=[(peer/'.git/objects/info/packs').stat() for peer in [repo,*peers]]
    result=retention.collect(collector,directory,observed)
    assert result['pack_stores']==4
    for peer,before,info in zip([repo,*peers],snapshots,infos):
        assert not retention._pack_files(peer/'.git/objects')
        assert all((peer/'.git'/name).read_bytes()==raw for name,raw in before.items())
        now=(peer/'.git/objects/info/packs').stat()
        assert (now.st_ino,now.st_ctime_ns)==(info.st_ino,info.st_ctime_ns)


def test_new_pack_allocation_is_separate_from_monotone_gross_reclaim(setup,monkeypatch):
    collector,directory,observed,_,_=setup;repo=packed_clone(setup);replace_canonical(collector)
    before=retention._pack_files(repo/'.git/objects');gross=sum(x.st_blocks*512 for x in before.values())
    original=retention._repack_local
    def adds_representation(path):
        original(path)
        oid=run(path,'rev-parse','HEAD')
        subprocess.run(['git','-C',str(path),'pack-objects',str(path/'.git/objects/pack/pack')],
            input=(oid+'\n').encode(),stdout=subprocess.PIPE,check=True)
    monkeypatch.setattr(retention,'_repack_local',adds_representation)
    result=retention.collect(collector,directory,observed)
    assert result['removed_allocated_bytes']==gross
    assert result['new_pack_allocated_bytes']>0
    assert result['removed_links']==0 and result['removed_pack_files']>=2
    assert run(repo,'show','HEAD:.trellis-history/supervisor_state.json')


def test_background_collector_reports_pack_scope_and_preserves_loose_counter(setup):
    from trellis.dispatch.storage import BackgroundCollector
    collector,directory,_,_,state=setup;repo=packed_clone(setup);replace_canonical(collector)
    background=BackgroundCollector(collector.root,repo=collector.repo,git_maintenance_threshold_bytes=None)
    try:
        assert background.schedule(state) and background.wait_idle(30)
        result=background.summary
        assert result['errors']==0 and result['git_pack_stores']==1
        assert result['git_pack_files']>=2 and result['git_object_links']==0
        assert result['git_new_pack_allocated_bytes']==0
        assert not retention._pack_files(repo/'.git/objects')
        assert not (repo/'.git/objects/info/commit-graph').exists()
    finally:
        background.close();assert background.wait_idle(30)
