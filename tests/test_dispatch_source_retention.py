from contextlib import contextmanager
import hashlib
import json
from pathlib import Path
import shutil
import os

import pytest

from test_dispatch_terminal_retention import setup, put, git
from test_dispatch_nonworker_retention import owner_setup
from trellis.dispatch import source_retention as sources, retained_files as retained
from trellis.dispatch.candidate import canonical_json


@pytest.fixture
def source_setup(owner_setup):
    collector, directory, observed, _, state = owner_setup
    files = {'Tablet/nested/A.lean': b'theorem A : True := by trivial\n',
             'reference/paper.txt': b'Original mathematical source\n'}
    for name, data in files.items():
        path = collector.repo / name; path.parent.mkdir(parents=True, exist_ok=True); path.write_bytes(data)
    git(collector.repo, 'add', '.')
    git(collector.repo, 'commit', '-qm', 'Recoverable sources')
    prepared = json.loads((directory / 'prepared.json').read_bytes())
    prepared['source_head'] = git(collector.repo, 'rev-parse', 'HEAD')
    put(directory / 'prepared.json', prepared)
    put(directory / 'base-manifest.json', {name:hashlib.sha256(data).hexdigest() for name,data in files.items()})
    for prefix in ('repo', 'repo/.trellis/supervisor/repo'):
        for surface in ('Tablet', 'reference'):
            target = directory / prefix / surface
            if target.exists(): shutil.rmtree(target)
            shutil.copytree(collector.repo / surface, target)
    private = directory / 'repo'
    git(private, 'init', '-q'); git(private, 'config', 'user.email', 'test@example.invalid'); git(private, 'config', 'user.name', 'Test')
    git(private, 'add', 'Tablet', 'reference'); git(private, 'commit', '-qm', 'Private original checkout')
    put(private / 'trellis.config.json', {'retain':'configuration'})
    put(private / '.trellis/scratch/note.json', {'advice':'future worker'})
    put(private / '.trellis/stuck-math-audit/cycle-1/probe.json', {'retain':'probe'})
    put(private / '.trellis/chats/live/chat/output.json', {'retain':'transcript'})
    return collector, directory, observed, files


def test_exact_tracked_sources_retire_preserving_git_and_advisory_evidence(source_setup):
    collector, directory, observed, files = source_setup
    targets = [p for _,p in sources.roots(directory)]
    before = {p:p.read_bytes() for p in directory.rglob('*') if p.is_file()
              and not any(root in p.parents for root in targets)}
    assert not retained._untracked(directory/'repo/Tablet')
    assert sources.retire(collector,directory,observed)['trees'] == 4
    result = sources.retire(collector,directory,observed,apply=True)
    assert result['removed'] == 4 and result['removed_bytes'] == sum(map(len,files.values()))*2
    assert all(not p.exists() and not sources.tombstone(p).exists() for p in targets)
    assert all(p.read_bytes()==data for p,data in before.items())
    assert sources.retire(collector,directory,observed,apply=True)['removed'] == 0
    prepared = json.loads((directory/'prepared.json').read_bytes())
    for name,data in files.items():
        import subprocess
        assert subprocess.check_output(['git','-C',str(collector.repo),'show',prepared['source_head']+':'+name]) == data


@pytest.mark.parametrize('fault',['extra','changed','missing','manifest','symlink','hardlink','executable','submodule','other_project','head'])
def test_unique_or_unrecoverable_sources_are_preserved(source_setup,fault):
    collector,directory,observed,_=source_setup
    tree=directory/'repo/Tablet'; leaf=tree/'nested/A.lean'
    if fault=='extra': (tree/'unique.lean').write_text('unique untracked work')
    elif fault=='changed': leaf.write_text('private unique edit')
    elif fault=='missing': leaf.unlink()
    elif fault=='manifest': put(directory/'base-manifest.json',{})
    elif fault=='symlink': leaf.unlink();leaf.symlink_to(collector.repo/'Tablet/nested/A.lean')
    elif fault=='hardlink': os.link(leaf,directory/'alias')
    elif fault=='executable': leaf.chmod(0o755)
    elif fault=='submodule': (tree/'.git').mkdir()
    else:
        prepared=json.loads((directory/'prepared.json').read_bytes())
        prepared['source_repo' if fault=='other_project' else 'source_head']=str(directory) if fault=='other_project' else 'f'*40
        put(directory/'prepared.json',prepared)
    if fault in ('extra','changed','missing','manifest','executable'):
        result=sources.retire(collector,directory,observed,apply=True)
        assert result['status']=='not_recoverable' and result['retained_sources']==1
    else:
        with pytest.raises((ValueError, __import__('subprocess').CalledProcessError)):
            sources.retire(collector,directory,observed,apply=True)
    assert tree.exists() and not sources.tombstone(tree).exists()
    assert not (directory/sources.RECEIPT).exists()


@pytest.mark.parametrize('fault',['nested_file','nested_extra','pin','prepared','checkpoint','cancel'])
def test_queued_changes_are_detected_by_final_subtree_guard(source_setup,monkeypatch,fault):
    import threading
    collector,directory,observed,_=source_setup
    tree=directory/'repo/Tablet'
    original=collector.retirement_lock
    @contextmanager
    def locked():
        if fault=='nested_file': (tree/'nested/A.lean').write_text('changed while queued')
        elif fault=='nested_extra': (tree/'nested/extra.lean').write_text('new while queued')
        elif fault=='pin': put(directory/'retain-builds.json',{})
        elif fault=='prepared':
            p=directory/'prepared.json';p.write_bytes(p.read_bytes()+b' ')
        elif fault=='checkpoint': monkeypatch.setattr(collector,'checkpoint_current',lambda _:False)
        elif fault=='cancel': collector.stop_event=threading.Event();collector.stop_event.set()
        with original() as acquired: yield acquired
    monkeypatch.setattr(collector,'retirement_lock',locked)
    if fault in ('checkpoint','cancel'):
        result=sources.retire(collector,directory,observed,apply=True)
        assert result['status'] in ('checkpoint_changed','cancelled')
    else:
        with pytest.raises(ValueError): sources.retire(collector,directory,observed,apply=True)
    assert tree.exists() and not sources.tombstone(tree).exists()


@pytest.mark.parametrize('interruption',['after_rename','partial_delete','pin_after_rename'])
def test_restart_only_deletes_authenticated_remaining_tombstone(source_setup,monkeypatch,interruption):
    collector,directory,observed,_=source_setup
    tree=directory/'repo/Tablet';trash=sources.tombstone(tree)
    original_sync=sources._sync_dir;original_delete=shutil.rmtree
    def sync(path):
        if trash.exists():
            if interruption=='pin_after_rename': put(directory/'retain-builds.json',{})
            elif interruption=='after_rename': raise OSError('simulated interruption')
        original_sync(path)
    def delete(path,*args,**kwargs):
        if Path(path)==trash:
            (trash/'nested/A.lean').unlink()
            raise OSError('partial deletion')
        return original_delete(path,*args,**kwargs)
    monkeypatch.setattr(sources,'_sync_dir',sync)
    if interruption=='partial_delete': monkeypatch.setattr(shutil,'rmtree',delete)
    with pytest.raises((OSError,ValueError)): sources.retire(collector,directory,observed,apply=True)
    assert trash.exists() and not tree.exists()
    monkeypatch.setattr(sources,'_sync_dir',original_sync);monkeypatch.setattr(shutil,'rmtree',original_delete)
    (directory/'retain-builds.json').unlink(missing_ok=True)
    assert sources.retire(collector,directory,observed,apply=True)['removed']==4
    assert not trash.exists()


def test_hashing_and_delete_offlock_full_inventory_inside(source_setup,monkeypatch):
    collector,directory,observed,_=source_setup
    held=False;inside=[];original_lock=collector.retirement_lock
    @contextmanager
    def lock():
        nonlocal held
        with original_lock() as acquired:
            held=acquired
            try: yield acquired
            finally: held=False
    monkeypatch.setattr(collector,'retirement_lock',lock)
    original_prove=sources._prove;original_inventory=sources.inventory;original_delete=shutil.rmtree
    def prove(*args):
        assert not held
        return original_prove(*args)
    def inventory(path):
        if held: inside.append(path)
        return original_inventory(path)
    def delete(*args,**kwargs):
        assert not held
        return original_delete(*args,**kwargs)
    monkeypatch.setattr(sources,'_prove',prove);monkeypatch.setattr(sources,'inventory',inventory);monkeypatch.setattr(shutil,'rmtree',delete)
    assert sources.retire(collector,directory,observed,apply=True)['removed']==4
    assert len(inside)==4


def test_workers_are_always_preserved(setup):
    collector,directory,observed,_,_=setup
    assert sources.retire(collector,directory,observed,apply=True)['status']=='worker_preserved'
    assert (directory/'repo/Tablet/A.lean').exists()


def test_retired_nonworker_still_supplies_worker_handoffs_and_transcripts(source_setup,tmp_path):
    collector,directory,observed,_=source_setup
    from trellis.dispatch import workspace,transcripts
    from trellis.dispatch.candidate import source_manifest,digest
    prepared=json.loads((directory/'prepared.json').read_bytes())
    request_id=prepared['request_id']
    # Use the supported completed-review response shape for both fixture roles;
    # this check exercises preserved chat files, not native verifier admission.
    response=canonical_json({'kind':'parallel_review','assignments':[],'action':None})
    (directory/'execution/response.json').write_bytes(response)
    put(directory/'execution/result.json',{'returncode':0,'response_sha256':digest(response)})
    chat=directory/f'repo/.trellis/chats/live/trellis_review_{request_id}_decision'
    chat.mkdir();(chat/'output.log').write_text('The useful original transcript.');(chat/'prompt.txt').write_text('Original prompt.')
    assert sources.retire(collector,directory,observed,apply=True)['removed']==4
    config=collector.repo/'trellis.config.json';put(config,{'repo_path':str(collector.repo)})
    runtime=collector.root.parent
    put(runtime/'runtime_metadata.json',{'repo_path':str(collector.repo),'config_path':str(config)})
    destination=collector.root/'100'
    comments='.trellis/scratch/note.json and .trellis/stuck-math-audit/cycle-1/probe.json'
    workspace.prepare(runtime,collector.repo,config,destination,
        {'in_flight_request':{'id':100,'kind':'Worker','reviewer_comments':comments}},
        expected_manifest=source_manifest(collector.repo),seed_packages=False,handoff_repo=directory/'repo')
    for relative in comments.split(' and '):
        assert (destination/'repo'/relative).read_bytes()==(directory/'repo'/relative).read_bytes()
    published=tmp_path/'published';published.mkdir()
    row=transcripts._publish(directory,published,prepared)
    assert len(row['chats'])==1
    assert (published/directory.name/chat.name/'output.log').read_bytes()==(chat/'output.log').read_bytes()


@pytest.mark.parametrize('fault',['active','uncommitted','pin'])
def test_initial_owner_guards_do_not_remove_sources(source_setup,fault):
    collector,directory,observed,_=source_setup
    if fault=='active': observed={}
    elif fault=='uncommitted': observed={directory.name:'different'}
    else: put(directory/'retain-builds.json',{})
    if fault=='pin':
        with pytest.raises(ValueError): sources.retire(collector,directory,observed,apply=True)
    else:
        assert sources.retire(collector,directory,observed,apply=True)['removed']==0
    assert (directory/'repo/Tablet/nested/A.lean').exists()


@pytest.mark.parametrize('fault',['owner','binding','escaped_root','changed_tombstone'])
def test_restart_rejects_changed_receipt_or_remaining_tombstone(source_setup,monkeypatch,fault):
    collector,directory,observed,_=source_setup
    tree=directory/'repo/Tablet';trash=sources.tombstone(tree)
    original_sync=sources._sync_dir
    def interrupted(path):
        if trash.exists(): raise OSError('stop after rename')
        original_sync(path)
    monkeypatch.setattr(sources,'_sync_dir',interrupted)
    with pytest.raises(OSError): sources.retire(collector,directory,observed,apply=True)
    monkeypatch.setattr(sources,'_sync_dir',original_sync)
    receipt=directory/sources.RECEIPT;record=json.loads(receipt.read_bytes())
    if fault=='owner': record['owner']['id']+=1
    elif fault=='binding': record['trees']['repo/Tablet']['binding']['source_head']='f'*40
    elif fault=='escaped_root': record['trees']['../outside']=record['trees'].pop('repo/Tablet')
    else: (trash/'nested/A.lean').write_text('new unique bytes in tombstone')
    if fault!='changed_tombstone': receipt.write_bytes(canonical_json(record))
    with pytest.raises(ValueError): sources.retire(collector,directory,observed,apply=True)
    assert trash.exists() and not tree.exists()


def test_background_collector_reaches_recoverable_nonworker_sources(source_setup):
    from trellis.dispatch.storage import BackgroundCollector
    original,directory,_,_=source_setup
    collector=BackgroundCollector(original.root,repo=original.repo)
    collector._review_snapshots=None
    state=json.loads((original.root.parent/'protocol_state.json').read_bytes())
    assert collector.schedule(state)
    assert collector.wait_idle(15)
    assert collector.summary['source_trees']==4
    assert collector.summary['errors']==0
    assert not (directory/'repo/Tablet').exists()
    assert (directory/'repo/.trellis/scratch/note.json').exists()
