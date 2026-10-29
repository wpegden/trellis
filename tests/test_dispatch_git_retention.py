from contextlib import contextmanager
import json
import os
from pathlib import Path
import shutil
import subprocess

import pytest
from test_dispatch_terminal_retention import setup, put, git
from trellis.dispatch import git_retention as retention


def run(repo, *args, data=None):
    return subprocess.check_output(['git', '-C', str(repo), *args], input=data, stderr=subprocess.PIPE).decode().strip()


def clone(setup, *, relative='repo'):
    collector, directory, observed, _, _ = setup
    temporary = directory.parent / ('clone-' + relative.replace('/', '-'))
    run(collector.repo, 'clone', '-q', '--local', '--no-checkout', str(collector.repo), str(temporary))
    repo = directory / relative; repo.mkdir(parents=True, exist_ok=True)
    shutil.move(str(temporary / '.git'), str(repo / '.git')); temporary.rmdir()
    run(repo, 'read-tree', 'HEAD')
    prepared = json.loads((directory / 'prepared.json').read_text())
    prepared['source_head'] = git(collector.repo, 'rev-parse', 'HEAD')
    put(directory / 'prepared.json', prepared)
    return repo


def loose(repo):
    return [p for d in (repo / '.git/objects').iterdir() if len(d.name) == 2
            for p in d.iterdir()]


def test_native_prune_preserves_git_semantics_and_counts_only_last_links(setup):
    collector, directory, observed, _, _ = setup
    repo = clone(setup); originals = {p.name:p.read_bytes() for p in (repo / '.git').glob('*') if p.is_file()}
    expected = run(repo, 'show', 'HEAD:.trellis-history/supervisor_state.json')
    run(collector.repo, 'repack', '-a', '-d')
    assert loose(repo) and all(p.stat().st_nlink == 1 for p in loose(repo))
    dry = retention.collect(collector, directory, observed, apply=False)
    assert dry['stores'] == 1 and not (repo / '.git/objects/info/alternates').exists()
    result = retention.collect(collector, directory, observed)
    assert result['stores'] == 1 and result['removed_links'] >= 3
    assert result['removed_allocated_bytes'] > 0 and not loose(repo)
    assert run(repo, 'show', 'HEAD:.trellis-history/supervisor_state.json') == expected
    assert all((repo / '.git' / name).read_bytes() == raw for name, raw in originals.items())
    assert retention.collect(collector, directory, observed)['removed_links'] == 0


def test_shared_links_are_not_reported_as_reclaimed_bytes(setup, tmp_path):
    collector, directory, observed, _, _ = setup
    repo = clone(setup)
    extra = tmp_path / 'extra'; run(collector.repo, 'clone', '-q', '--local', '--no-checkout', str(repo), str(extra))
    run(collector.repo, 'repack', '-a', '-d')
    result = retention.collect(collector, directory, observed)
    assert result['removed_links'] and result['unlinked_allocated_bytes'] > 0
    assert result['removed_allocated_bytes'] == 0 and loose(extra)


@pytest.mark.parametrize('kind', ['private', 'canonical_dangling', 'local_packed'])
def test_unprotected_object_preserves_complete_store(setup, kind):
    collector, directory, observed, _, _ = setup
    repo = clone(setup)
    oid = run(repo, 'hash-object', '-w', '--stdin', data=b'private dangling evidence')
    if kind == 'canonical_dangling':
        assert run(collector.repo, 'hash-object', '-w', '--stdin', data=b'private dangling evidence') == oid
    if kind == 'local_packed':
        objects = repo / '.git/objects'; pack = objects / 'pack';pack.mkdir(exist_ok=True)
        subprocess.run(['git','-C',str(repo),'pack-objects',str(pack/'pack')], input=(oid+'\n').encode(),check=True,stdout=subprocess.PIPE)
    run(collector.repo, 'repack', '-a', '-d')
    before = retention._inventory(repo / '.git')
    result = retention.collect(collector, directory, observed)
    assert result['retained_stores'] == 1 and result['removed_links'] == 0
    assert retention._inventory(repo / '.git') == before


def test_foreign_alternate_is_not_overwritten(setup, tmp_path):
    collector, directory, observed, _, _ = setup; repo = clone(setup)
    alternate = repo / '.git/objects/info/alternates';alternate.parent.mkdir(exist_ok=True)
    alternate.write_text(str(tmp_path) + '\n');before=alternate.read_bytes()
    assert retention.collect(collector, directory, observed)['retained_stores'] == 1
    assert alternate.read_bytes() == before


def test_crash_after_alternate_is_idempotent_and_pin_survives_gc(setup, monkeypatch):
    collector, directory, observed, _, _ = setup; repo=clone(setup)
    head=git(collector.repo,'rev-parse','HEAD');expected=run(repo,'show','HEAD:.trellis-history/supervisor_state.json')
    run(collector.repo,'repack','-a','-d')
    original=retention._prune
    def crash(_):raise RuntimeError('interrupted after durable alternate')
    monkeypatch.setattr(retention,'_prune',crash)
    with pytest.raises(RuntimeError,match='interrupted'):retention.collect(collector,directory,observed)
    assert loose(repo) and (repo/'.git/objects/info/alternates').is_file()
    assert git(collector.repo,'rev-parse',retention.PIN_PREFIX+head)==head
    monkeypatch.setattr(retention,'_prune',original)
    assert retention.collect(collector,directory,observed)['removed_links']>0
    tree=git(collector.repo,'rev-parse','HEAD^{tree}')
    new=run(collector.repo,'commit-tree',tree,'-m','new disconnected head')
    run(collector.repo,'update-ref','HEAD',new)
    run(collector.repo,'reflog','expire','--expire=now','--all')
    run(collector.repo,'gc','--prune=now')
    assert run(repo,'show','HEAD:.trellis-history/supervisor_state.json')==expected


@pytest.mark.parametrize('change', ['new_object','index','pin','prepared','cancel','head','pending'])
def test_final_fence_rechecks_after_offlock_proof(setup, monkeypatch, change):
    collector,directory,observed,_,_=setup;repo=clone(setup);run(collector.repo,'repack','-a','-d')
    original=collector.retirement_lock
    @contextmanager
    def lock():
        if change=='new_object':run(repo,'hash-object','-w','--stdin',data=b'new object while queued')
        elif change=='index':(repo/'.git/index').write_bytes(b'changed')
        elif change=='pin':put(directory/'retain-builds.json',{})
        elif change=='prepared':put(directory/'prepared.json',{'source_repo':str(directory)})
        elif change=='cancel':monkeypatch.setattr(collector,'cancelled',lambda:True)
        elif change=='head':monkeypatch.setattr(collector,'checkpoint_current',lambda _:False)
        elif change=='pending':monkeypatch.setattr(collector,'_owner_pending',lambda _:True)
        with original() as acquired:yield acquired
    monkeypatch.setattr(collector,'retirement_lock',lock)
    try:result=retention.collect(collector,directory,observed)
    except (ValueError, OSError):pass
    else:assert result['removed_links']==0
    assert loose(repo) and not (repo/'.git/objects/info/alternates').exists()


def test_conflicting_recovery_ref_fails_closed(setup):
    collector,directory,observed,_,_=setup;repo=clone(setup);head=git(collector.repo,'rev-parse','HEAD')
    other=run(collector.repo,'hash-object','-w','--stdin',data=b'conflicting ref')
    run(collector.repo,'update-ref',retention.PIN_PREFIX+head,other)
    with pytest.raises(ValueError,match='pin conflicts'):retention.collect(collector,directory,observed)
    assert loose(repo) and not (repo/'.git/objects/info/alternates').exists()


def test_known_nested_store_is_included_but_arbitrary_repositories_are_not(setup):
    collector,directory,observed,_,_=setup
    outer=clone(setup);nested=clone(setup,relative='repo/.trellis/supervisor/repo');other=clone(setup,relative='scratch/repo')
    run(collector.repo,'repack','-a','-d')
    result=retention.collect(collector,directory,observed)
    assert result['stores']==2 and not loose(outer) and not loose(nested) and loose(other)


@pytest.mark.parametrize('failure',['active','uncommitted','live','unreaped','pin','alias','git_lock'])
def test_initial_guards_preserve_objects(setup,monkeypatch,failure):
    collector,directory,observed,_,_=setup;repo=clone(setup)
    if failure=='active':observed={}
    elif failure=='uncommitted':observed={**observed,'1':'different'}
    elif failure=='pin':put(directory/'retain-builds.json',{})
    elif failure in ('live','unreaped'):
        put(directory/'execution/process.json',{'pid':123})
        if failure=='live':put(directory/'execution/cleanup.json',{'pid':123})
        monkeypatch.setattr('trellis.dispatch.process.alive',lambda _:failure=='live')
    elif failure=='alias':
        p=repo/'.git/objects/info';p.rmdir();p.symlink_to(collector.repo/'.git/objects/info',target_is_directory=True)
    elif failure=='git_lock':(repo/'.git/index.lock').write_bytes(b'writer')
    try:result=retention.collect(collector,directory,observed)
    except (ValueError, OSError):pass
    else:assert result['removed_links']==0
    assert loose(repo)


def test_later_canonical_packing_revisits_migrated_clone(setup, monkeypatch):
    collector,directory,observed,_,_=setup;repo=clone(setup)
    first=retention.collect(collector,directory,observed)
    assert first['stores']==1 and first['removed_links']==0 and loose(repo)
    def forbidden(*_):raise AssertionError('unchanged generation should skip expensive proof')
    with monkeypatch.context() as m:
        m.setattr(retention,'_local_objects',forbidden)
        assert retention.collect(collector,directory,observed)['stores']==0
    run(collector.repo,'repack','-a','-d')
    assert retention.collect(collector,directory,observed)['removed_links']>0
    assert not loose(repo)


def test_sibling_unlinks_do_not_invalidate_immutable_objects(setup,monkeypatch,tmp_path):
    collector,directory,observed,_,_=setup;repo=clone(setup)
    extra=tmp_path/'extra';run(collector.repo,'clone','-q','--local','--no-checkout',str(repo),str(extra))
    run(collector.repo,'repack','-a','-d');original=collector.retirement_lock
    @contextmanager
    def lock():
        for path in loose(extra):path.unlink()
        with original() as acquired:yield acquired
    monkeypatch.setattr(collector,'retirement_lock',lock)
    assert retention.collect(collector,directory,observed)['removed_links']>0


def test_symbolic_pin_is_not_accepted(setup):
    collector,directory,observed,_,_=setup;repo=clone(setup);head=git(collector.repo,'rev-parse','HEAD')
    run(collector.repo,'symbolic-ref',retention.PIN_PREFIX+head,'refs/heads/master')
    with pytest.raises(ValueError,match='symbolic'):retention.collect(collector,directory,observed)
    assert not (repo/'.git/objects/info/alternates').exists()


def test_deleted_fanout_is_synced_before_finished_receipt(setup,monkeypatch):
    collector,directory,observed,_,_=setup;repo=clone(setup);run(collector.repo,'repack','-a','-d')
    restored=loose(repo)[0]; original_loose=restored.read_bytes()
    original=retention._sync_dir;failed=[];pruned=[];prune=retention._prune
    def prune_and_mark(path):
        prune(path);pruned.append(True)
    monkeypatch.setattr(retention,'_prune',prune_and_mark)
    def fail(path):
        if pruned and (path==repo/'.git/objects' or path.parent==repo/'.git/objects'):
            failed.append(path);raise OSError('fsync failure')
        return original(path)
    monkeypatch.setattr(retention,'_sync_dir',fail)
    with pytest.raises(OSError,match='fsync failure'):retention.collect(collector,directory,observed)
    assert failed
    record=json.loads((repo/'.git'/retention.RECEIPT).read_text())
    assert 'finished_pack_generation' not in record
    # Simulate a crash restoring one not-yet-durable removed directory entry.
    restored.parent.mkdir(exist_ok=True);restored.write_bytes(original_loose)
    monkeypatch.setattr(retention,'_sync_dir',original)
    assert retention.collect(collector,directory,observed)['removed_links']==1
    assert not restored.exists()


def test_incomplete_pack_publication_defers_without_remembering_generation(setup):
    collector,directory,observed,_,_=setup;repo=clone(setup)
    run(collector.repo,'repack','-a','-d')
    index=next((collector.repo/'.git/objects/pack').glob('*.idx'));saved=index.read_bytes();index.unlink()
    result=retention.collect(collector,directory,observed)
    assert result['status']=='canonical_pack_pending' and loose(repo)
    assert not (repo/'.git/objects/info/alternates').exists()
    index.write_bytes(saved)
    assert retention.collect(collector,directory,observed)['removed_links']>0


def test_git_environment_overrides_do_not_redirect_mutation(setup,monkeypatch,tmp_path):
    collector,directory,observed,_,_=setup;repo=clone(setup);run(collector.repo,'repack','-a','-d')
    unrelated=tmp_path/'unrelated';unrelated.mkdir();run(unrelated,'init','-q')
    monkeypatch.setenv('GIT_DIR',str(unrelated/'.git'))
    monkeypatch.setenv('GIT_OBJECT_DIRECTORY',str(unrelated/'.git/objects'))
    monkeypatch.setattr('trellis.dispatch.retained_files._untracked',lambda _:True)
    # Collector uses its older helper in witness/checkpoint_current; snapshot it
    # before overrides while this test exercises all new Git mutation paths.
    monkeypatch.delenv('GIT_DIR');monkeypatch.delenv('GIT_OBJECT_DIRECTORY')
    witness=collector.witness();monkeypatch.setattr(collector,'witness',lambda:witness)
    monkeypatch.setattr(collector,'checkpoint_current',lambda _:True)
    monkeypatch.setenv('GIT_DIR',str(unrelated/'.git'));monkeypatch.setenv('GIT_OBJECT_DIRECTORY',str(unrelated/'.git/objects'))
    assert retention.collect(collector,directory,observed)['removed_links']>0
    assert not (unrelated/'.git/refs/trellis').exists()


from test_dispatch_nonworker_retention import owner_setup


def test_typed_nonworker_uses_same_native_git_retirement(owner_setup):
    collector,directory,observed,_,_=owner_setup;repo=clone(owner_setup)
    run(collector.repo,'repack','-a','-d')
    result=retention.collect(collector,directory,observed)
    assert result['removed_links']>0
    assert json.loads((repo/'.git'/retention.RECEIPT).read_text())['owner']['kind'] in ('review','verifier')


@pytest.mark.parametrize('name',['shallow','commondir','info/grafts'])
@pytest.mark.parametrize('canonical',[True,False])
def test_unsupported_graph_or_object_layout_is_preserved(setup,name,canonical):
    collector,directory,observed,_,_=setup;repo=clone(setup)
    path=(collector.repo if canonical else repo)/'.git'/name;path.parent.mkdir(exist_ok=True)
    path.write_text('unsupported\n')
    try:result=retention.collect(collector,directory,observed)
    except (ValueError, subprocess.CalledProcessError):pass
    else:assert result['removed_links']==0
    assert loose(repo) and not (repo/'.git/objects/info/alternates').exists()


def test_new_alternate_directory_is_durable_before_prune(setup,monkeypatch):
    collector,directory,observed,_,_=setup;repo=clone(setup)
    (repo/'.git/objects/info').rmdir();run(collector.repo,'repack','-a','-d')
    synced=[];sync=retention._sync_dir;prune=retention._prune
    def record(path):synced.append(path);return sync(path)
    def checked(path):
        assert path==repo and repo/'.git/objects' in synced
        assert (repo/'.git/objects/info/alternates').exists()
        return prune(path)
    monkeypatch.setattr(retention,'_sync_dir',record);monkeypatch.setattr(retention,'_prune',checked)
    assert retention.collect(collector,directory,observed)['removed_links']>0


@pytest.mark.parametrize('name',['info','pack'])
def test_canonical_object_subdirectory_alias_is_rejected(setup,tmp_path,name):
    collector,directory,observed,_,_=setup;repo=clone(setup)
    target=tmp_path/'other';target.mkdir()
    path=collector.repo/'.git/objects'/name
    if path.exists():path.rmdir()
    path.symlink_to(target,target_is_directory=True)
    with pytest.raises(ValueError,match='aliases'):retention.collect(collector,directory,observed)
    assert loose(repo) and not (repo/'.git/objects/info/alternates').exists()


def test_background_drain_reports_native_git_link_and_last_link_counters(setup):
    from trellis.dispatch.storage import BackgroundCollector
    collector,directory,_,_,state=setup;repo=clone(setup);run(collector.repo,'repack','-a','-d')
    background=BackgroundCollector(collector.root,repo=collector.repo)
    try:
        assert background.schedule(state)
        assert background.wait_idle(30)
        summary=background.summary
        assert summary['errors']==0
        assert summary['git_object_stores']==1 and summary['git_object_links']>=3
        assert summary['git_object_unlinked_allocated_bytes']>0
        assert summary['removed_allocated_bytes']>=summary['git_object_unlinked_allocated_bytes']
        assert not loose(repo)
    finally:
        background.close()
        assert background.wait_idle(30)
