from contextlib import contextmanager
import json
import os
import subprocess
import sys

import pytest

from test_dispatch_terminal_retention import setup, put
from test_dispatch_nonworker_retention import owner_setup
from test_terminal_provider_sessions import session, SID
from trellis import codex_timing, codex_rollouts
from trellis.dispatch import provider_session_retention as sessions, retained_files, terminal_retention
from trellis.dispatch.storage import BackgroundCollector
from trellis.codex_rollouts import rollout_usage_fallback


def test_nonworker_actual_timing_usage_restore_cli_and_unique_evidence(owner_setup):
    collector, directory, observed, _, state = owner_setup
    home, path = session(directory)
    raw, info = path.read_bytes(), path.stat()
    before = {p:p.read_bytes() for p in directory.rglob('*') if p.is_file() and p != path}
    timing = codex_timing.compute_timing_for_session(SID, sessions_root=home/'.codex/sessions')
    usage = rollout_usage_fallback(home, SID)
    assert sessions.collect(collector, directory, observed)['archived'] == 1
    assert not path.exists() and retained_files.read_bytes(path) == raw
    assert codex_rollouts.discover(home/'.codex/sessions') == [path]
    assert codex_timing.compute_timing_for_session(SID, sessions_root=home/'.codex/sessions') == timing
    assert rollout_usage_fallback(home, SID) == usage
    assert all(p.read_bytes() == value for p,value in before.items())
    env = dict(os.environ, TRELLIS_CODEX_SESSIONS_ROOT=str(home/'.codex/sessions'))
    subprocess.run([sys.executable, '-B', '-m', 'trellis.codex_timing', 'show', SID], env=env,
                   check=True, capture_output=True)
    # Existing operator custody survives restore; native CLI receives exact raw bytes.
    pin = put(directory/'retain-builds.json', {'reason':'offline inspection'})
    saved_pin = pin.read_bytes()
    result = subprocess.run([sys.executable, '-B', '-m', 'trellis.dispatch.retained_files',
        '--runtime', str(collector.root.parent), '--owner', directory.name, '--restore'],
        check=True, capture_output=True)
    assert json.loads(result.stdout)['owner'] == directory.name
    assert path.read_bytes() == raw and path.stat().st_mtime_ns == info.st_mtime_ns
    assert path.stat().st_mode == info.st_mode and pin.read_bytes() == saved_pin


@pytest.mark.parametrize('change', ['active', 'uncommitted', 'pin', 'live', 'unreaped', 'wrong_request'])
def test_nonworker_native_checkpoint_process_and_review_guards(owner_setup, monkeypatch, change):
    collector, directory, observed, _, state = owner_setup
    _, path = session(directory); raw = path.read_bytes()
    if change in ('active', 'uncommitted'):
        if directory.name.startswith('review-'):
            if change == 'active': state['dispatch']['reviewer'] = {'request_id':2}
            else: state['dispatch']['receipts']['2']['response_sha256'] = 'b'*64
        else:
            if change == 'active': state['dispatch']['verifiers']['jobs']['3']['status'] = {'status':'checking'}
            else: state['dispatch']['verifiers']['jobs']['3']['identity']['panel_sha256'] = 'different'
        observed = terminal_retention.terminal_owners(state, defer_verifiers=True)
    elif change == 'pin':
        put(collector.root/'reviews/2/retain-state.json' if directory.name.startswith('review-')
            else directory/'retain-builds.json', {})
    elif change in ('live', 'unreaped'):
        put(directory/'execution/process.json', {'pid':123})
        if change == 'live': put(directory/'execution/cleanup.json', {'pid':123})
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: change == 'live')
    else:
        p = directory/'prepared.json'; value=json.loads(p.read_bytes()); value['request_id']=999; put(p,value)
        if directory.name.startswith('verifier-'):
            # A regenerated equivalent verifier request may carry a different
            # request ID; the committed verifier job identity remains exact.
            assert sessions.collect(collector,directory,observed)['archived']==1
            assert not path.exists() and retained_files.read_bytes(path)==raw
            return
    try: result = sessions.collect(collector, directory, observed)
    except (ValueError, FileNotFoundError): pass
    else: assert result['archived'] == 0
    assert path.read_bytes() == raw


@pytest.mark.parametrize('change', ['reader', 'checkpoint', 'pin', 'prepared'])
def test_nonworker_queued_archive_rechecks_custody(owner_setup, monkeypatch, change):
    collector, directory, observed, _, _ = owner_setup
    _, path = session(directory); raw=path.read_bytes(); original=collector.retirement_lock
    held=False
    @contextmanager
    def lease():
        nonlocal held
        with original() as acquired:
            held=True
            if change=='checkpoint': monkeypatch.setattr(collector,'checkpoint_current',lambda _:False)
            elif change=='pin': put(directory/'retain-builds.json',{})
            elif change=='prepared': put(directory/'prepared.json',{'source_repo':'changed'})
            try: yield acquired
            finally: held=False
    monkeypatch.setattr(collector,'retirement_lock',lease)
    def reader():
        if change=='reader' and held: raise RuntimeError('reader unavailable')
    with pytest.raises((ValueError,RuntimeError)):
        sessions.collect(collector,directory,observed,runtime_admission=reader)
    assert path.read_bytes()==raw


@pytest.mark.parametrize('enabled', [False,True])
def test_background_nonworker_scope_uses_existing_optin_and_gate(owner_setup, enabled):
    collector,directory,_,_,state=owner_setup
    _,path=session(directory); calls=[]
    background=BackgroundCollector(collector.root, repo=collector.repo, git_maintenance_threshold_bytes=None,
        archive_provider_sessions=enabled, provider_session_admission=lambda:calls.append(True))
    try:
        assert background.schedule(state) and background.wait_idle(30)
        assert background.summary['errors']==0
        assert path.exists() is not enabled
        assert background.summary['provider_session_archives']==int(enabled)
        assert bool(calls)==enabled
    finally: background.close()


def test_owner_restore_refuses_active_current_state(owner_setup):
    collector,directory,observed,_,state=owner_setup
    _,path=session(directory)
    assert sessions.collect(collector,directory,observed)['archived']==1
    if directory.name.startswith('review-'):state['dispatch']['reviewer']={'request_id':2}
    else:state['dispatch']['verifiers']['jobs']['3']['status']={'status':'checking'}
    put(collector.root.parent/'protocol_state.json',state)
    result=subprocess.run([sys.executable,'-B','-m','trellis.dispatch.retained_files','--runtime',
        str(collector.root.parent),'--owner',directory.name,'--restore'],capture_output=True)
    assert result.returncode != 0 and not path.exists()


@pytest.mark.parametrize('failure', ['live', 'unreaped'])
def test_owner_restore_cli_refuses_process_custody(owner_setup, failure):
    from trellis.dispatch.process import identity
    collector,directory,observed,_,_=owner_setup
    _,path=session(directory)
    assert sessions.collect(collector,directory,observed)['archived']==1
    if failure=='live':
        process=identity(os.getpid())
    else:
        child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)'])
        try:process=identity(child.pid)
        finally:child.terminate();child.wait(timeout=10)
    put(directory/'execution/process.json',process)
    if failure=='live':put(directory/'execution/cleanup.json',process)
    # Exact live identity or dead identity lacking its cleanup receipt both
    # retain the archive without writing a restore pin.
    result=subprocess.run([sys.executable,'-B','-m','trellis.dispatch.retained_files','--runtime',
        str(collector.root.parent),'--owner',directory.name,'--restore'],capture_output=True)
    assert result.returncode != 0 and not path.exists()
    assert not (directory/'retain-builds.json').exists()


@pytest.mark.parametrize('failure', ['traversal', 'alias', 'exclusive'])
def test_owner_restore_cli_rejects_unsupported_paths(owner_setup,failure):
    collector,directory,observed,_,_=owner_setup
    _,path=session(directory)
    assert sessions.collect(collector,directory,observed)['archived']==1
    if failure=='traversal':owner='../dispatch/'+directory.name
    elif failure=='alias':
        alias=collector.root/'review-999';alias.symlink_to(directory,target_is_directory=True);owner=alias.name
    else:owner='exclusive-1'
    result=subprocess.run([sys.executable,'-B','-m','trellis.dispatch.retained_files','--runtime',
        str(collector.root.parent),'--owner',owner,'--restore'],capture_output=True)
    assert result.returncode != 0 and not path.exists()
