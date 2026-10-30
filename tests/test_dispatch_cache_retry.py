import copy
import fcntl
import shutil
import threading
import time

import pytest

from test_dispatch_terminal_retention import setup, put, git
from test_dispatch_cache_lane import wait_for
from trellis.dispatch import storage, terminal_retention as retention


def cohort(setup):
    original,directory,_,_,state=setup
    newer=original.root/'2';shutil.copytree(directory,newer)
    state['dispatch']['tasks']['2']=dict(copy.deepcopy(state['dispatch']['tasks']['1']),id=2)
    put(original.repo/retention.HISTORY,{'state':state})
    git(original.repo,'add','.');git(original.repo,'commit','-qm','Terminal second owner')
    build=put(newer/'repo/.lake/build/example.olean',{})
    return original,directory,newer,state,build


@pytest.mark.parametrize('deferral',['unreaped','live','pin'])
def test_deferred_cache_retries_without_new_state_or_retention_completion(setup,monkeypatch,deferral):
    original,directory,newer,state,build=cohort(setup)
    process={'pid':123};alive=[deferral=='live']
    if deferral=='pin':put(newer/'retain-builds.json',{})
    else:
        put(newer/'execution/process.json',process)
        if deferral=='live':put(newer/'execution/cleanup.json',process)
    monkeypatch.setattr(storage,'alive',lambda _:alive[0])
    monkeypatch.setattr('trellis.dispatch.process.alive',lambda _:alive[0])
    collector=storage.BackgroundCollector(original.root,repo=original.repo,cache_retry_seconds=.1)
    collector._review_snapshots=None
    try:
        with(original.root/'coordinator.lock').open('rb')as held:
            fcntl.flock(held,fcntl.LOCK_EX)
            collector.schedule(state)
            wait_for(lambda:newer in collector._cache_retry)
            assert build.exists()and not collector.wait_idle(.01)
            if deferral=='pin':(newer/'retain-builds.json').unlink()
            else:
                alive[0]=False;put(newer/'execution/cleanup.json',process)
            # No schedule() call and no canonical bytes change after reaping.
            wait_for(lambda:not build.exists())
            assert not collector.wait_idle(.01)
    finally:collector.close()
    assert collector.wait_idle(10)


def test_frequent_schedule_does_not_bypass_retry_spacing_or_duplicate_active_owner(setup,monkeypatch):
    original,_,newer,state,build=cohort(setup)
    put(newer/'retain-builds.json',{})
    collector=storage.BackgroundCollector(original.root,repo=original.repo,cache_retry_seconds=60)
    collector._review_snapshots=None
    calls=[];prune=storage.prune
    def counted(path,**kwargs):
        calls.append(path);return prune(path,**kwargs)
    monkeypatch.setattr(storage,'prune',counted)
    with(original.root/'coordinator.lock').open('rb')as held:
        fcntl.flock(held,fcntl.LOCK_EX);collector.schedule(state)
        wait_for(lambda:newer in collector._cache_retry)
        for _ in range(50):collector.schedule(state)
        assert calls.count(newer)==1 and build.exists()
        collector.close()
    assert collector.wait_idle(10)


def test_retry_rechecks_latest_owner_is_terminal(setup):
    original,_,newer,state,build=cohort(setup)
    pin=put(newer/'retain-builds.json',{})
    collector=storage.BackgroundCollector(original.root,repo=original.repo,cache_retry_seconds=.05)
    collector._review_snapshots=None
    with(original.root/'coordinator.lock').open('rb')as held:
        fcntl.flock(held,fcntl.LOCK_EX);collector.schedule(state)
        wait_for(lambda:newer in collector._cache_retry)
        changed=copy.deepcopy(state);changed['dispatch']['tasks']['2']['status']={'status':'checking'}
        collector.schedule(changed);pin.unlink()
        threading.Event().wait(.15)
        assert build.exists()and newer not in collector._cache_queue
        collector.close()
    assert collector.wait_idle(10)


def test_successful_cache_is_not_retried_during_long_drain(setup,monkeypatch):
    original,_,newer,state,build=cohort(setup)
    collector=storage.BackgroundCollector(original.root,repo=original.repo,cache_retry_seconds=.03)
    collector._review_snapshots=None
    calls=[];prune=storage.prune
    def counted(path,**kwargs):calls.append(path);return prune(path,**kwargs)
    monkeypatch.setattr(storage,'prune',counted)
    with(original.root/'coordinator.lock').open('rb')as held:
        fcntl.flock(held,fcntl.LOCK_EX);collector.schedule(state)
        wait_for(lambda:newer in collector._cache_done)
        for _ in range(10):collector.schedule(state)
        threading.Event().wait(.1)
        assert calls.count(newer)==1 and not build.exists()
        collector.close()
    assert collector.wait_idle(10)


def test_deferrals_alone_do_not_keep_drain_alive(setup):
    original,directory,_,_,state=setup
    put(directory/'retain-builds.json',{})
    build=put(directory/'repo/.lake/build/example.olean',{})
    collector=storage.BackgroundCollector(original.root,repo=original.repo,cache_retry_seconds=60)
    collector._review_snapshots=None
    collector.schedule(state)
    assert collector.wait_idle(10)and build.exists()
    (directory/'retain-builds.json').unlink()
    assert collector.schedule(state)and collector.wait_idle(10)
    assert not build.exists()


def test_close_prevents_timed_retry_after_build_lease_release(setup,monkeypatch):
    original,_,newer,state,build=cohort(setup)
    pin=put(newer/'retain-builds.json',{})
    collector=storage.BackgroundCollector(original.root,repo=original.repo,cache_retry_seconds=.05)
    collector._review_snapshots=None
    calls=[];prune=storage.prune
    def counted(path,**kwargs):calls.append(path);return prune(path,**kwargs)
    monkeypatch.setattr(storage,'prune',counted)
    with(original.root/'coordinator.lock').open('rb')as held:
        fcntl.flock(held,fcntl.LOCK_EX);collector.schedule(state)
        wait_for(lambda:newer in collector._cache_retry)
        collector.close();pin.unlink()
    assert collector.wait_idle(10)
    count=len(calls)
    with(original.root/'build-cleanup.lock').open('rb')as lease:
        fcntl.flock(lease,fcntl.LOCK_EX|fcntl.LOCK_NB)
        threading.Event().wait(.15)
        assert len(calls)==count and build.exists()


@pytest.mark.parametrize('interval',[0,-1,float('nan'),float('inf')])
def test_retry_interval_must_be_finite_positive(tmp_path,interval):
    with pytest.raises(ValueError):storage.BackgroundCollector(tmp_path,cache_retry_seconds=interval)


def test_due_cache_retry_progresses_during_continuous_batch_discovery(setup,monkeypatch):
    original,_,newer,state,build=cohort(setup)
    pin=put(newer/'retain-builds.json',{})
    collector=storage.BackgroundCollector(original.root,repo=original.repo,cache_retry_seconds=.05)
    collector._review_snapshots=None
    collector._batch_iterator=(item for item in ())
    def batch():
        threading.Event().wait(.005)
        return True
    monkeypatch.setattr(collector,'_collect_batch_cache',batch)
    try:
        with(original.root/'coordinator.lock').open('rb')as held:
            fcntl.flock(held,fcntl.LOCK_EX);collector.schedule(state)
            wait_for(lambda:newer in collector._cache_retry)
            pin.unlink()
            wait_for(lambda:not build.exists())
            assert not collector.wait_idle(.01)
            collector.close()
    finally:collector.close()
    assert collector.wait_idle(10)


def batch_cohort(setup):
    from test_dispatch_batch_cleanup import cache
    original,directory,_,_,state=setup
    state['dispatch']['tasks']['1']['review_id']=7
    state['dispatch']['tasks']['2']=dict(copy.deepcopy(state['dispatch']['tasks']['1']),id=2,status={'status':'checking'})
    state['dispatch']['receipts']={'7':{'review_id':7}}
    put(original.repo/retention.HISTORY,{'state':state})
    git(original.repo,'add','.');git(original.repo,'commit','-qm','Batch owner lifecycle')
    donor=cache(original.root,'deferred',workers=('2',))
    return original,state,donor


@pytest.mark.parametrize('deferral',['worker','pin','cleanup'])
def test_exact_batch_cache_retries_during_unrelated_retirement(setup,monkeypatch,deferral):
    original,state,donor=batch_cohort(setup)
    if deferral!='worker':state['dispatch']['tasks']['2']['status']={'status':'resolved'}
    pin=None
    if deferral=='pin':pin=put(original.root/'2/retain-builds.json',{})
    if deferral=='cleanup':
        put(donor/'execution/process.json',{'pid':123})
        monkeypatch.setattr(storage,'alive',lambda _:False)
    collector=storage.BackgroundCollector(original.root,repo=original.repo,cache_retry_seconds=.1)
    collector._review_snapshots=None
    scans=[];discover=storage._batch_candidates
    def candidates(root,reviews):
        scans.extend(reviews);yield from discover(root,reviews)
    monkeypatch.setattr(storage,'_batch_candidates',candidates)
    try:
        with(original.root/'coordinator.lock').open('rb')as held:
            fcntl.flock(held,fcntl.LOCK_EX);collector.schedule(state)
            wait_for(lambda:donor in collector._batch_retry)
            assert(donor/'repo/.lake').exists()
            if deferral=='worker':
                changed=copy.deepcopy(state);changed['dispatch']['tasks']['2']['status']={'status':'resolved'}
                collector.schedule(changed)
            elif deferral=='pin':pin.unlink()
            else:put(donor/'execution/cleanup.json',{'pid':123})
            wait_for(lambda:not(donor/'repo/.lake').exists())
            assert not collector.wait_idle(.01)
            # Timer-only retries use the saved path, never rewalk the catalog.
            if deferral!='worker':assert scans==['7']
            collector.close()
    finally:collector.close()
    assert collector.wait_idle(10)


def test_new_terminal_binding_rediscovers_only_its_changed_review(setup,monkeypatch):
    from test_dispatch_batch_cleanup import cache
    original,state,donor=batch_cohort(setup)
    shutil.rmtree(donor)
    collector=storage.BackgroundCollector(original.root,repo=original.repo,cache_retry_seconds=.1)
    collector._review_snapshots=None
    scans=[];discover=storage._batch_candidates
    def candidates(root,reviews):scans.extend(reviews);yield from discover(root,reviews)
    monkeypatch.setattr(storage,'_batch_candidates',candidates)
    with(original.root/'coordinator.lock').open('rb')as held:
        fcntl.flock(held,fcntl.LOCK_EX);collector.schedule(state)
        wait_for(lambda:scans==['7']and collector._batch_iterator is None)
        donor=cache(original.root,'late',workers=('2',))
        for _ in range(5):collector.schedule(state)
        assert scans==['7']and(donor/'repo/.lake').exists()
        changed=copy.deepcopy(state);changed['dispatch']['tasks']['2']['status']={'status':'resolved'}
        collector.schedule(changed)
        wait_for(lambda:not(donor/'repo/.lake').exists())
        assert scans==['7','7']
        collector.close()
    assert collector.wait_idle(10)


def test_deferred_batch_rechecks_receipt_removal_and_pin(setup):
    original,state,donor=batch_cohort(setup)
    collector=storage.BackgroundCollector(original.root,repo=original.repo,cache_retry_seconds=.05)
    collector._review_snapshots=None
    with(original.root/'coordinator.lock').open('rb')as held:
        fcntl.flock(held,fcntl.LOCK_EX);collector.schedule(state)
        wait_for(lambda:donor in collector._batch_retry)
        changed=copy.deepcopy(state);changed['dispatch']['receipts']={}
        changed['dispatch']['tasks']['2']['status']={'status':'resolved'}
        collector.schedule(changed)
        threading.Event().wait(.15)
        assert(donor/'repo/.lake').exists()and donor not in collector._batch_retry
        collector.close()
    assert collector.wait_idle(10)


def test_oversized_cache_marker_is_bounded_and_preserves_build(setup):
    _,directory,_,_,_=setup
    build=put(directory/'repo/.lake/build/example.olean',{})
    (directory/'build-caches-pruned.json').write_bytes(b' '*65537)
    assert not storage.BackgroundCollector._cache_complete(directory)
    with pytest.raises(ValueError,match='oversized'):storage.prune(directory)
    assert build.exists()


@pytest.mark.parametrize('resolved',[False,True])
def test_batch_rediscovery_preserves_already_queued_retry_membership(tmp_path,monkeypatch,resolved):
    from test_dispatch_batch_cleanup import cache,queued,state
    donor=cache(tmp_path,'overlap')
    collector=queued(tmp_path,state(1)if resolved else state(),monkeypatch)
    collector._batch_iterator=iter([('7',donor)])
    collector._batch_retry_queue.append(('7',donor));collector._batch_retry_queued.add(donor)
    collector._batch_retry[donor]=('7',0)
    collector._batch_retry_turn=False  # Rediscovery wins this turn.
    assert collector._collect_batch_cache()
    assert donor in collector._batch_retry_queued
    assert len(collector._batch_retry_queue)==1
    if resolved:assert donor not in collector._batch_retry
    else:collector._batch_retry[donor]=('7',0)
    with collector._guard:collector._admit_batch_retries()
    assert len(collector._batch_retry_queue)==1
    collector._batch_tasks={'1':7}
    assert collector._collect_batch_cache()
    assert donor not in collector._batch_retry_queued and donor not in collector._batch_retry
    assert not collector._batch_retry_queue and not(donor/'repo/.lake').exists()
