from contextlib import contextmanager
import copy
import json
import os
from pathlib import Path

import pytest

from test_dispatch_terminal_retention import setup, put, git
from test_dispatch_worker_source_retention import worker_setup
from trellis.dispatch import accepted_source_retention as copies
from trellis.dispatch import terminal_retention as terminal, candidate, handoffs


def retire(collector, directory, observed, *, apply=False):
    def accepted(name):
        state = json.loads((collector.root.parent/'protocol_state.json').read_text())
        return (terminal.terminal_tasks(state).get(name), terminal.accepted_source_candidates(state).get(name))
    return copies.retire(collector, directory, observed, apply=apply, accepted=accepted)


def checkpoint(collector, state):
    put(collector.repo / terminal.HISTORY, {'state': state})
    put(collector.root.parent / 'protocol_state.json', state)
    git(collector.repo, 'add', terminal.HISTORY)
    git(collector.repo, 'commit', '-qm', 'Publication witness')
    return terminal.terminal_tasks(state)


@pytest.fixture
def accepted(worker_setup):
    collector, directory, _, data, state = worker_setup
    task = state['dispatch']['tasks']['1']
    task['status'] = dict(status='resolved', outcome=dict(kind='accepted', event_id=41), resolved_at_ms=200)
    task['assignment']['scope'] = {'nodes': ['A', 'Unique']}
    (directory/'repo/Tablet/Unique.lean').write_text('Unique accepted proof')
    sha = candidate.seal(directory/'repo', directory/'candidate', task_id=1,
        base_manifest=json.loads((directory/'base-manifest.json').read_text()), inputs={}, response={'outcome':'Valid'})
    state['dispatch']['publications'] = {'1': dict(candidate_sha256=sha, payload_sha256='a'*64, event_id=41)}
    process = dict(pid=2147483647, start_ticks=1, boot_id='finished')
    put(directory/'execution/process.json', process)
    put(directory/'execution/cleanup.json', process)
    observed = checkpoint(collector, state)
    return collector, directory, observed, state


def test_retire_exact_outer_sources_preserves_unique_seal_and_future_handoff(accepted):
    collector, directory, observed, state = accepted
    task = state['dispatch']['tasks']['1']
    before = handoffs._findings(directory, task)
    reviewer_before = collector.root.parent/'reviewer-before'
    handoff_before = handoffs.worker_handoffs(collector.root, reviewer_before, [task])
    captured_before = {str(p.relative_to(reviewer_before)):p.read_bytes()
                       for p in reviewer_before.rglob('*') if p.is_file()}
    assert before and json.loads(before)['source_origin'] == 'sealed_candidate'
    cached = directory/'unaccepted-findings.json'
    cached.unlink()
    preserved = {p:p.read_bytes() for p in directory.rglob('*') if p.is_file()
        and p not in (directory/'repo/Tablet/A.lean', directory/'repo/reference/context.txt',
                      directory/'repo/reference/paper.txt')}
    result = retire(collector, directory, observed, apply=True)
    assert result['removed'] == 2 and result['retained_files'] == 2
    assert all(p.read_bytes() == content for p,content in preserved.items())
    assert not (directory/'repo/Tablet/A.lean').exists()
    assert (directory/'repo/Tablet/Unique.lean').read_text() == 'Unique accepted proof'
    reviewer_after = collector.root.parent/'reviewer-after'
    assert handoffs.worker_handoffs(collector.root, reviewer_after, [task]) == handoff_before
    assert {str(p.relative_to(reviewer_after)):p.read_bytes()
            for p in reviewer_after.rglob('*') if p.is_file()} == captured_before
    assert handoffs._findings(directory, task) == before
    record = json.loads((directory/copies.RECEIPT).read_text())
    assert record['completed'] and record['admission']['candidate_sha256'] == state['dispatch']['publications']['1']['candidate_sha256']
    assert retire(collector, directory, observed, apply=True)['removed'] == 0


@pytest.mark.parametrize('change', ['rejected', 'stopping', 'pending_retry', 'child_retry', 'publication_missing', 'event', 'sha'])
def test_checkpoint_requires_accepted_publication_and_no_retry_custody(accepted, change):
    collector,directory,observed,state = accepted
    task=state['dispatch']['tasks']['1']
    if change=='rejected':task['status']['outcome']={'kind':'rejected','reason':'no'}
    elif change=='stopping':task['status']['status']='stopping'
    elif change=='pending_retry':task['pending_retry']={'seed':'must survive'}
    elif change=='child_retry':
        child=copy.deepcopy(task);child['id']=2;child['retry_of']=1
        state['dispatch']['tasks']['2']=child
    elif change=='publication_missing':state['dispatch']['publications'].clear()
    elif change=='event':state['dispatch']['publications']['1']['event_id']=99
    else:state['dispatch']['publications']['1']['candidate_sha256']='invalid'
    observed=checkpoint(collector,state)
    assert retire(collector,directory,observed,apply=True)['removed']==0
    assert (directory/'repo/Tablet/A.lean').exists()


@pytest.mark.parametrize('change', ['active', 'identity', 'pin', 'live', 'cleanup', 'publication'])
def test_existing_terminal_guards_remain(accepted,monkeypatch,change):
    collector,directory,observed,state=accepted
    if change=='active':observed={}
    elif change=='identity':observed['1']={'status':'different'}
    elif change=='pin':put(directory/'retain-builds.json',{})
    elif change=='live':monkeypatch.setattr('trellis.dispatch.process.alive',lambda _:True)
    elif change=='cleanup':(directory/'execution/cleanup.json').unlink()
    else:monkeypatch.setattr(collector,'_owner_pending',lambda _:True)
    if change in ('pin','live','cleanup'):
        with pytest.raises((ValueError,FileNotFoundError)):retire(collector,directory,observed,apply=True)
    else:assert retire(collector,directory,observed,apply=True)['removed']==0
    assert (directory/'repo/Tablet/A.lean').exists()


@pytest.mark.parametrize('change',['missing','corrupt_file','manifest','wrong_publication','alias','hardlink'])
def test_sealed_candidate_required_and_all_bytes_validated(accepted,change):
    collector,directory,observed,state=accepted
    path=directory/'candidate/files/Tablet/Unique.lean'
    if change=='missing':(directory/'candidate/candidate.json').unlink()
    elif change=='corrupt_file':path.write_text('Changed')
    elif change=='manifest':(directory/'candidate/candidate.json').write_text('{}')
    elif change=='wrong_publication':
        state['dispatch']['publications']['1']['candidate_sha256']='b'*64
        observed=checkpoint(collector,state)
    elif change=='alias':
        path.parent.rename(path.parent.with_name('saved'));path.parent.symlink_to('saved',target_is_directory=True)
    else:os.link(path,directory/'alias')
    if change=='missing':
        assert retire(collector,directory,observed,apply=True)['status']=='no_sealed_candidate'
    else:
        with pytest.raises((ValueError,KeyError,FileNotFoundError,candidate.CandidateError)):
            retire(collector,directory,observed,apply=True)
    assert (directory/'repo/Tablet/A.lean').exists()


@pytest.mark.parametrize('change',['seal_file','seal_parent','new_turn','pin','phase'])
def test_changes_while_queued_prevent_unlink(accepted,monkeypatch,change):
    collector,directory,observed,state=accepted
    @contextmanager
    def lock():
        if change=='seal_file':(directory/'candidate/files/Tablet/Unique.lean').write_text('changed')
        elif change=='seal_parent':
            p=directory/'candidate/files/Tablet';p.rename(p.with_name('saved'));p.symlink_to('saved',target_is_directory=True)
        elif change=='new_turn':
            put(directory/'turns/2/execution/process.json',{'pid':123})
            put(directory/'turns/2/execution/cleanup.json',{'pid':123})
            candidate.seal(directory/'repo',directory/'turns/2/candidate',task_id=1,
                base_manifest={},inputs={},response={})
        elif change=='pin':put(directory/'retain-builds.json',{})
        else:monkeypatch.setattr(collector,'checkpoint_current',lambda _:False)
        yield True
    monkeypatch.setattr(collector,'retirement_lock',lock)
    if change=='phase':assert retire(collector,directory,observed,apply=True)['status']=='checkpoint_changed'
    else:
        with pytest.raises(ValueError):retire(collector,directory,observed,apply=True)
    assert (directory/'repo/Tablet/A.lean').exists()


def test_partial_restart_revalidates_seal_and_continues_exact_receipt(accepted,monkeypatch):
    collector,directory,observed,_=accepted
    calls=[]
    @contextmanager
    def lock():
        calls.append(1)
        yield len(calls)==1
    monkeypatch.setattr(collector,'retirement_lock',lock)
    first=retire(collector,directory,observed,apply=True)
    assert first['removed']==1 and first['status']=='coordinator_busy'
    assert not (directory/'repo/Tablet/A.lean').exists()
    assert (directory/'repo/reference/context.txt').exists()
    sealed=directory/'candidate/files/Tablet/Unique.lean';old=sealed.read_bytes();sealed.write_text('corrupt')
    with pytest.raises(candidate.CandidateError):retire(collector,directory,observed,apply=True)
    sealed.write_bytes(old)
    @contextmanager
    def ready():yield True
    monkeypatch.setattr(collector,'retirement_lock',ready)
    assert retire(collector,directory,observed,apply=True)['removed']==1
    assert json.loads((directory/copies.RECEIPT).read_text())['completed']


def test_latest_handoff_seal_must_equal_published_seal(accepted):
    collector,directory,observed,state=accepted
    put(directory/'turns/2/execution/process.json',{'pid':123})
    put(directory/'turns/2/execution/cleanup.json',{'pid':123})
    sha=candidate.seal(directory/'repo',directory/'turns/2/candidate',task_id=1,
        base_manifest={},inputs={},response={})
    with pytest.raises(ValueError,match='publication'):retire(collector,directory,observed,apply=True)
    state['dispatch']['publications']['1']['candidate_sha256']=sha
    observed=checkpoint(collector,state)
    assert retire(collector,directory,observed,apply=True)['removed']==2


@pytest.mark.parametrize('change',['publication','retry','phase'])
def test_latest_native_observation_changes_while_waiting(accepted,monkeypatch,change):
    collector,directory,observed,state=accepted
    @contextmanager
    def lock():
        if change=='publication':state['dispatch']['publications']['1']['candidate_sha256']='c'*64
        elif change=='retry':state['dispatch']['tasks']['1']['pending_retry']={'needed':True}
        else:state['dispatch']['tasks']['1']['status']['status']='checking'
        put(collector.root.parent/'protocol_state.json',state)
        yield True
    monkeypatch.setattr(collector,'retirement_lock',lock)
    with pytest.raises(ValueError,match='native observation'):
        retire(collector,directory,observed,apply=True)
    assert (directory/'repo/Tablet/A.lean').exists()


def test_no_current_publication_observation_never_authorizes(accepted):
    collector,directory,observed,_=accepted
    assert copies.retire(collector,directory,observed,apply=True)['removed']==0
    assert (directory/'repo/Tablet/A.lean').exists()


def test_real_background_hook_and_counters(accepted):
    from trellis.dispatch.storage import BackgroundCollector
    collector,directory,observed,state=accepted
    # Model a real local clone with a valid checkpoint HEAD. Keep staged worker
    # modifications and all private metadata rather than relaxing Git guards.
    prepared=json.loads((directory/'prepared.json').read_text())
    git(directory/'repo','fetch','-q',str(collector.repo),prepared['source_head'])
    git(directory/'repo','update-ref','HEAD',prepared['source_head'])
    background=BackgroundCollector(collector.root,repo=collector.repo,git_maintenance_threshold_bytes=None)
    try:
        assert background.schedule(state)
        assert background.wait_idle(30)
        assert background.summary['errors']==0
        assert background.summary['accepted_source_files']==2
        assert not (directory/'repo/Tablet/A.lean').exists()
        assert (directory/'repo/Tablet/Unique.lean').exists()
        candidate.read(directory/'candidate')
    finally:
        background.close()


def test_final_compaction_rechecks_seal(accepted,monkeypatch):
    collector,directory,observed,_=accepted
    count=[]
    @contextmanager
    def lock():
        count.append(1)
        if len(count)==3:(directory/'candidate/files/Tablet/Unique.lean').write_text('changed at completion')
        yield True
    monkeypatch.setattr(collector,'retirement_lock',lock)
    with pytest.raises(ValueError,match='candidate changed'):
        retire(collector,directory,observed,apply=True)
    assert not json.loads((directory/copies.RECEIPT).read_text()).get('completed')
