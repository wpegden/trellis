import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor

import pytest

from trellis.dispatch import resources
from trellis.dispatch.process import AttemptProcess
from trellis.checker import admission, workspaces
from trellis.checker.server import CheckerServer
from trellis.check_job import rpc
from trellis.atomic_actions.checker_client import CheckerRpcError, client_release_workspace


def runtime_at(path):
    repo = path.parent / 'repo'
    (repo/'Tablet').mkdir(parents=True)
    (repo/'.trellis').mkdir()
    (repo/'Tablet/N.lean').write_text('example : True := by trivial\n')
    path.mkdir()
    (path/'runtime_metadata.json').write_text(json.dumps(dict(repo_path=str(repo))))
    return path


@pytest.mark.parametrize('compiler', ['managed', 'none'])
@pytest.mark.parametrize('lifetime', [3600, None])
def test_adopted_launcher_owns_channels_and_reaps_real_service_jobs(monkeypatch, compiler, lifetime):
    monkeypatch.setenv('TRELLIS_CHECK_JOBS', '1')
    monkeypatch.setenv('TRELLIS_CHECK_JOBS_RESERVE_GIB', '0')
    monkeypatch.setattr(admission, 'ACTIVE', None)
    # Short root for the authoritative server's Unix socket.
    with tempfile.TemporaryDirectory(prefix='dispatch-drain-', dir='/tmp') as directory:
        runtime = runtime_at(Path(directory)/'runtime')
        private = runtime_at(runtime/'dispatch/1/runtime')
        process = AttemptProcess(private.parent/'attempt')
        server = CheckerServer(runtime)
        server.set_expected_peer_uid(os.geteuid())
        server.start()
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        record = None
        entered = threading.Event()
        try:
            def execute(spec, job):
                entered.set()
                while True:
                    server._job_service._check_stop(job, spec, float('inf'))
                    time.sleep(.01)
            server._job_service.execute_override = execute
            record = process.start_or_adopt([sys.executable, '-c', 'import time; time.sleep(60)'], {},
                checker=dict(runtime=str(runtime), task_runtime=str(private), compiler=compiler,
                             scope='worker1', lifetime=lifetime))
            process.permit(record)
            deadline = time.monotonic()+5
            path = process.root/'compiler-channels.json'
            while not path.exists() and time.monotonic() < deadline:
                time.sleep(.01)
            channels = json.loads(path.read_text())['environment']
            registry = workspaces.registry_path(runtime)
            entry = json.loads(next(registry.glob('*.json')).read_text())
            assert entry['owner']['pid'] == record['pid'] != os.getpid()
            assert (entry['expires'] is None) == (lifetime is None)
            if lifetime is None:
                monkeypatch.setattr(workspaces.time, 'time', lambda: 1e18)
            assert not path.stat().st_mode & 0o077
            if compiler == 'managed':
                result = rpc(dict(action='submit', request_id='one', kind='build', target='N', timeout=30),
                    socket_path=channels['TRELLIS_CHECK_JOBS_SOCKET'], token=channels['TRELLIS_CHECK_JOBS_TOKEN'])
                assert entered.wait(2)
            else:
                assert 'TRELLIS_CHECK_JOBS_TOKEN' not in channels
            # An ambient channel from another worker must not retarget drain.
            monkeypatch.setenv('TRELLIS_CHECKER_TOKEN', 'wrong ambient capability')
            monkeypatch.setenv('TRELLIS_CHECK_JOBS_TOKEN', 'wrong ambient capability')
            monkeypatch.setenv('TRELLIS_CLOSURE_IDENTITY_ONLY', '1')
            adopted = AttemptProcess(process.root)
            recipe = json.loads((process.root / 'recipe.json').read_text())
            assert adopted.start_or_adopt(recipe['command'], recipe['payload'], checker=recipe['checker']) == record
            adopted.stop_and_reap(record)
            process.child.wait(timeout=5)
            assert not list(registry.glob('*.json'))
            assert not server._job_service.scheduler.active
            if compiler == 'managed':
                row = server._job_service.db.execute('SELECT state FROM jobs WHERE id=?', (result['job_id'],)).fetchone()
                assert row['state'] == 'cancelled'
        finally:
            if record:
                process.stop_and_reap(record)
            server.shutdown()
            thread.join(timeout=5)


def test_missing_cleanup_acknowledgement_keeps_channel_evidence(tmp_path):
    runtime = runtime_at(tmp_path/'runtime')
    private = runtime_at(runtime/'dispatch/1/runtime')
    root = private.parent/'attempt'
    root.mkdir()
    resources.grant(root, dict(runtime=str(runtime), task_runtime=str(private), compiler='none',
                               scope='worker1', lifetime=10))
    with pytest.raises(RuntimeError, match='retain the worker slot'):
        resources.drain(root, timeout=0, authority=lambda *_: {'drained': False})
    assert (root/'compiler-channels.json').exists()
    assert not list(workspaces.registry_path(runtime).glob('*.json'))


def test_release_timeout_retries_ongoing_cleanup_without_releasing_other_workspace(monkeypatch):
    monkeypatch.setenv('TRELLIS_CHECK_JOBS', '0')
    monkeypatch.setattr(admission, 'ACTIVE', None)
    with tempfile.TemporaryDirectory(prefix='drain-retry-', dir='/tmp') as directory:
        runtime = runtime_at(Path(directory)/'runtime')
        private = runtime_at(runtime/'dispatch/1/runtime')
        other = runtime_at(runtime/'dispatch/2/runtime')
        root = private.parent/'attempt'
        root.mkdir()
        env = resources.grant(root, dict(runtime=str(runtime), task_runtime=str(private), compiler='none',
                                         scope='worker1', lifetime=60))
        other_env = workspaces.register(runtime, other)
        result = root/'result.json'
        result.write_text('{"returncode":0,"status":"Stuck"}')
        evidence = (root/'compiler-channels.json').read_bytes(), result.read_bytes()
        entered, finish_cleanup, first_timeout, retry_entered = (threading.Event() for _ in range(4))
        closed, calls = [], []

        class Handler:
            def __init__(self, path): self.path = path
            def shutdown(self, **kwargs):
                if self.path == private:
                    assert kwargs == {'require_reaped': True}
                    entered.set()
                    assert finish_cleanup.wait(5)
                closed.append(self.path)

        server = CheckerServer(runtime)
        server._task_workspaces.factory = Handler
        server.set_expected_peer_uid(os.geteuid())
        server.start()
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()

        def authority(socket_path, token, *, deadline):
            calls.append((socket_path, token, deadline))
            if len(calls) == 1:
                try:
                    return client_release_workspace(socket_path, token, deadline=min(deadline, time.monotonic()+.15))
                except CheckerRpcError as exc:
                    assert exc.kind == 'supervisor_unavailable'
                    assert 'timed out waiting for response' in exc.message
                    first_timeout.set()
                    raise
            retry_entered.set()
            return client_release_workspace(socket_path, token, deadline=deadline)

        try:
            router = server._task_workspaces
            with router.acquire(env['TRELLIS_CHECKER_TOKEN']): pass
            with router.acquire(other_env['TRELLIS_CHECKER_TOKEN']), ThreadPoolExecutor(1) as pool:
                future = pool.submit(resources.drain, root, timeout=3, authority=authority)
                try:
                    assert entered.wait(2)
                    assert first_timeout.wait(2)
                    assert retry_entered.wait(2)
                    assert not future.done(), 'a timed-out cleanup must not release the slot'
                    assert private in router.contexts
                    assert not closed
                finally:
                    finish_cleanup.set()
                future.result(timeout=3)
                assert closed == [private]
                assert other in router.contexts
                router.check_live(other_env['TRELLIS_CHECKER_TOKEN'])
            assert len(calls) == 2 and calls[0] == calls[1], 'retry the exact capability and deadline'
            assert evidence == ((root/'compiler-channels.json').read_bytes(), result.read_bytes())
        finally:
            finish_cleanup.set()
            server.shutdown()
            thread.join(timeout=5)


def test_drain_retries_each_transport_but_requires_explicit_acknowledgements(monkeypatch):
    monkeypatch.setattr(resources.time, 'sleep', lambda _: None)
    env = dict(TRELLIS_CHECKER_SOCKET='authority-socket', TRELLIS_CHECKER_TOKEN='authority-token',
               TRELLIS_CHECK_JOBS_SOCKET='jobs-socket', TRELLIS_CHECK_JOBS_TOKEN='jobs-token')
    authority_calls, advisory_calls = [], []
    def authority(socket_path, token, **kwargs):
        authority_calls.append((socket_path, token, kwargs))
        if len(authority_calls) == 1:
            raise CheckerRpcError('supervisor_unavailable', 'response timed out')
        return {'drained': True}
    def advisory(request, **kwargs):
        advisory_calls.append((request, kwargs))
        if len(advisory_calls) == 1:
            raise resources.check_job.JobTransportError('response timed out')
        return {'drained': 1 if len(advisory_calls) == 2 else True}
    resources.drain_channels(env, authority=authority, advisory=advisory)
    assert len(authority_calls) == 2 and authority_calls[0] == authority_calls[1]
    assert len(advisory_calls) == 3
    assert all(call[1]['token'] == 'jobs-token' for call in advisory_calls)
    assert 0 < advisory_calls[-1][1]['timeout'] <= advisory_calls[0][1]['timeout'] <= 30


@pytest.mark.parametrize('error', [CheckerRpcError('internal_error', 'cleanup incomplete'),
                                  CheckerRpcError('auth_required', 'wrong token'),
                                  CheckerRpcError('malformed_response', 'wrong request id'),
                                  resources.check_job.JobError('cleanup incomplete')])
def test_drain_does_not_retry_protocol_or_cleanup_errors(error):
    calls = []
    def authority(*args, **kwargs):
        calls.append(args)
        raise error
    with pytest.raises(type(error)) as raised:
        resources.drain_channels(dict(TRELLIS_CHECKER_SOCKET='s', TRELLIS_CHECKER_TOKEN='t'), authority=authority)
    assert raised.value is error
    assert len(calls) == 1


def test_drain_exhaustion_keeps_evidence_and_retries_with_same_deadline(tmp_path, monkeypatch):
    runtime = runtime_at(tmp_path/'runtime')
    private = runtime_at(runtime/'dispatch/1/runtime')
    root = private.parent/'attempt'
    root.mkdir()
    resources.grant(root, dict(runtime=str(runtime), task_runtime=str(private), compiler='none',
                              scope='worker1', lifetime=60))
    clock = [100.0]
    monkeypatch.setattr(resources.time, 'monotonic', lambda: clock[0])
    monkeypatch.setattr(resources.time, 'sleep', lambda seconds: clock.__setitem__(0, clock[0]+seconds))
    calls = []
    def authority(*args, deadline):
        calls.append((args, deadline))
        clock[0] += min(10, deadline-clock[0])
        raise CheckerRpcError('supervisor_unavailable', 'response timed out')
    with pytest.raises(RuntimeError, match='retain the worker slot') as raised:
        resources.drain(root, authority=authority)
    assert isinstance(raised.value.__cause__, CheckerRpcError)
    assert len(calls) == 3 and calls[0] == calls[1] == calls[2]
    assert clock[0] == 130
    assert (root/'compiler-channels.json').exists()
    assert not list(workspaces.registry_path(runtime).glob('*.json'))


def test_cleanup_failure_reports_pending_exact_channels_without_secrets(tmp_path, monkeypatch):
    env = dict(TRELLIS_CHECKER_SOCKET='authority-socket', TRELLIS_CHECKER_TOKEN='workspace:authority-secret',
               TRELLIS_CHECK_JOBS_SOCKET='jobs-socket', TRELLIS_CHECK_JOBS_TOKEN='job-secret')
    monkeypatch.setattr(resources, '_revoke', lambda *_: None)
    # A deterministic clock exhausts the one shared deadline on the advisory call.
    clock = iter([0, 0, 0, 1, 1, 1])
    monkeypatch.setattr(resources.time, 'monotonic', lambda: next(clock))
    monkeypatch.setattr(resources.time, 'sleep', lambda *_: None)
    root=tmp_path/'execution';root.mkdir()
    (root/'compiler-channels.json').write_text(json.dumps({'environment':env}))
    def advisory(*args, **kwargs):
        raise resources.check_job.JobTransportError('timed out')
    with pytest.raises(resources.CleanupNotAcknowledged) as failure:
        resources.drain(root,timeout=.5,authority=lambda *_a,**_k: {'drained': True},advisory=advisory)
    message=str(failure.value)
    evidence=(root/'compiler-cleanup-failure.json').read_text()
    assert 'execution=' in message and str(root) in message
    assert 'advisory:' in message and 'authority:' not in message
    assert 'transport failure: timed out' in message
    assert all(secret not in message+evidence for secret in [env['TRELLIS_CHECKER_TOKEN'],env['TRELLIS_CHECK_JOBS_TOKEN']])
    rows=json.loads(evidence)['pending']
    assert len(rows)==1 and rows[0]['channel']=='advisory' and rows[0]['attempts']==1
    assert rows[0]['capability_sha256']==resources.hashlib.sha256(env['TRELLIS_CHECK_JOBS_TOKEN'].encode()).hexdigest()
    assert (root/'compiler-channels.json').exists()
