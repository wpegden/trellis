import hashlib
import json
import shutil
import threading
from pathlib import Path

import pytest

from test_check_jobs import setup_project, request
from trellis.checker import jobs, job_workspace, mirror_retention


def prepared(tmp_path, monkeypatch):
    repo, runtime, token, service = setup_project(tmp_path)
    monkeypatch.setattr(job_workspace, '_seed_tablet_build', lambda *_: 0)
    key = hashlib.sha256(str(repo).encode()).hexdigest()
    directory = service.root / 'workspaces' / key
    _, snapshot = job_workspace.capture(repo)
    job_workspace.prepare_mirror(repo, directory/'repo', snapshot)
    build=directory/'repo/.lake/build';build.mkdir(parents=True)
    (build/'cache.olean').write_bytes(b'optional cache')
    (directory/'home').mkdir()
    (directory/'home/tmp').write_text('compiler temporary file')
    session=service.sessions[str(repo)]=jobs.WorkspaceSession()
    session.materialized_dependencies=('old dependencies',)
    return repo,runtime,token,service,directory,snapshot


def test_complete_mirror_detaches_and_original_snapshot_rematerializes(tmp_path, monkeypatch):
    repo,runtime,token,service,directory,snapshot=prepared(tmp_path,monkeypatch)
    original=(directory/'repo/Tablet/N.lean').read_bytes()
    jobs.revoke_workspace(runtime,token)
    service._collect_mirror_builds()
    assert not directory.exists()
    queued=list(service.mirror_retirement.queue.iterdir());assert len(queued)==1
    assert (queued[0]/'packages-ready').exists() and (queued[0]/'input-files.json').exists()
    assert service.sessions[str(repo)].materialized_dependencies is None
    # A fresh mirror re-runs package preparation and uses the submitted bytes,
    # even after the registered source has changed.
    (repo/'Tablet/N.lean').write_text('later source edit')
    from trellis import supervisor_workspace
    original_packages=supervisor_workspace._ensure_supervisor_lake_packages
    calls=[]
    def packages(*args,**kwargs):
        calls.append(True);return original_packages(*args,**kwargs)
    monkeypatch.setattr(supervisor_workspace,'_ensure_supervisor_lake_packages',packages)
    job_workspace.prepare_mirror(repo,directory/'repo',snapshot)
    assert calls==[True] and (directory/'repo/Tablet/N.lean').read_bytes()==original
    service.mirror_retirement.drain()
    assert not queued[0].exists() and (directory/'repo/Tablet/N.lean').read_bytes()==original


@pytest.mark.parametrize('guard',['capability','queued','claim','warm','cleanup_failed','active'])
def test_existing_admission_and_custody_guards_protect_whole_mirror(tmp_path,monkeypatch,guard):
    repo,runtime,token,service,directory,_=prepared(tmp_path,monkeypatch)
    if guard in {'queued','claim'}:
        job=service.handle(request(token))
        if guard=='claim':
            service._finish(job['job_id'],'cancelled',{})
            service.claimed[job['job_id']]=(str(repo),'interactive','owner')
    if guard!='capability':jobs.revoke_workspace(runtime,token)
    if guard=='warm':service.sessions[str(repo)].broker=object()
    if guard=='cleanup_failed':service.scheduler.cleanup_failed=True
    if guard=='active':service.scheduler.active['compiler']={}
    service._collect_mirror_builds()
    assert (directory/'repo/.lake/build/cache.olean').read_bytes()==b'optional cache'
    assert not service.mirror_retirement.queue.exists()


@pytest.mark.parametrize('fault',['unmarked','unknown','malformed','traversal','git_file','git_dir','git_alias','repo_alias','marker_alias'])
def test_unusual_mirrors_keep_legacy_build_only_behavior(tmp_path,monkeypatch,fault):
    repo,runtime,token,service,directory,_=prepared(tmp_path,monkeypatch)
    if fault=='unmarked':(directory/'input-files.json').unlink()
    elif fault=='unknown':(directory/'investigation.txt').write_text('keep')
    elif fault=='malformed':(directory/'input-files.json').write_text('{')
    elif fault=='traversal':(directory/'input-files.json').write_text('["../outside"]')
    elif fault=='git_file':(directory/'repo/.git').write_text('gitdir: outside')
    elif fault=='git_dir':(directory/'repo/.git').mkdir()
    elif fault=='git_alias':(directory/'repo/.git').symlink_to(tmp_path/'missing-git')
    elif fault=='repo_alias':
        actual=directory/'repo';outside=tmp_path/'outside';actual.rename(outside);actual.symlink_to(outside)
    else:
        marker=directory/'input-files.json';data=marker.read_bytes();marker.unlink()
        outside=tmp_path/'marker';outside.write_bytes(data);marker.symlink_to(outside)
    jobs.revoke_workspace(runtime,token)
    service._collect_mirror_builds()
    assert directory.exists() and (directory/'repo/Tablet/N.lean').exists()
    assert not service.mirror_retirement.queue.exists()
    if fault=='repo_alias':assert (directory/'repo/.lake/build/cache.olean').exists()
    else:assert not (directory/'repo/.lake/build').exists()


def test_detached_deletion_holds_neither_lock_and_cannot_delete_new_mirror(tmp_path,monkeypatch):
    repo,runtime,token,service,directory,snapshot=prepared(tmp_path,monkeypatch)
    jobs.revoke_workspace(runtime,token);service._collect_mirror_builds()
    queued=next(service.mirror_retirement.queue.iterdir())
    reached=threading.Event();proceed=threading.Event();original=shutil.rmtree
    def slow(path,*args,**kwargs):
        assert path==queued
        assert not service.lock._is_owned() and not service.scheduler.condition._is_owned()
        reached.set();assert proceed.wait(10)
        return original(path,*args,**kwargs)
    slow.avoids_symlink_attacks=True
    monkeypatch.setattr(mirror_retention.shutil,'rmtree',slow)
    thread=threading.Thread(target=service.mirror_retirement.drain);thread.start()
    try:
        assert reached.wait(10)
        # Real submission acquires admission/database locks while deletion waits.
        new_token=jobs.register_workspace(runtime,repo,scope='next')['TRELLIS_CHECK_JOBS_TOKEN']
        job=service.handle(request(new_token))
        assert job['state']=='queued'
        job_workspace.prepare_mirror(repo,directory/'repo',snapshot)
        (directory/'repo/new-cache').write_text('new incarnation')
    finally:
        proceed.set();thread.join(10)
    assert not thread.is_alive() and not queued.exists()
    assert (directory/'repo/new-cache').read_text()=='new incarnation'


def test_partial_deletion_restarts_from_queue_without_busy_retry(tmp_path,monkeypatch,caplog):
    repo,runtime,token,service,directory,_=prepared(tmp_path,monkeypatch)
    jobs.revoke_workspace(runtime,token);service._collect_mirror_builds()
    queued=next(service.mirror_retirement.queue.iterdir());attempts=[]
    original=shutil.rmtree
    def partial(path):
        attempts.append(path);(path/'home/tmp').unlink();raise OSError('partial failure')
    partial.avoids_symlink_attacks=True
    monkeypatch.setattr(mirror_retention.shutil,'rmtree',partial)
    service.mirror_retirement.drain()
    assert attempts==[queued] and queued.exists() and not directory.exists()
    assert service.mirror_retirement.last_error=='partial failure'
    assert 'Advisory mirror retirement deferred: partial failure' in caplog.text
    service.mirror_retirement._error(OSError('partial failure'))
    assert caplog.text.count('Advisory mirror retirement deferred: partial failure')==1
    monkeypatch.setattr(mirror_retention.shutil,'rmtree',original)
    # A fresh service queue reader needs no in-memory receipt to recover.
    restarted=mirror_retention.MirrorRetirement(service.root,threading.Event())
    restarted.drain();assert not queued.exists() and restarted.removed==1


def test_rename_parent_sync_failure_preserves_entire_detached_tree(tmp_path,monkeypatch):
    repo,runtime,token,service,directory,_=prepared(tmp_path,monkeypatch)
    jobs.revoke_workspace(runtime,token);service._collect_mirror_builds()
    queued=next(service.mirror_retirement.queue.iterdir());original=mirror_retention._sync
    monkeypatch.setattr(mirror_retention,'_sync',lambda _:(_ for _ in ()).throw(OSError('fsync failure')))
    with pytest.raises(OSError,match='fsync failure'):service.mirror_retirement.drain()
    assert (queued/'home/tmp').exists()
    monkeypatch.setattr(mirror_retention,'_sync',original)
    service.mirror_retirement.drain();assert not queued.exists()


def test_queue_alias_and_replaced_inode_are_not_deletion_authority(tmp_path,monkeypatch):
    repo,runtime,token,service,directory,_=prepared(tmp_path,monkeypatch)
    jobs.revoke_workspace(runtime,token);service._collect_mirror_builds()
    queued=next(service.mirror_retirement.queue.iterdir())
    saved=tmp_path/'saved';queued.rename(saved);queued.mkdir();(queued/'keep').write_text('replacement')
    service.mirror_retirement.drain();assert (queued/'keep').read_text()=='replacement'
    queue=service.mirror_retirement.queue;old=tmp_path/'queue';queue.rename(old);queue.symlink_to(old)
    with pytest.raises(ValueError,match='aliased'):service.mirror_retirement.drain()
    assert (old/queued.name/'keep').exists()


def test_background_start_recovers_queue_and_stops(tmp_path,monkeypatch):
    repo,runtime,token,service,directory,_=prepared(tmp_path,monkeypatch)
    jobs.revoke_workspace(runtime,token);service._collect_mirror_builds()
    queued=next(service.mirror_retirement.queue.iterdir())
    recovered=jobs.JobService(runtime)
    reached=threading.Event();original=recovered.mirror_retirement.drain
    def drain():
        original();reached.set()
    monkeypatch.setattr(recovered.mirror_retirement,'drain',drain)
    recovered.start()
    try:
        assert reached.wait(10) and not queued.exists()
        assert recovered.mirror_retirement.removed==1
    finally:
        recovered.shutdown();recovered.mirror_retirement.thread.join(10)
    assert not recovered.mirror_retirement.thread.is_alive()


def test_retirement_preserves_terminal_job_and_snapshot_records(tmp_path,monkeypatch):
    repo,runtime,token,service,directory,_=prepared(tmp_path,monkeypatch)
    job=service.handle(request(token))
    service._finish(job['job_id'],'succeeded',dict(returncode=0,message='retained outcome'))
    row=dict(service.db.execute('SELECT * FROM jobs WHERE id=?',(job['job_id'],)).fetchone())
    snapshot=service.root/'snapshots'/(row['snapshot']+'.json');raw=snapshot.read_bytes()
    requests=[tuple(r) for r in service.db.execute('SELECT * FROM requests')]
    jobs.revoke_workspace(runtime,token);service._collect_mirror_builds();service.mirror_retirement.drain()
    assert not directory.exists() and snapshot.read_bytes()==raw
    assert dict(service.db.execute('SELECT * FROM jobs WHERE id=?',(job['job_id'],)).fetchone())==row
    assert [tuple(r) for r in service.db.execute('SELECT * FROM requests')]==requests
