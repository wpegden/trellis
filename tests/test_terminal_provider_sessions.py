from contextlib import contextmanager
import json
import os
from pathlib import Path
import subprocess
import sys

import pytest
from test_dispatch_terminal_retention import setup, put, git
from test_codex_timing import _rollout_synthetic
from trellis import codex_rollouts as rollouts, codex_timing as timing
from trellis.dispatch import provider_session_retention as sessions, retained_files as retained
from trellis.codex_rollouts import rollout_usage_fallback

SID='01a16a80-9aa0-77d1-9f6f-38d0bf5b2b81'


@pytest.fixture(autouse=True)
def prepared_source_head(setup):
    collector,directory,_,_,_=setup
    path=directory/'prepared.json'
    prepared=json.loads(path.read_bytes())
    prepared['source_head']=git(collector.repo,'rev-parse','HEAD')
    put(path,prepared)



def session(directory, *, sid=SID, day='23', mtime=1234567890000000000):
    home=directory/'runtime/burst-homes/worker-task-1'
    path=home/f'.codex/sessions/2026/10/{day}/rollout-2026-10-{day}T16-42-55-{sid}.jsonl'
    path.parent.mkdir(parents=True,exist_ok=True)
    text=_rollout_synthetic()+json.dumps({'type':'padding','payload':{'text':'unchanged retained history '*60000}})+'\n'
    text+=json.dumps({'payload':{'info':{'usage':{'input_tokens':900,'output_tokens':42}}}})+'\n'
    path.write_text(text);path.chmod(0o600);os.utime(path,ns=(mtime,mtime))
    return home,path


def test_archive_preserves_timing_usage_warm_discovery_and_restore_metadata(setup):
    collector,directory,observed,_,_=setup
    home,path=session(directory);root=home/'.codex/sessions';raw=path.read_bytes();before=path.stat()
    found=timing.find_rollout_path(SID,sessions_root=root)
    assert found==path
    timed=timing.compute_timing_for_session(SID,sessions_root=root)
    usage=rollout_usage_fallback(home,SID)
    assert sessions.collect(collector,directory,observed)['archived']==1
    assert not path.exists()
    assert timing.find_rollout_path(SID,sessions_root=root)==path
    assert rollouts.discover(root)==[path]
    assert timing.compute_timing_for_session(SID,sessions_root=root)==timed
    assert rollout_usage_fallback(home,SID)==usage
    assert rollouts.original_mtime_ns(path)==before.st_mtime_ns
    assert retained.restore(path)
    assert path.read_bytes()==raw and path.stat().st_mtime_ns==before.st_mtime_ns
    assert path.stat().st_mode&0o777==0o600
    assert rollouts.discover(root)==[path]


def test_archive_only_discovery_and_latest_usage_original_mtime(setup):
    collector,directory,observed,_,_=setup
    home,older=session(directory,sid='11111111-1111-1111-1111-111111111111',mtime=10)
    _,newer=session(directory,mtime=20)
    newer.write_text(newer.read_text().replace('"input_tokens": 900','"input_tokens": 901'));os.utime(newer,ns=(20,20))
    assert sessions.collect(collector,directory,observed)['archived']==2
    timing._ROLLOUT_INDEX_CACHE.clear()
    assert timing.find_rollout_path(SID,sessions_root=home/'.codex/sessions')==newer
    # Archive filesystem times deliberately contradict original session order.
    os.utime(retained._paths(older)[0],ns=(1000,1000))
    assert rollout_usage_fallback(home,None)['input_tokens']==901


@pytest.mark.parametrize('failure',['orphan','corrupt','missing_mtime'])
def test_incomplete_or_legacy_archive_never_silently_selects_older_usage(setup,failure):
    collector,directory,observed,_,_=setup
    home,older=session(directory,sid='11111111-1111-1111-1111-111111111111',mtime=10)
    _,newer=session(directory,mtime=20)
    assert sessions.collect(collector,directory,observed)['archived']==2
    archive,metadata=retained._paths(newer)
    if failure=='orphan':metadata.unlink()
    elif failure=='corrupt':archive.write_bytes(b'corrupt')
    else:
        record=json.loads(metadata.read_bytes());record.pop('mtime_ns');put(metadata,record)
        # Explicit-session timing does not need to invent an ordering.
        assert timing.compute_timing_for_session(SID,sessions_root=home/'.codex/sessions')
    with pytest.raises((ValueError,FileNotFoundError)):
        rollout_usage_fallback(home,None)


@pytest.mark.parametrize('failure',['active','uncommitted','pending','pin','live','unreaped','tracked','hardlink','alias','project','exclusive'])
def test_owner_process_source_and_independent_file_guards(setup,monkeypatch,failure):
    collector,directory,observed,_,_=setup
    home,path=session(directory);raw=path.read_bytes()
    if failure=='active':observed={}
    elif failure=='uncommitted':observed={'1':'different'}
    elif failure=='pending':monkeypatch.setattr(collector,'_owner_pending',lambda _:True)
    elif failure=='pin':put(directory/'retain-builds.json',{})
    elif failure in ('live','unreaped'):
        put(directory/'execution/process.json',{'pid':12})
        if failure=='live':put(directory/'execution/cleanup.json',{'pid':12})
        monkeypatch.setattr('trellis.dispatch.process.alive',lambda _:failure=='live')
    elif failure=='tracked':monkeypatch.setattr(retained,'_untracked',lambda _:False)
    elif failure=='hardlink':path.with_name('shared.jsonl').hardlink_to(path)
    elif failure=='alias':
        original=path.with_name('raw');path.rename(original);path.symlink_to(original)
    elif failure=='project':put(directory/'prepared.json',{'source_repo':str(directory)})
    elif failure=='exclusive':
        old=directory;directory=directory.with_name('exclusive-1');old.rename(directory);path=directory/path.relative_to(old)
        observed={'exclusive-1':observed['1']}
    try:result=sessions.collect(collector,directory,observed)
    except (ValueError,FileNotFoundError):pass
    else:assert result['archived']==0
    assert path.read_bytes()==raw


def test_exact_dated_layout_excludes_unknown_files_and_alias_parents(setup):
    collector,directory,observed,_,_=setup
    home,path=session(directory)
    excluded=[home/'.codex/sessions/root.jsonl',home/'.codex/sessions/2026/10/23/credentials.json',
              home/'.codex/archived_sessions/rollout-private.jsonl',home/'.codex/sessions/2026/10/99/rollout-invalid.jsonl']
    for p in excluded:p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(path.read_bytes())
    assert sessions.collect(collector,directory,observed)['archived']==1
    assert all(p.exists() for p in excluded)
    alias=home/'.codex/sessions/2025';alias.symlink_to(home/'.codex/sessions/2026',target_is_directory=True)
    with pytest.raises(ValueError,match='symlink'):list(sessions.paths(directory))


def test_gate_rechecked_after_offlock_encoding_and_busy_restart(setup,monkeypatch):
    collector,directory,observed,_,_=setup
    home,path=session(directory);raw=path.read_bytes();held=False;checks=[];original=collector.retirement_lock
    @contextmanager
    def lock():
        nonlocal held
        with original() as acquired:
            held=True
            try:yield acquired
            finally:held=False
    monkeypatch.setattr(collector,'retirement_lock',lock)
    def admission():
        checks.append(held)
        if held:raise RuntimeError('viewer reader changed')
    with pytest.raises(RuntimeError,match='viewer reader'):
        sessions.collect(collector,directory,observed,runtime_admission=admission)
    assert checks==[False,True] and path.read_bytes()==raw
    assert sessions.collect(collector,directory,observed,runtime_admission=lambda:None)['archived']==1
    assert retained.read_bytes(path)==raw


def test_active_native_resume_rejects_archive_with_explicit_restore_instructions(setup):
    from trellis.agent_wrapper.continuation import record_session, require_session
    collector,directory,observed,_,_=setup
    home,path=session(directory)
    binding=dict(provider='codex',home=str(home),thread_id=SID,model='test',reasoning_effort='test')
    receipt=directory/'session.json';record_session(receipt,binding)
    require_session(receipt,binding)
    sessions.collect(collector,directory,observed)
    with pytest.raises(RuntimeError,match='--restore'):require_session(receipt,binding)
    retained.restore(path);require_session(receipt,binding)


def test_restore_cli_recreates_exact_native_paths_and_preserves_existing_pin(setup):
    collector,directory,observed,_,state=setup
    home,path=session(directory);raw=path.read_bytes();mtime=path.stat().st_mtime_ns
    sessions.collect(collector,directory,observed)
    put(collector.root.parent/'protocol_state.json',state)
    pin=put(directory/'retain-builds.json',{'reason':'existing operator custody'});original=pin.read_bytes()
    subprocess.run([sys.executable,'-B','-m','trellis.dispatch.retained_files','--runtime',str(collector.root.parent),
                    '--task','1','--restore'],check=True,capture_output=True)
    assert path.read_bytes()==raw and path.stat().st_mtime_ns==mtime and pin.read_bytes()==original


@pytest.mark.parametrize('enabled',[False,True])
def test_background_scope_default_off_then_explicit_reader_gate(setup,monkeypatch,enabled):
    from trellis.dispatch.storage import BackgroundCollector
    collector,directory,observed,_,state=setup
    home,path=session(directory)
    monkeypatch.setattr('trellis.dispatch.auxiliary_retention.retire',lambda *a,**k:{'removed':0})
    monkeypatch.setattr('trellis.dispatch.worker_source_retention.retire',lambda *a,**k:{'removed':0})
    calls=[]
    background=BackgroundCollector(collector.root,repo=collector.repo,git_maintenance_threshold_bytes=None,
        archive_provider_sessions=enabled,provider_session_admission=lambda:calls.append(True))
    try:
        assert background.schedule(state) and background.wait_idle(30)
        assert background.summary['errors']==0
        assert path.exists() is not enabled
        assert background.summary['provider_session_archives']==int(enabled)
        assert bool(calls)==enabled
    finally:background.close()


def test_old_manifest_restore_and_raw_hardlink_reader_compatibility(tmp_path):
    home,path=session(tmp_path)
    original=path.read_bytes()
    link=path.with_name('rollout-linked.jsonl');link.hardlink_to(path)
    assert rollouts.read_text(path).encode()==original
    link.unlink()
    assert retained.archive(path) # Old manifest without the optional timestamp.
    assert rollouts.read_text(path).encode()==original
    assert retained.restore(path) and path.read_bytes()==original


def test_equal_mtime_usage_has_same_deterministic_tie_before_after_archive(setup):
    collector,directory,observed,_,_=setup
    home,first=session(directory,sid='00000000-0000-0000-0000-000000000000',mtime=20)
    _,second=session(directory,mtime=20)
    second.write_text(second.read_text().replace('"input_tokens": 900','"input_tokens": 902'))
    os.utime(second,ns=(20,20))
    before=rollout_usage_fallback(home,None)
    assert before['input_tokens']==902
    sessions.collect(collector,directory,observed)
    assert rollout_usage_fallback(home,None)==before


def test_viewer_style_timing_subprocess_reads_archived_session(setup):
    collector,directory,observed,_,_=setup
    home,path=session(directory)
    env=dict(os.environ,TRELLIS_CODEX_SESSIONS_ROOT=str(home/'.codex/sessions'),PYTHONDONTWRITEBYTECODE='1')
    command=[sys.executable,'-B','-m','trellis.codex_timing','show',SID]
    before=subprocess.run(command,check=True,capture_output=True,env=env).stdout
    sessions.collect(collector,directory,observed)
    assert subprocess.run(command,check=True,capture_output=True,env=env).stdout==before


@pytest.mark.parametrize('change',['cancel','prepared','checkpoint','pin'])
def test_queued_final_fence_retains_raw_when_owner_guards_change(setup,monkeypatch,change):
    collector,directory,observed,_,_=setup
    home,path=session(directory);raw=path.read_bytes();original=collector.retirement_lock
    @contextmanager
    def changed_lease():
        if change=='cancel':monkeypatch.setattr(collector,'cancelled',lambda:True)
        elif change=='prepared':put(directory/'prepared.json',{'source_repo':str(collector.repo),'changed':True})
        elif change=='checkpoint':monkeypatch.setattr(collector,'checkpoint_current',lambda _:False)
        elif change=='pin':put(directory/'retain-builds.json',{})
        with original() as acquired:yield acquired
    monkeypatch.setattr(collector,'retirement_lock',changed_lease)
    try: result=sessions.collect(collector,directory,observed)
    except ValueError:pass
    else:assert result['archived']==0
    assert path.read_bytes()==raw
