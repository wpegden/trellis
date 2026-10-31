"""Managed compiler startup retains owned-child identity and cleanup."""
import json
from pathlib import Path
import subprocess
import sys
import time

import pytest

from trellis.checker import jobs
from trellis.checker.job_workspace import capture


@pytest.fixture
def service(tmp_path):
    instance = jobs.JobService(tmp_path / 'runtime')
    yield instance
    instance.db.close()


def test_transient_setuid_denial_retains_complete_identity(service, monkeypatch):
    process = subprocess.Popen(['/bin/sleep', '30'])
    original = jobs.ic._process_identity
    calls = []
    identity = None

    def identity_after_exec(pid):
        calls.append(pid)
        if len(calls) <= 2:
            raise PermissionError('setuid exec transition')
        return original(pid)

    monkeypatch.setattr(jobs.ic, '_process_identity', identity_after_exec)
    try:
        identity = service._remember(process)
        assert calls == [process.pid] * 3
        assert identity == original(process.pid)
        assert set(identity) == {'pid', 'start_ticks', 'boot_id', 'pid_namespace'}
        recorded = service.db.execute('SELECT identity FROM processes').fetchall()
        assert [json.loads(row[0]) for row in recorded] == [identity]
    finally:
        process.kill()
        process.wait(timeout=3)
        if identity is not None:
            service._forget(identity)


@pytest.mark.parametrize('already_reaped', [False, True])
def test_fast_exit_never_records_or_rereads_reusable_pid(service, monkeypatch, already_reaped):
    process = subprocess.Popen(['/bin/sleep', '30'])
    calls = []

    def exit_during_capture(pid):
        calls.append(pid)
        assert not already_reaped, 'must not inspect a PID after Popen reaped it'
        process.kill()
        process.wait(timeout=3)
        raise PermissionError('child exited during exec')

    monkeypatch.setattr(jobs.ic, '_process_identity', exit_during_capture)
    try:
        if already_reaped:
            process.kill()
            process.wait(timeout=3)
        assert service._remember(process) is None
        assert calls == ([] if already_reaped else [process.pid])
        assert service.db.execute('SELECT count(*) FROM processes').fetchone()[0] == 0
    finally:
        if process.poll() is None:
            process.kill()
        process.wait(timeout=3)


@pytest.mark.parametrize('kind', ['build', 'incremental'])
def test_identity_failure_is_bounded_and_reaps_every_launched_child(service, tmp_path, monkeypatch, kind):
    source = tmp_path / 'repo'
    (source / 'Tablet').mkdir(parents=True)
    (source / 'Tablet/N.lean').write_text('example : True := by trivial\n')
    digest, content = capture(source)
    snapshots = service.root / 'snapshots'
    snapshots.mkdir()
    (snapshots / (digest + '.json')).write_bytes(content)
    processes = []
    original_popen = subprocess.Popen

    def popen(*args, **kwargs):
        process = original_popen(*args, **kwargs)
        processes.append(process)
        return process

    def prepare_mirror(source, destination, snapshot):
        (destination / 'Tablet').mkdir(parents=True, exist_ok=True)
        (destination / 'Tablet/N.lean').write_bytes((source / 'Tablet/N.lean').read_bytes())

    def deny_identity(pid):
        raise PermissionError('persistent namespace denial')

    monkeypatch.setattr(jobs, 'prepare_mirror', prepare_mirror)
    monkeypatch.setattr(jobs, 'compiler_command', lambda *a, **kw:
        [sys.executable, '-c', 'import time; time.sleep(30)'])
    monkeypatch.setattr(jobs.subprocess, 'Popen', popen)
    monkeypatch.setattr(jobs.ic, '_process_identity', deny_identity)
    monkeypatch.setattr(service, '_check_stop', lambda *a, **kw: None)
    start = time.monotonic()
    try:
        with pytest.raises(PermissionError, match='persistent namespace denial'):
            service._execute(dict(repo=str(source), kind=kind, target='N'),
                dict(owner='test', snapshot=digest), float('inf'), jobs.WorkspaceSession())
        assert time.monotonic() - start < 3
        assert processes
        # Incremental may safely try the existing build fallback; neither its
        # failed server constructor nor that fallback may leak a compiler.
        assert all(process.returncode is not None for process in processes)
        assert all(not Path(f'/proc/{process.pid}').exists() for process in processes)
        assert service.db.execute('SELECT count(*) FROM processes').fetchone()[0] == 0
    finally:
        for process in processes:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=3)
