import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
import time

import pytest

from trellis.dispatch import git_maintenance as m


def git(repo,*args,data=None):
    return subprocess.check_output(['git','-C',str(repo),*args],input=data,stderr=subprocess.PIPE)


@pytest.fixture
def repo(tmp_path,monkeypatch):
    p=tmp_path/'repo';p.mkdir();git(p,'init','-q');git(p,'config','user.name','Test');git(p,'config','user.email','test@example.invalid')
    (p/'file').write_text('first\n');git(p,'add','.');git(p,'commit','-qm','first');git(p,'repack','-d','-l')
    (p/'file').write_text('second\n');git(p,'add','.');git(p,'commit','-qm','second')
    monkeypatch.setattr(m,'_external_maintenance',lambda *a:None)
    return p


def wait(predicate,seconds=5):
    end=time.monotonic()+seconds
    while not predicate():
        assert time.monotonic()<end
        time.sleep(.01)


def test_threshold_packs_all_observed_reachable_loose_and_preserves_objects(repo):
    dangling=git(repo,'hash-object','-w','--stdin',data=b'private unreachable').strip().decode()
    before=m._loose(repo/'.git/objects');packs=m._packs(repo/'.git/objects')
    metadata={str(p.relative_to(repo/'.git')):p.read_bytes() for name in ('refs','logs') for p in (repo/'.git'/name).rglob('*') if p.is_file()}
    metadata.update({name:(repo/'.git'/name).read_bytes() for name in ('HEAD','index','config')})
    result=m.collect(repo,threshold_bytes=1)
    assert result['repacked'] and result['verified_objects']>=len(before)
    assert m._packs(repo/'.git/objects').items()>=packs.items()
    assert git(repo,'cat-file','blob',dangling)==b'private unreachable'
    assert all((repo/'.git'/name).read_bytes()==raw for name,raw in metadata.items())
    assert not (repo/'.git/objects/maintenance.lock').exists()
    assert m.collect(repo,threshold_bytes=1)['status']=='below_threshold'


def test_large_dangling_only_never_triggers_repeated_pack(repo):
    git(repo,'repack','-d','-l')
    git(repo,'hash-object','-w','--stdin',data=os.urandom(100000))
    def forbidden(*_):raise AssertionError('unreachable data must not cause repacking')
    for _ in range(3):
        result=m.collect(repo,threshold_bytes=1,execute=forbidden)
        assert result['loose_bytes']>100000 and result['reachable_loose_bytes']==0


def test_below_threshold_does_no_reachability_walk_or_process_launch(repo,monkeypatch):
    def forbidden(*a,**kw):raise AssertionError('unnecessary work')
    monkeypatch.setattr(m,'_reachable',forbidden)
    assert not m.collect(repo,execute=forbidden)['repacked']


def test_own_lock_excludes_duplicate_service(repo):
    with m._leases(repo,repo/'.git/objects'):
        with pytest.raises(m.Busy):m.collect(repo,threshold_bytes=1)
    assert not (repo/'.git/objects/maintenance.lock').exists()


def test_cancel_before_launch_and_error_release_native_lock(repo):
    stop=threading.Event()
    def cancel(*_):stop.set();raise m.Cancelled()
    with pytest.raises(m.Cancelled):m.collect(repo,threshold_bytes=1,cancelled=stop.is_set,execute=cancel)
    assert not (repo/'.git/objects/maintenance.lock').exists()
    with pytest.raises(m.Cancelled):m.collect(repo,threshold_bytes=1,cancelled=stop.is_set)
    assert m.collect(repo,threshold_bytes=1)['repacked']


def test_worker_retries_error_and_observes_new_arrivals_without_schedule(repo,monkeypatch):
    actual=m.collect;failed=threading.Event()
    def first_failure(*args,**kwargs):
        if not failed.is_set():failed.set();raise RuntimeError('injected')
        return actual(*args,**kwargs)
    monkeypatch.setattr(m,'collect',first_failure)
    worker=m.Worker(repo,threshold_bytes=1,interval=.05);worker.schedule()
    try:
        wait(lambda:worker.summary['repacks']==1)
        assert worker.summary['errors']==1
        (repo/'file').write_text('third\n');git(repo,'add','.');git(repo,'commit','-qm','third')
        wait(lambda:worker.summary['repacks']==2)
        assert worker.thread.daemon
    finally:
        worker.close();assert worker.wait(5)


def test_native_child_cancel_reaps_owned_group_releases_lease(repo,tmp_path,monkeypatch):
    started=tmp_path/'started'
    # Ignores TERM so the explicit-close grace escalation is exercised.
    code='import signal,time,pathlib;signal.signal(signal.SIGTERM,signal.SIG_IGN);pathlib.Path('+repr(str(started))+').write_text("ready");time.sleep(60)'
    monkeypatch.setattr(m,'REPACK',[sys.executable,'-c',code])
    worker=m.Worker(repo,threshold_bytes=1,interval=.05);worker.schedule()
    wait(started.exists)
    assert worker.summary['status']=='running' and worker.summary['started_at']>0
    before=time.monotonic();worker.close()
    assert worker.wait(4) and time.monotonic()-before<3
    assert not (repo/'.git/objects/maintenance.lock').exists()
    assert worker.summary['status']=='stopped'


def test_background_lane_does_not_wait_for_coordinator_or_cleanup_idle(repo,tmp_path):
    import fcntl
    from trellis.dispatch.storage import BackgroundCollector
    root=tmp_path/'runtime';root.mkdir()
    lock=(root/'coordinator.lock').open('w');fcntl.flock(lock,fcntl.LOCK_EX)
    collector=BackgroundCollector(root,repo=repo,git_maintenance_threshold_bytes=1,git_maintenance_interval=.05)
    state={'dispatch':{'tasks':{},'receipts':{},'verifiers':{'jobs':{}}}}
    try:
        assert not collector.schedule(state)
        assert collector.wait_idle(.01)
        wait(lambda:collector.summary['git_maintenance']['repacks']==1)
    finally:
        collector.close();assert collector._git_maintenance.wait(5);lock.close()


def test_schedule_and_close_do_no_filesystem_work_in_caller(repo,monkeypatch):
    worker=m.Worker(repo)
    monkeypatch.setattr(threading.Thread,'start',lambda _:None)
    monkeypatch.setattr(Path,'stat',lambda *a,**kw: (_ for _ in ()).throw(AssertionError('main-thread I/O')))
    worker.schedule();worker.close()


def test_invalid_threshold_and_interval(repo):
    for threshold in [0,-1,True,1.5]:
        with pytest.raises(ValueError):m.Worker(repo,threshold_bytes=threshold)
    for interval in [0,float('inf')]:
        with pytest.raises(ValueError):m.Worker(repo,interval=interval)


def child_environment():
    return {**os.environ,'PYTHONPATH':str(Path(__file__).resolve().parents[1])}


def test_real_sidecar_close_only_reaps_child_before_process_exit(repo,tmp_path):
    code='''import sys,time
from pathlib import Path
from trellis.dispatch import git_maintenance as m
from trellis.dispatch.storage import BackgroundCollector
repo=Path(sys.argv[1]);root=Path(sys.argv[2]);root.mkdir()
m._external_maintenance=lambda *a:None
started=root/'started'
m.REPACK=[sys.executable,'-c','import signal,time,pathlib;signal.signal(signal.SIGTERM,signal.SIG_IGN);pathlib.Path('+repr(str(started))+').touch();time.sleep(60)']
c=BackgroundCollector(root,repo=repo,git_maintenance_threshold_bytes=1)
c.schedule({'dispatch':{'tasks':{},'receipts':{},'verifiers':{'jobs':{}}}})
while not started.exists():time.sleep(.01)
c.close()
'''
    result=subprocess.run([sys.executable,'-c',code,str(repo),str(tmp_path/'runtime')],env=child_environment(),capture_output=True,timeout=8)
    assert result.returncode==0,result.stderr.decode()
    assert not (repo/'.git/objects/maintenance.lock').exists()
    with m._leases(repo,repo/'.git/objects'):pass


def test_killed_parent_keeps_inherited_lease_until_owned_child_finishes(repo,tmp_path):
    started=tmp_path/'started';done=tmp_path/'done'
    code="""import sys
from pathlib import Path
from trellis.dispatch import git_maintenance as m
repo=Path(sys.argv[1]);started=Path(sys.argv[2]);done=Path(sys.argv[3])
m._external_maintenance=lambda *a:None
m.REPACK=[sys.executable,'-c','import os,time,pathlib;pathlib.Path('+repr(str(started))+').write_text(str(os.getpgrp()));time.sleep(2);pathlib.Path('+repr(str(done))+').touch()']
m.collect(repo,threshold_bytes=1)
"""
    process=subprocess.Popen([sys.executable,'-c',code,str(repo),str(started),str(done)],env=child_environment())
    try:
        wait(started.exists);process.kill();process.wait()
        with pytest.raises(m.Busy):
            with m._leases(repo,repo/'.git/objects'):pass
        wait(done.exists)
        def released():
            try:
                with m._leases(repo,repo/'.git/objects'):return True
            except m.Busy:return False
        wait(released)
        assert m.collect(repo,threshold_bytes=1)['repacked']
    finally:
        if process.poll() is None:process.kill();process.wait()
        if started.exists() and not done.exists():
            try:os.killpg(int(started.read_text()),signal.SIGKILL)
            except ProcessLookupError:pass


def test_clone_during_packing_inherits_no_control_files(repo,tmp_path):
    clone=tmp_path/'clone'
    def clone_then_pack(path,fd):
        subprocess.run(['git','clone','--local','--no-checkout',str(path),str(clone)],capture_output=True,check=True)
        assert not list((clone/'.git/objects').rglob('*.lock'))
        assert not list((clone/'.git/objects').rglob('.trellis-*'))
        assert not (clone/'.git/trellis-byte-maintenance.flock').exists()
        subprocess.run(m.REPACK,cwd=path,env=m._env(),capture_output=True,check=True)
    assert m.collect(repo,threshold_bytes=1,execute=clone_then_pack)['repacked']
    assert git(clone,'show','HEAD:file')==b'second\n'


def test_maintenance_native_marker_and_pack_locks_defer(repo,monkeypatch):
    monkeypatch.undo()
    # The actual process scan can see concurrent tests/live maintenance, so
    # exercise known-path rejection before that scan begins.
    for name in ['gc.pid.lock','objects/maintenance.lock','objects/pack/maintenance.lock']:
        path=repo/'.git'/name;path.write_text('foreign')
        try:
            with pytest.raises(m.Busy):m._external_maintenance(repo)
        finally:path.unlink()
