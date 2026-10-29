import concurrent.futures
import json
import os
from pathlib import Path
import threading
import time

import pytest

from trellis.checker import admission
from trellis.checker.jobs import JobService, register_workspace
from trellis.checker.job_workspace import capture


def setup_project(tmp_path, execute=None):
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / "Tablet/N.lean").write_text("example : True := by trivial\n")
    runtime = tmp_path / "runtime"
    env = register_workspace(runtime, repo, scope="test")
    service = JobService(runtime, execute=execute)
    return repo, runtime, env["TRELLIS_CHECK_JOBS_TOKEN"], service


def request(token, request_id="request1", **kw):
    return dict(action="submit", token=token, request_id=request_id,
                kind="build", target="N", timeout=20, **kw)


def test_owner_bound_jobs_survive_clock_and_gc_but_not_revocation(tmp_path, monkeypatch):
    import hashlib
    from trellis.checker import workspaces
    from trellis.checker.jobs import revoke_workspace
    repo, runtime, finite, service = setup_project(tmp_path)
    token = register_workspace(runtime, repo, scope='acceptance', lifetime=None)['TRELLIS_CHECK_JOBS_TOKEN']
    job = service.handle(request(token))
    row = service.db.execute('SELECT request FROM jobs WHERE id=?', (job['job_id'],)).fetchone()
    spec = json.loads(row['request'])
    build = service.root / 'workspaces' / hashlib.sha256(str(repo).encode()).hexdigest() / 'repo/.lake/build'
    build.mkdir(parents=True); (build / 'retained.olean').write_bytes(b'cache')
    monkeypatch.setattr(workspaces.time, 'time', lambda: 1e18)
    assert service._live_spec(spec)
    assert service._capability(token)[1]['expires'] is None
    with pytest.raises(ValueError, match='expired'): service._capability(finite)
    service._finish(job['job_id'], 'succeeded', dict(returncode=0))
    with service.lock: service._collect_ended_attempts()
    service._collect_mirror_builds()
    assert build.exists()
    assert service.db.execute('SELECT id FROM jobs WHERE id=?', (job['job_id'],)).fetchone()
    with monkeypatch.context() as changed:
        changed.setattr(workspaces, '_process_identity', lambda pid: dict(pid=pid, start_ticks='reused'))
        assert not service._live_spec(spec)
        with pytest.raises(ValueError, match='expired'): service._capability(token)
    revoke_workspace(runtime, token)
    assert not service._live_spec(spec)
    assert service.release(token)['drained']
    assert not build.exists()


def wait_done(service, token, job):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        result = service.handle(dict(action="status", token=token, job_id=job["job_id"]))
        if result["state"] not in {"queued", "running"}:
            return result
        time.sleep(.01)
    raise AssertionError("job never completed")


def test_advisory_reservations_learn_across_targets_without_forgetting_target_peaks(tmp_path):
    from trellis.memory_guard import GIB
    scheduler = admission.Admission(6, reserve_bytes=20 * GIB)
    service = JobService(tmp_path / "runtime", scheduler=scheduler)
    spec = dict(kind="incremental", target="new-node")
    assert service._estimate(spec) == 20 * GIB
    for index in range(3):
        with scheduler.acquire(str(index), operation="advisory:incremental"):
            scheduler.observe_peak(5 * GIB, completed=True)
    assert service._estimate(spec) == 8.25 * GIB
    service.memory_peaks["incremental", "new-node"] = 15 * GIB
    assert service._estimate(spec) == 20.75 * GIB
    assert service._estimate(dict(kind="build", target="new-node")) == 20 * GIB


def test_parallel_submit_single_flight_and_reconnect(tmp_path):
    entered, release = threading.Event(), threading.Event()
    calls = []
    def execute(spec, job):
        calls.append(job["id"])
        entered.set()
        assert release.wait(5)
        return dict(returncode=1, stdout="Tablet/N.lean:1:8: error: example failure\n", stderr="")
    repo, runtime, token, service = setup_project(tmp_path, execute)
    service.worker.start()
    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            jobs = list(pool.map(lambda i: service.handle(request(token, f"id{i}")), range(12)))
        assert len({j["job_id"] for j in jobs}) == 1
        assert entered.wait(2)
        # No connected client is required for the running job.
        release.set()
        result = wait_done(service, token, jobs[0])
        assert result["state"] == "failed"
        assert result["result"]["stdout"] == "Tablet/N.lean:1:8: error: example failure\n"
        assert calls == [jobs[0]["job_id"]]
        assert service.handle(request(token, "again"))["job_id"] == jobs[0]["job_id"]
        (repo / "Tablet/N.lean").write_text("example : True := by constructor\n")
        assert service.handle(request(token, "edited"))["job_id"] != jobs[0]["job_id"]
    finally:
        release.set()
        service.shutdown()


def test_retry_id_conflict_and_capability_isolation(tmp_path):
    repo, runtime, token, service = setup_project(tmp_path)
    job = service.handle(request(token))
    bad = request(token)
    bad["kind"] = "incremental"
    with pytest.raises(ValueError, match="different check parameters"):
        service.handle(bad)
    other = register_workspace(runtime, repo, scope="other")["TRELLIS_CHECK_JOBS_TOKEN"]
    with pytest.raises(ValueError, match="job not found"):
        service.handle(dict(action="cancel", token=other, job_id=job["job_id"]))
    with pytest.raises(ValueError, match="capability"):
        service.handle(request("unknown-capability-token"))
    assert service.handle(dict(action="cancel", token=token, job_id=job["job_id"]))["state"] == "cancelled"
    assert service.handle(request(token))["state"] == "cancelled"


def test_restart_interrupted_is_not_reexecuted(tmp_path):
    repo, runtime, token, service = setup_project(tmp_path)
    job = service.handle(request(token))
    restarted = JobService(runtime)
    result = restarted.handle(request(token))
    assert result["job_id"] == job["job_id"]
    assert result["state"] == "interrupted"
    assert "restarted" in result["result"]["message"]
    assert restarted.handle(request(token, "new-check"))["job_id"] != job["job_id"]


def test_capture_rejects_symlink_escape_and_keeps_exact_bytes(tmp_path):
    repo, runtime, token, service = setup_project(tmp_path)
    digest, data = capture(repo)
    (repo / "Tablet/N.lean").write_text("changed")
    assert capture(repo)[0] != digest
    assert b"changed" not in data
    (repo / "Tablet/N.lean").unlink()
    (repo / "Tablet/N.lean").symlink_to("/etc/passwd")
    with pytest.raises(ValueError, match="regular file"):
        service.handle(request(token))


@pytest.mark.parametrize('root', ['.trellis/tmp/correspondence', '.trellis/stuck-math-audit'])
def test_reviewer_probe_is_captured_and_managed_with_absolute_or_relative_path(tmp_path, monkeypatch, root):
    from trellis import check_job
    captured = []
    def execute(spec, job):
        captured.append((spec, json.loads((service.root/'snapshots'/(job['snapshot']+'.json')).read_text())))
        return dict(returncode=0, stdout='reviewer-probe-result\n', stderr='')
    repo, runtime, token, service = setup_project(tmp_path, execute)
    probe = repo/root/'request-7/probe.lean'
    probe.parent.mkdir(parents=True)
    probe.write_text('#check True\n')
    for rel in ['.trellis/tmp/unrelated/private.lean', '.trellis/tmp/private.lean', '.trellis/private/context.lean']:
        p = repo/rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text('not a checker input\n')
    monkeypatch.chdir(repo)
    monkeypatch.setenv(check_job.SOCKET_ENV, str(service.path))
    monkeypatch.setenv(check_job.TOKEN_ENV, token)
    service.start()
    try:
        assert check_job.main(['--shim', 'lake', 'env', 'lean', str(probe)]) == 0
        assert check_job.main(['--shim', 'lean', str(probe.relative_to(repo))]) == 0
        assert len(captured) == 1, 'relative and absolute calls must reconnect to the same captured job'
        spec, snapshot = captured[0]
        assert spec['kind'] == 'scratch'
        assert spec['target'] == str(probe.relative_to(repo))
        assert spec['target'] in snapshot
        assert not any('private' in p for p in snapshot)
        probe.write_text('#check False\n')
        job = service.db.execute('SELECT id FROM jobs').fetchone()
        old = check_job.rpc(dict(action='status', job_id=job['id']))
        assert old['state'] == 'succeeded' and old['current_inputs_match'] is False
        with pytest.raises(check_job.JobError):
            check_job.shim('lean', [str(tmp_path/'outside.lean')])
    finally:
        service.shutdown()


@pytest.mark.parametrize('root', ['.trellis/tmp/correspondence', '.trellis/stuck-math-audit'])
def test_reviewer_probe_capture_rejects_symlink_files(tmp_path, root):
    repo, runtime, token, service = setup_project(tmp_path)
    probe = repo/root/'request-7/probe.lean'
    probe.parent.mkdir(parents=True)
    probe.symlink_to('/etc/passwd')
    with pytest.raises(ValueError, match='regular file'):
        capture(repo)


@pytest.mark.parametrize('root', ['.trellis/tmp/correspondence', '.trellis/stuck-math-audit'])
def test_reviewer_probe_stays_readonly_in_compiler_sandbox(tmp_path, monkeypatch, root):
    import subprocess
    from trellis.checker.job_workspace import prepare_mirror, compiler_command
    repo, runtime, token, service = setup_project(tmp_path)
    probe = repo/root/'request-7/probe.lean'
    probe.parent.mkdir(parents=True)
    probe.write_text('#check True\n')
    monkeypatch.setattr('trellis.supervisor_workspace._ensure_supervisor_lake_packages', lambda *a: None)
    mirror = tmp_path/'compiler/repo'
    prepare_mirror(repo, mirror, capture(repo)[1])
    command = compiler_command(mirror, ['/bin/sh', '-c', 'cat "$1"; ! printf changed >> "$1"', 'probe', str(probe.relative_to(repo))])
    result = subprocess.run(command, cwd=mirror, capture_output=True, text=True, timeout=15)
    assert result.returncode == 0, result.stderr
    assert result.stdout == '#check True\n'
    assert 'Read-only file system' in result.stderr
    assert (mirror/probe.relative_to(repo)).read_text() == '#check True\n'


def test_admission_evicts_resident_before_other_work():
    scheduler = admission.Admission()
    events = []
    with scheduler.acquire("worker"):
        scheduler.retain("worker", lambda: events.append("exited"))
    with scheduler.acquire("worker"):
        assert events == []
    with scheduler.acquire("authoritative"):
        assert events == ["exited"]
    assert scheduler.evict is None


def test_dependency_receipt_survives_eviction_but_not_input_changes(tmp_path, monkeypatch):
    """Exercise the real capture/mirror path without launching a compiler."""
    from types import SimpleNamespace
    from trellis.checker import jobs
    repo, runtime, token, service = setup_project(tmp_path)
    probe = repo / '.trellis/scratch/Probe.lean'
    probe.parent.mkdir(parents=True)
    probe.write_text('import Tablet.N\n#check True\n')
    session = jobs.WorkspaceSession()
    commands = []
    operations = []
    returncode = [0]

    class Process:
        def __init__(self, *args, **kwargs):
            self.returncode = returncode[0]
        def poll(self):
            return self.returncode
        def wait(self, **kwargs):
            return self.returncode

    class Broker:
        def __init__(self, *args, **kwargs):
            self._open_docs = {}
            self._srv = SimpleNamespace(proc=Process())
        def handle_file(self, path):
            self._open_docs[path.as_uri()] = 1
            return dict(verdict='ok')
        def _teardown_server(self):
            self._open_docs.clear()

    monkeypatch.setattr(jobs.ic, '_Broker', Broker)
    monkeypatch.setattr(jobs.subprocess, 'Popen', Process)
    monkeypatch.setattr(jobs, 'compiler_command', lambda repo, args, **kw: commands.append(args) or args)
    monkeypatch.setattr(service, '_remember', lambda proc: None)
    monkeypatch.setattr(service, '_check_stop', lambda *a, **kw: None)
    monkeypatch.setattr(service.scheduler, 'retain', lambda *a: None)
    monkeypatch.setattr('trellis.supervisor_workspace._ensure_supervisor_lake_packages', lambda *a: None)

    def check():
        identity, snapshot = capture(repo)
        directory = service.root / 'snapshots'
        directory.mkdir(exist_ok=True)
        (directory / (identity + '.json')).write_bytes(snapshot)
        spec = dict(repo=str(repo), kind='scratch', target=str(probe.relative_to(repo)))
        job = dict(owner='test', snapshot=identity)
        operation, captured = service._admission_inputs(spec, job, session)
        operations.append(operation)
        return service._execute(spec, job, float('inf'), session, captured=captured)

    try:
        assert check()['returncode'] == 0
        assert len(commands) == 1
        assert operations[-1] == 'advisory:scratch'
        service._evict(session)
        probe.write_text('import Tablet.N\n#check False\n')
        reused = check()
        assert reused['phases']['dependencies_reused'] is True
        assert operations[-1] == 'advisory:scratch:dependencies_reused'
        assert reused['phases']['warm_session'] is False
        assert len(commands) == 1

        # All captured non-target bytes matter, even with preserved timestamps.
        source = repo / 'Tablet/N.lean'
        stamp = source.stat()
        source.write_text('example : True := by constructor\n')
        os.utime(source, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        assert check()['phases']['dependencies_reused'] is False
        assert len(commands) == 2
        (repo / 'lakefile.toml').write_text('name = "changed"\n')
        assert check()['phases']['dependencies_reused'] is False
        assert len(commands) == 3
        (repo / 'lean-toolchain').write_text('leanprover/lean4:v4.29.0\n')
        assert check()['phases']['dependencies_reused'] is False
        assert len(commands) == 4
        probe.write_text('import Tablet.N\nimport Tablet.Other\n#check True\n')
        returncode[0] = 1
        assert check()['returncode'] == 1
        assert session.materialized_dependencies is None
        returncode[0] = 0
        assert check()['phases']['dependencies_reused'] is False
        assert len(commands) == 6, 'a failed dependency build cannot issue a reuse receipt'
        assert operations == ['advisory:scratch', 'advisory:scratch:dependencies_reused',
                              *(['advisory:scratch'] * 5)]
    finally:
        service.shutdown()


def test_terminal_snapshot_retention_bounds_edits_and_keeps_poll_and_dedup(tmp_path):
    repo, runtime, token, service = setup_project(tmp_path)
    source = repo / 'Tablet/N.lean'
    first = None
    for epoch in range(30):
        source.write_text(f'-- epoch {epoch}\nexample : True := by trivial\n')
        job = service.handle(request(token, f'edit{epoch}'))
        if first is None:
            first = job
        state = 'failed' if epoch % 2 == 0 else 'succeeded'
        service._finish(job['job_id'], state, dict(returncode=int(state == 'failed'), stdout=f'epoch {epoch}'))
    with service.lock:
        service._collect_ended_attempts()
    snapshots = service.root / 'snapshots'
    assert len(list(snapshots.glob('*.json'))) == 2
    assert not (snapshots / (first['input_sha256'] + '.json')).exists()
    polled = service.handle(dict(action='status', token=token, job_id=first['job_id']))
    assert polled['result']['stdout'] == 'epoch 0'
    assert polled['current_inputs_match'] is False
    source.write_text('-- epoch 0\nexample : True := by trivial\n')
    dedup = service.handle(request(token, 'again'))
    assert dedup['job_id'] == first['job_id']
    assert dedup['current_inputs_match'] is True
    assert not (snapshots / (first['input_sha256'] + '.json')).exists()
    assert service.db.execute('SELECT count(*) FROM jobs').fetchone()[0] == 30


def test_snapshot_collection_protects_pending_claimed_and_exceptional_inputs(tmp_path):
    repo, runtime, token, service = setup_project(tmp_path)
    protected = []
    states = ['queued', 'running', 'cancelled', 'infrastructure_error', 'timed_out',
              'resource_exhausted', 'interrupted', 'failed', 'succeeded']
    for epoch, state in enumerate(states):
        (repo / 'Tablet/N.lean').write_text(f'-- epoch {epoch}\n')
        job = service.handle(request(token, f'edit{epoch}'))
        protected.append(job['input_sha256'])
        if state == 'running':
            service.db.execute("UPDATE jobs SET state='running' WHERE id=?", (job['job_id'],))
            service.db.commit()
        elif state != 'queued':
            service._finish(job['job_id'], state, dict(returncode=1))
        if state == 'cancelled':
            service.claimed[job['job_id']] = (str(repo), 'interactive', 'owner')
    with service.lock:
        service._collect_ended_attempts()
    assert all((service.root / 'snapshots' / (identity + '.json')).exists() for identity in protected)


def test_mirror_build_cleanup_requires_reaping_and_all_shared_owners_ended(tmp_path):
    import hashlib
    from trellis.checker.jobs import revoke_workspace, WorkspaceSession
    repo, runtime, token, service = setup_project(tmp_path)
    other = register_workspace(runtime, repo, scope='other')['TRELLIS_CHECK_JOBS_TOKEN']
    key = hashlib.sha256(str(repo).encode()).hexdigest()
    mirror = service.root / 'workspaces' / key / 'repo'
    build = mirror / '.lake/build'
    build.mkdir(parents=True)
    (build / 'dependency.olean').write_bytes(b'cached')
    (mirror / 'preserved-source.lean').write_text('source evidence')
    assert service.release(token)['drained']
    assert build.exists(), 'another live capability protects this workspace even before it submits'
    revoke_workspace(runtime, other)
    with service.scheduler.acquire('still-running'):
        service._collect_mirror_builds()
        assert build.exists()
    service.scheduler.cleanup_failed = True
    service._collect_mirror_builds()
    assert build.exists()
    service.scheduler.cleanup_failed = False
    session = service.sessions[str(repo)] = WorkspaceSession()
    session.broker = object()
    service._collect_mirror_builds()
    assert build.exists(), 'a resident process must be reaped first'
    session.broker = None
    session.materialized_dependencies = ('old receipt',)
    assert service.release(other)['drained']
    assert not build.exists()
    assert session.materialized_dependencies is None
    assert (mirror / 'preserved-source.lean').read_text() == 'source evidence'


def test_mirror_cleanup_keeps_cancelled_claim_and_rejects_symlink_escape(tmp_path):
    import hashlib
    from trellis.checker.jobs import revoke_workspace
    repo, runtime, token, service = setup_project(tmp_path)
    job = service.handle(request(token))
    key = hashlib.sha256(str(repo).encode()).hexdigest()
    mirror = service.root / 'workspaces' / key / 'repo'
    build = mirror / '.lake/build'
    build.mkdir(parents=True)
    revoke_workspace(runtime, token)
    service._collect_mirror_builds()
    assert build.exists(), 'a queued job still needs its workspace'
    service._finish(job['job_id'], 'cancelled', dict(message='cancelled'))
    service.claimed[job['job_id']] = (str(repo), 'interactive', 'owner')
    service._collect_mirror_builds()
    assert build.exists(), 'terminal status is not a drained claim'
    service.claimed.clear()
    build.rmdir()
    outside = tmp_path / 'outside'
    outside.mkdir()
    (outside / 'preserve').write_text('evidence')
    build.symlink_to(outside, target_is_directory=True)
    service._collect_mirror_builds()
    assert (outside / 'preserve').read_text() == 'evidence'
    assert build.is_symlink()


def test_partial_mirror_cleanup_revokes_dependency_receipt_before_failure(tmp_path, monkeypatch):
    import hashlib
    from trellis.checker import jobs
    repo, runtime, token, service = setup_project(tmp_path)
    jobs.revoke_workspace(runtime, token)
    key = hashlib.sha256(str(repo).encode()).hexdigest()
    build = service.root / 'workspaces' / key / 'repo/.lake/build'
    build.mkdir(parents=True)
    artifact = build / 'dependency.olean'
    artifact.write_bytes(b'compiled')
    session = service.sessions[str(repo)] = jobs.WorkspaceSession()
    session.materialized_dependencies = ('previous dependency receipt',)

    def partial_remove(path):
        assert path == build
        assert session.materialized_dependencies is None
        artifact.unlink()
        raise OSError('simulated failure after partial removal')

    monkeypatch.setattr(jobs.shutil, 'rmtree', partial_remove)
    with pytest.raises(OSError, match='partial removal'):
        service._collect_mirror_builds()
    assert build.exists() and not artifact.exists()
    assert session.materialized_dependencies is None


@pytest.mark.parametrize("kind,target", [
    ("build", "../evil"), ("scratch", "/etc/passwd.lean"),
    ("scratch", ".trellis/scratch/../../evil.lean"),
    ("scratch", ".trellis/tmp/other/probe.lean"),
    ("scratch", ".trellis/tmp/correspondence-old/probe.lean"),
    ("scratch", ".trellis/stuck-math-audit/../private/probe.lean"),
    ("scratch", ".trellis/private/probe.lean"),
    ("file", ".trellis/tmp/correspondence/probe.lean"),
])
def test_rejects_path_and_command_injection(tmp_path, kind, target):
    repo, runtime, token, service = setup_project(tmp_path)
    req = request(token)
    req.update(kind=kind, target=target)
    with pytest.raises(ValueError):
        service.handle(req)


def test_cancel_queued_job_waiting_for_admission_does_not_execute(tmp_path):
    calls = []
    repo, runtime, token, service = setup_project(tmp_path, lambda spec, job: calls.append(job) or {'returncode': 0})
    with service.scheduler.acquire('authoritative'):
        service.worker.start()
        job = service.handle(request(token))
        time.sleep(.05)  # Let the worker select the queued job, then block.
        service.handle(dict(action='cancel', token=token, job_id=job['job_id']))
    try:
        assert wait_done(service, token, job)['state'] == 'cancelled'
        time.sleep(.05)
        assert calls == []
    finally:
        service.shutdown()


def test_socket_disconnect_after_submit_does_not_stop_job(tmp_path, monkeypatch):
    from trellis import check_job
    calls = []
    repo, runtime, token, service = setup_project(tmp_path, lambda spec, job: calls.append(job['id']) or dict(returncode=0, stdout='done\n', stderr=''))
    monkeypatch.setenv(check_job.SOCKET_ENV, str(service.path))
    monkeypatch.setenv(check_job.TOKEN_ENV, token)
    service.start()
    try:
        job = check_job.rpc({k: v for k, v in request(token).items() if k != 'token'})
        result = check_job.wait(job)
        assert result['state'] == 'succeeded'
        assert calls == [job['job_id']]
        assert check_job.rpc(dict(action='status', job_id=job['job_id']))['result']['stdout'] == 'done\n'
    finally:
        service.shutdown()


def test_configured_service_outage_never_falls_back_to_local_build(monkeypatch, capsys):
    from trellis import check_job
    monkeypatch.setenv(check_job.SOCKET_ENV, '/does-not-exist/jobs.sock')
    monkeypatch.setenv(check_job.TOKEN_ENV, 'test')
    monkeypatch.setattr('subprocess.run', lambda *a, **k: pytest.fail('unmanaged compiler launched'))
    assert check_job.main(['build', 'N']) == 2
    assert 'central checker unavailable' in capsys.readouterr().err


def test_stale_process_identity_cannot_authorize_kill(monkeypatch):
    closed = []
    monkeypatch.setattr(os, 'pidfd_open', lambda pid: 123)
    monkeypatch.setattr(os, 'close', lambda fd: closed.append(fd))
    monkeypatch.setattr('trellis.checker.jobs.ic._process_identity', lambda pid: {'pid': pid, 'start_ticks': 'new'})
    monkeypatch.setattr('signal.pidfd_send_signal', lambda *a: pytest.fail('signalled reused PID'))
    JobService._stop_identity({'pid': 9999, 'start_ticks': 'old'})
    assert closed == [123]


def test_changed_inputs_are_reported_without_relabeling_old_diagnostics(tmp_path, capsys):
    from trellis import check_job
    repo, runtime, token, service = setup_project(tmp_path, lambda spec, job: dict(returncode=0, stdout='captured result\n', stderr=''))
    service.worker.start()
    try:
        job = service.handle(request(token))
        done = wait_done(service, token, job)
        assert done['current_inputs_match'] is True
        (repo / 'Tablet/N.lean').write_text('example : False := by sorry\n')
        old = service.handle(dict(action='status', token=token, job_id=job['job_id']))
        assert old['state'] == 'succeeded' and old['current_inputs_match'] is False
        assert check_job.render(old) == 2
        output = capsys.readouterr()
        assert output.out == 'captured result\n'
        assert 'workspace changed after submission' in output.err
    finally:
        service.shutdown()


def test_arbitrary_command_fields_are_rejected(tmp_path):
    repo, runtime, token, service = setup_project(tmp_path)
    with pytest.raises(ValueError, match='unsupported check fields: argv'):
        service.handle(request(token, argv=['/bin/sh', '-c', 'anything']))


def test_memory_exhaustion_is_not_a_heartbeat_verdict(monkeypatch):
    import sys
    monkeypatch.setattr(admission, 'ACTIVE', admission.Admission())
    monkeypatch.setattr('trellis.memory_guard.MemoryGuard.check', lambda self, pid: 'host reserve reached')
    with pytest.raises(OSError, match='checker resource exhaustion: host reserve reached'):
        admission.guarded_run([sys.executable, '-c', 'import time; time.sleep(10)'], timeout=15, capture_output=True, text=True)


def test_authoritative_waiter_precedes_another_advisory_job():
    scheduler = admission.Admission()
    order = []
    def call(owner):
        with scheduler.acquire(owner):
            order.append(owner)
    with scheduler.acquire('running-advisory'):
        background = threading.Thread(target=call, args=('next-advisory',))
        trusted = threading.Thread(target=call, args=('authoritative',))
        background.start(); trusted.start()
        deadline = time.monotonic() + 2
        while not scheduler.authoritative_waiters and time.monotonic() < deadline:
            time.sleep(.005)
        assert scheduler.authoritative_waiters == 1
    trusted.join(2); background.join(2)
    assert order == ['authoritative', 'next-advisory']


def test_reused_dependency_history_is_separate_and_does_not_lower_seeds(tmp_path):
    from trellis.memory_guard import GIB
    from trellis.checker.jobs import state_dir
    runtime = tmp_path / 'runtime'
    directory = state_dir(runtime)
    directory.mkdir(parents=True)
    operation = 'advisory:scratch:dependencies_reused'
    target = '.trellis/scratch/Probe.lean'
    # Genuine old host log format has no cohort. It remains a conservative
    # cold/default bound, without poisoning the separately measured warm job.
    rows = [dict(kind='scratch', node=target, peak_tree_rss_bytes=20 * GIB),
            dict(kind='scratch', node=target, peak_tree_rss_bytes=4 * GIB,
                 memory_operation=operation)]
    (directory / 'checks.jsonl').write_text(''.join(json.dumps(r) + '\n' for r in rows))
    scheduler = admission.Admission(2, reserve_bytes=24 * GIB)
    service = JobService(runtime, scheduler=scheduler)
    spec = dict(kind='scratch', target=target)
    assert service._estimate(spec) == 27 * GIB
    assert service._estimate(spec, operation) == 24 * GIB
    for _ in range(3):
        with scheduler.acquire('job', operation=operation):
            scheduler.observe_peak(4 * GIB, completed=True)
    assert service._estimate(spec, operation) == 7 * GIB
    assert service._estimate(spec) == 27 * GIB
    assert scheduler.estimate('kernel_replay') == 24 * GIB
    service.shutdown()


def test_queued_reuse_class_is_rechecked_without_lease_upgrade(tmp_path, monkeypatch):
    from contextlib import contextmanager
    from trellis.checker.jobs import WorkspaceSession
    from trellis.checker.job_workspace import prepare_mirror
    repo, runtime, token, service = setup_project(tmp_path)
    probe = repo / '.trellis/scratch/Probe.lean'
    probe.parent.mkdir(parents=True)
    probe.write_text('import Tablet.N\n#check True\n')
    identity, snapshot = capture(repo)
    (service.root / 'snapshots').mkdir()
    (service.root / 'snapshots' / (identity + '.json')).write_bytes(snapshot)
    spec = dict(repo=str(repo), kind='scratch', target=str(probe.relative_to(repo)))
    job = dict(owner='test', snapshot=identity)
    session = WorkspaceSession()
    mirror = service._captured_inputs(spec, job)[0]
    monkeypatch.setattr('trellis.supervisor_workspace._ensure_supervisor_lake_packages', lambda *a: None)
    prepare_mirror(repo, mirror, snapshot)
    session.materialized_dependencies = service._captured_inputs(spec, job)[-1]
    assert service._admission_inputs(spec, job, session)[0].endswith(':dependencies_reused')
    monkeypatch.setattr(service, '_check_stop', lambda *a: None)
    scheduler = service.scheduler
    original = scheduler.acquire
    admitted = []
    @contextmanager
    def tracked(*args, **kwargs):
        with original(*args, **kwargs) as value:
            if args[0] == 'test':
                admitted.append(kwargs['operation'])
                assert scheduler.active[threading.get_ident()]['depth'] == 1
            yield value
    monkeypatch.setattr(scheduler, 'acquire', tracked)
    queued = threading.Event()
    def work():
        with service._admitted(spec, job, session, lambda _: queued.set()) as (operation, captured):
            assert operation == 'advisory:scratch'
            assert 'Tablet/N.lean' in captured[-2]
    with concurrent.futures.ThreadPoolExecutor(1) as pool:
        with scheduler.acquire('blocking'):
            pending = pool.submit(work)
            assert queued.wait(2)
            old = (mirror / 'Tablet/N.lean').stat()
            (mirror / 'Tablet/N.lean').write_text('example : True := by constructor\n')
            os.utime(mirror / 'Tablet/N.lean', ns=(old.st_atime_ns, old.st_mtime_ns))
        pending.result(timeout=3)
    assert admitted == ['advisory:scratch:dependencies_reused', 'advisory:scratch']
    assert not scheduler.active and not scheduler.waiters
    service.shutdown()


def test_completed_diagnostic_memory_samples_learn_and_age_without_lowering_cold_budget(tmp_path, monkeypatch):
    from trellis.memory_guard import GIB
    operation = 'advisory:scratch:dependencies_reused'
    peaks = iter([16 * GIB, *([4 * GIB] * 32)])
    def execute(spec, job):
        peak = next(peaks)
        service.scheduler.observe_peak(peak)
        return dict(returncode=1, stdout='ordinary Lean diagnostic\n', stderr='',
                    backend='incremental', peak_tree_rss_bytes=peak, _diagnostics_complete=True)
    repo, runtime, token, service = setup_project(tmp_path, execute)
    service.scheduler.reserve_bytes = 24 * GIB
    probe = repo / '.trellis/scratch/Probe.lean'
    probe.parent.mkdir(parents=True)
    target = str(probe.relative_to(repo))
    # Cohort classification has separate capture/receipt tests; exercise real
    # worker completion, output validation, history and lease cleanup here.
    monkeypatch.setattr(service, '_admission_inputs', lambda *a: (operation, None))
    service.worker.start()
    try:
        for index in range(33):
            probe.write_text(f'-- diagnostic version {index}\n')
            req = request(token, str(index)); req.update(kind='scratch', target=target)
            result = wait_done(service, token, service.handle(req))
            assert result['state'] == 'failed'
            assert result['result']['memory_sample_complete'] is True
            assert '_diagnostics_complete' not in result['result']
            with service.scheduler.condition:
                assert service.scheduler.condition.wait_for(lambda: not service.scheduler.active, timeout=2)
            assert not service.scheduler.incomplete_peaks
            if index == 2:
                assert service.scheduler.estimate(operation) == 22 * GIB
        assert service.scheduler.memory_samples[operation] == [4 * GIB] * 32
        assert service.scheduler.estimate(operation) == 7 * GIB
        # The separate target maximum continues to protect a known large job.
        assert service._estimate(dict(kind='scratch', target=target), operation) == 22 * GIB
        assert service._estimate(dict(kind='scratch', target='.trellis/scratch/Other.lean'), operation) == 7 * GIB
        assert service.scheduler.estimate('advisory:scratch') == 24 * GIB
        assert service.scheduler.estimate('kernel_replay') == 24 * GIB
        rows = [json.loads(line) for line in (service.root / 'checks.jsonl').read_text().splitlines()]
        assert all(row['memory_sample_complete'] is True for row in rows)
    finally:
        service.shutdown()


@pytest.mark.parametrize('outcome', ['resource', 'cancel', 'timeout', 'spawn', 'signal',
                                     'oversize', 'oversize_success', 'fallback', 'unclassified_exit1'])
def test_incomplete_or_rejected_jobs_keep_peak_lower_bound(tmp_path, monkeypatch, outcome):
    from trellis.checker.jobs import JobStopped
    from trellis.memory_guard import GIB
    operation = 'advisory:scratch:dependencies_reused'
    def execute(spec, job):
        service.scheduler.observe_peak(16 * GIB)
        if outcome in {'resource', 'cancel', 'timeout'}:
            state = dict(resource='resource_exhausted', cancel='cancelled', timeout='timed_out')[outcome]
            raise JobStopped(state, 'injected interrupted job')
        if outcome == 'spawn':
            raise OSError('compiler spawn failed')
        result = dict(returncode=1, stdout='', stderr='', backend='incremental',
                      peak_tree_rss_bytes=16 * GIB, _diagnostics_complete=True)
        if outcome == 'signal':
            result['returncode'] = -9
        elif outcome.startswith('oversize'):
            result['stdout'] = 'x' * (4 * 1024 * 1024 + 1)
            if outcome == 'oversize_success': result['returncode'] = 0
        elif outcome == 'fallback':
            result['backend'] = 'lake'
        elif outcome == 'unclassified_exit1':
            result.pop('_diagnostics_complete')
        return result
    repo, runtime, token, service = setup_project(tmp_path, execute)
    service.scheduler.reserve_bytes = 24 * GIB
    monkeypatch.setattr(service, '_admission_inputs', lambda *a: (operation, None))
    service.worker.start()
    try:
        result = wait_done(service, token, service.handle(request(token)))
        assert result['state'] != 'succeeded'
        with service.scheduler.condition:
            assert service.scheduler.condition.wait_for(lambda: not service.scheduler.active, timeout=2)
        assert not service.scheduler.memory_samples
        assert service.scheduler.incomplete_peaks[operation] == 16 * GIB
        assert service.scheduler.estimate(operation) == 24 * GIB
        assert not result['result'].get('memory_sample_complete')
    finally:
        service.shutdown()


@pytest.mark.parametrize('verdict', ['ok', 'fail', 'fallback', 'resource'])
def test_only_guarded_terminal_incremental_diagnostics_issue_completion_flag(tmp_path, monkeypatch, verdict):
    from types import SimpleNamespace
    from trellis.checker import jobs
    repo, runtime, token, service = setup_project(tmp_path)
    probe = repo / '.trellis/scratch/Probe.lean'
    probe.parent.mkdir(parents=True)
    probe.write_text('#check True\n')
    class Process:
        returncode = 1
        def __init__(self, *a, **kw): pass
        def poll(self): return self.returncode
        def wait(self, **kw): return self.returncode
    class Broker:
        def __init__(self, *a, **kw):
            self._open_docs = {}
            self._srv = SimpleNamespace(proc=Process())
        def handle_file(self, path):
            return dict(verdict='fail' if verdict == 'resource' else verdict,
                        reason='ambiguous or crashed diagnostics')
        def _teardown_server(self): pass
    def guard(*args, **kwargs):
        if verdict == 'resource' and len(args) == 5:
            raise jobs.JobStopped('resource_exhausted', 'final guard interrupted observation')
    monkeypatch.setattr(jobs.ic, '_Broker', Broker)
    monkeypatch.setattr(jobs.subprocess, 'Popen', Process)
    monkeypatch.setattr(jobs, 'compiler_command', lambda repo, args, **kw: args)
    monkeypatch.setattr(service, '_remember', lambda proc: None)
    monkeypatch.setattr(service, '_check_stop', guard)
    monkeypatch.setattr(service.scheduler, 'retain', lambda *a: None)
    monkeypatch.setattr('trellis.supervisor_workspace._ensure_supervisor_lake_packages', lambda *a: None)
    identity, snapshot = capture(repo)
    (service.root / 'snapshots').mkdir(exist_ok=True)
    (service.root / 'snapshots' / (identity + '.json')).write_bytes(snapshot)
    spec = dict(repo=str(repo), kind='scratch', target=str(probe.relative_to(repo)))
    try:
        if verdict == 'resource':
            with pytest.raises(jobs.JobStopped):
                service._execute(spec, dict(owner='test', snapshot=identity), float('inf'), jobs.WorkspaceSession())
        else:
            result = service._execute(spec, dict(owner='test', snapshot=identity), float('inf'), jobs.WorkspaceSession())
            assert result.get('_diagnostics_complete', False) is (verdict in {'ok', 'fail'})
            assert result['backend'] == ('lake' if verdict == 'fallback' else 'incremental')
    finally:
        service.shutdown()
