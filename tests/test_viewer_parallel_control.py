import importlib.util
from pathlib import Path
import json
import os
import tempfile
import threading

import pytest

spec = importlib.util.spec_from_file_location('parallel_control', Path(__file__).parents[1] / 'viewer/parallel_control.py')
control = importlib.util.module_from_spec(spec)
spec.loader.exec_module(control)


def launch_environment(runtime, **env):
    (runtime / 'launch_env.json').write_text(json.dumps({'runtime_root': str(runtime), 'env': {
        'TRELLIS_TRELLIS_KERNEL_CMD': '/deployed/kernel',
        'TRELLIS_RUNTIME_CHECKPOINT_HOOK': '/deployed/hook', **env}}))


def test_limit_control_records_only_native_admission_event(tmp_path, monkeypatch):
    value = dict(expected_worker_slots=8, expected_heavy_worker_slots=None, worker_slots=2, heavy_worker_slots=1)
    launch_environment(tmp_path)
    calls = []
    class Ledger:
        def __init__(self, runtime):
            assert runtime == tmp_path
        def observe(self, event):
            assert control.os.environ['TRELLIS_TRELLIS_KERNEL_CMD'] == '/deployed/kernel'
            assert control.os.environ['TRELLIS_CHECKER_TOKEN']
            calls.append(event)
            return {'config': {'worker_slots': 2, 'heavy_worker_slots': 1}}
    monkeypatch.setattr(control, 'NativeLedger', Ledger)
    monkeypatch.setattr(control.os, 'environ', {})
    assert control.change_limits(tmp_path, value) == {'worker_slots': 2, 'heavy_worker_slots': 1}
    assert calls == [dict(kind='agent_capacity_changed', **value)]
    assert control.os.environ == {}
    assert not (tmp_path / 'protocol_state.json').exists()


def test_missing_saved_hook_uses_run_deployment_not_viewer(tmp_path, monkeypatch):
    deployment = tmp_path / 'deployed'
    hook = deployment / 'scripts/trellis_git_checkpoint_hook.sh'
    hook.parent.mkdir(parents=True)
    hook.write_text('#!/bin/sh\n')
    launch_environment(tmp_path)
    launch = json.loads((tmp_path / 'launch_env.json').read_text())
    launch['env'].pop('TRELLIS_RUNTIME_CHECKPOINT_HOOK')
    launch['trellis_root'] = str(deployment)
    (tmp_path / 'launch_env.json').write_text(json.dumps(launch))
    class Ledger:
        def __init__(self, runtime): pass
        def observe(self, event):
            assert os.environ['TRELLIS_RUNTIME_CHECKPOINT_HOOK'] == str(hook)
            return {'config': {'worker_slots': 8, 'heavy_worker_slots': 3}}
    monkeypatch.setattr(control, 'NativeLedger', Ledger)
    monkeypatch.setenv('TRELLIS_RUNTIME_CHECKPOINT_HOOK', '/newer-viewer/hook')
    control.change_limits(tmp_path, dict(expected_worker_slots=8, expected_heavy_worker_slots=6,
                                        worker_slots=8, heavy_worker_slots=3))
    assert os.environ['TRELLIS_RUNTIME_CHECKPOINT_HOOK'] == '/newer-viewer/hook'


@pytest.mark.parametrize('foreign', ['launch', 'socket', 'registry'])
def test_limit_control_rejects_foreign_runtime_without_mutation(tmp_path, monkeypatch, foreign):
    launch_environment(tmp_path)
    path = tmp_path / 'launch_env.json'
    launch = json.loads(path.read_text())
    if foreign == 'launch': launch['runtime_root'] = str(tmp_path / 'other')
    if foreign == 'socket': launch['env']['TRELLIS_CHECKER_SOCKET'] = str(tmp_path / 'other/checker.sock')
    path.write_text(json.dumps(launch))
    registry = tmp_path / 'checker-state/burst-tokens.json'
    registry.parent.mkdir()
    registry.write_text(json.dumps({'runtime_root': str(tmp_path / 'other' if foreign == 'registry' else tmp_path),
                                   'tokens': ['supervisor'], 'entries': []}))
    before = registry.read_bytes()
    monkeypatch.setattr(control, 'NativeLedger', lambda _: pytest.fail('foreign runtime reached native mutation'))
    with pytest.raises(ValueError, match='runtime'):
        control.change_limits(tmp_path, dict(expected_worker_slots=8, expected_heavy_worker_slots=6,
                                            worker_slots=8, heavy_worker_slots=3))
    assert registry.read_bytes() == before
    assert not (tmp_path / 'protocol_state.json').exists()


@pytest.fixture
def authenticated_checker(monkeypatch):
    from trellis.checker.server import CheckerServer
    from trellis.checker.workspaces import registry_path
    from trellis.runtime.bridge import _register_burst_token
    monkeypatch.setenv('TRELLIS_CLOSURE_PRECOMPILE', '0')
    monkeypatch.setenv('TRELLIS_SEMANTIC_PRECOMPILE', '0')
    # AF_UNIX needs a short path. Exercise the real socket and authentication,
    # replacing only the compiler work after the request has been authorized.
    with tempfile.TemporaryDirectory(prefix='vctl-', dir='/tmp') as directory:
        repo = Path(directory) / 'repo'
        runtime = repo / '.trellis/runtime/test'
        runtime.mkdir(parents=True)
        (repo / 'Tablet').mkdir()
        server = CheckerServer(runtime, parallelism=1, socket_group_gid=None)
        registry_path(runtime).mkdir()
        _register_burst_token(runtime, token='supervisor', burst_id='supervisor',
                              kind='supervisor', request_id=0, cycle=0)
        server.start()
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield server
        finally:
            server.shutdown()
            thread.join(timeout=5)


@pytest.mark.parametrize('denied', [False, True])
def test_capacity_checkpoint_auth_lifetime_and_failure(authenticated_checker, monkeypatch, denied):
    from trellis.atomic_actions.checker_client import CheckerRpcError, client_prepare_compiled_support
    from trellis.checker.protocol import ProtocolError
    from trellis.dispatch.native import NativeLedger
    from trellis.runtime.bridge import _register_burst_token
    server = authenticated_checker
    runtime = server.runtime_root
    launch_environment(runtime)  # Captured launches do not contain a supervisor token.
    monkeypatch.setenv('TRELLIS_CHECKER_TOKEN', 'unrelated-viewer-token')
    state_path = runtime / 'protocol_state.json'
    state = {'config': {'worker_slots': 8, 'heavy_worker_slots': 6},
             'tasks': {'1': {'status': 'running'}, '2': {'status': 'candidate_ready'}}}
    state_path.write_text(json.dumps(state))
    before = state_path.read_bytes()
    tokens = []
    def prepare(request):
        assert request.op == 'prepare_compiled_support'
        tokens.append(request.raw['auth_token'])
        if denied: raise ProtocolError('fixture_denied', 'compiled support refused')
        return {'request_id': request.request_id, 'returncode': 0}
    monkeypatch.setattr(server, '_handle_request', prepare)
    def checkpoint(payload):
        assert payload['action'] == 'dispatch_control'
        assert payload['event']['kind'] == 'agent_capacity_changed'
        assert os.environ['TRELLIS_CHECKER_SOCKET'] == str(server.socket_path)
        # A new worker grant written during the control transaction must survive
        # both successful and failed control cleanup.
        worker = threading.Thread(target=lambda: _register_burst_token(runtime, token='new-worker',
            burst_id='worker', kind='worker', request_id=1, cycle=1))
        worker.start(); worker.join(timeout=5)
        assert not worker.is_alive()
        client_prepare_compiled_support(server.socket_path, runtime, timeout_secs=5)
        state['config']['heavy_worker_slots'] = 3
        state_path.write_text(json.dumps(state))
        return {'status': 'dispatch_ok', 'output': state}
    monkeypatch.setattr(control, 'NativeLedger', lambda root: NativeLedger(root, kernel=checkpoint))
    request = dict(expected_worker_slots=8, expected_heavy_worker_slots=6, worker_slots=8, heavy_worker_slots=3)
    if denied:
        with pytest.raises(CheckerRpcError, match='compiled support refused'):
            control.change_limits(runtime, request)
        assert state_path.read_bytes() == before
    else:
        assert control.change_limits(runtime, request) == {'worker_slots': 8, 'heavy_worker_slots': 3}
        assert json.loads(state_path.read_text())['tasks'] == json.loads(before)['tasks']
    assert len(tokens) == 1 and tokens[0] != 'unrelated-viewer-token'
    registry = json.loads(server.burst_tokens_path.read_text())
    assert registry['runtime_root'] == str(runtime.resolve())
    assert registry['tokens'] == ['supervisor', 'new-worker']
    assert os.environ['TRELLIS_CHECKER_TOKEN'] == 'unrelated-viewer-token'
    for token in [tokens[0], 'unrelated-viewer-token', None]:
        if token is None: monkeypatch.delenv('TRELLIS_CHECKER_TOKEN')
        else: monkeypatch.setenv('TRELLIS_CHECKER_TOKEN', token)
        with pytest.raises(CheckerRpcError, match='auth_token missing or unknown'):
            client_prepare_compiled_support(server.socket_path, runtime, timeout_secs=5)


@pytest.mark.parametrize('extra', [dict(worker_slots=0), dict(heavy_worker_slots=True),
    dict(heavy_worker_slots=1.5), dict(kind='attempt_stopping'), dict(expected_heavy_worker_slots='1')])
def test_limit_control_rejects_non_capacity_mutations(extra):
    value = dict(expected_worker_slots=8, expected_heavy_worker_slots=None, worker_slots=2, heavy_worker_slots=1)
    with pytest.raises(ValueError):
        control.capacity_event(dict(value, **extra))
