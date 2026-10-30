"""Host cache copies still pass the real derivation/replay consumer gates."""
import json
import os

import pytest

from trellis.atomic_actions import observations as obs
from trellis.checker import statement_transform as transform
from trellis.checker.protocol import CheckerRequest
from trellis.checker.server import CheckerServer
from trellis.dispatch.candidate import source_manifest
from trellis.dispatch.workspace import prepare
from test_dispatch_workspace import project, trusted_build


@pytest.fixture
def derivation(trusted_build, monkeypatch):
    repo, runtime, config, authority = trusted_build
    server = CheckerServer(runtime, parallelism=1)
    request = CheckerRequest(op='lean_statement_transform', request_id=7, timeout_secs=1,
        raw={'input': {'name': 'Original', 'statement': 'theorem Original : True := by',
                       'context': 'import Tablet.B\n'}})
    calls = []
    def compile(actual_repo, args, **kwargs):
        calls.append(actual_repo)
        return {'returncode': 0, 'stdout': json.dumps(
            {'status': 'ok', 'result': {'refutation_type': '¬ True'}})}
    monkeypatch.setattr(obs, '_run_lake_command', compile)
    monkeypatch.setattr(obs, 'materialize_tablet_oleans',
        lambda *a, **kw: pytest.fail('unchanged replay-attested seed should not rebuild'))
    with server._workspace_lock:
        response, code, _ = transform.handle(server, request)
    assert code == 0
    assert calls == [authority]
    source = next((runtime / 'checker-state/statement-transforms').glob('*.json'))
    # Workspace preparation may run Git, but the consumer never launches Lean
    # outside the explicitly stubbed helper above.
    def seed():
        target = runtime / 'dispatch/1'
        prepare(runtime, repo, config, target, {'in_flight_request': {'id': 7}},
            expected_manifest=source_manifest(repo), seed_packages=False)
        private = CheckerServer(target / 'runtime', parallelism=1)
        return target, private
    return source, response, request, calls, seed


def test_seeded_derivation_reuses_exact_response_without_shared_writable_inode(derivation):
    source, expected, request, calls, seed = derivation
    target, private = seed()
    copied = target / 'runtime/checker-state/statement-transforms' / source.name
    assert copied.stat().st_ino != source.stat().st_ino
    assert json.loads(copied.read_text()) == json.loads(source.read_text())
    assert json.loads((target / 'prepared.json').read_text())['seeded_statement_transforms'] == 1
    with private._workspace_lock:
        actual, code, extra = transform.handle(private, request)
    assert code == 0 and actual == expected
    assert extra == {'statement_transform_cache_hit': True}
    assert len(calls) == 1
    copied.write_text('{')
    assert json.loads(source.read_text())['response']['status'] == 'ok'


@pytest.mark.parametrize('change', ['source', 'toolchain', 'bundle', 'replay'])
def test_seeded_derivation_cannot_bypass_changed_replay_inputs(derivation, monkeypatch, change):
    source, expected, request, calls, seed = derivation
    target, private = seed()
    path = {
        'source': private.supervisor_repo / 'Tablet/A.lean',
        'toolchain': private.supervisor_repo / 'lean-toolchain',
        'bundle': obs._tablet_olean_path(private.supervisor_repo, 'A'),
        'replay': obs._olean_srcclosure_sidecar_path(private.supervisor_repo, 'A'),
    }[change]
    stamp = path.stat()
    path.write_bytes(path.read_bytes() + b'changed')
    os.utime(path, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
    rebuilds = []
    def rejected_replay(*args, **kwargs):
        rebuilds.append(args)
        return {'returncode': 1, 'stderr': 'changed inputs require authoritative replay'}
    monkeypatch.setattr(obs, 'materialize_tablet_oleans', rejected_replay)
    with private._workspace_lock:
        response, code, _ = transform.handle(private, request)
    assert code == 1 and 'require authoritative replay' in response['error']
    assert rebuilds and len(calls) == 1


@pytest.mark.parametrize('damage', ['json', 'version', 'key', 'response', 'symlink'])
def test_invalid_host_cache_is_not_seeded_and_consumer_runs_helper(derivation, damage):
    source, expected, request, calls, seed = derivation
    if damage == 'json':
        source.write_text('{')
    elif damage == 'symlink':
        elsewhere = source.with_suffix('.saved')
        source.rename(elsewhere)
        source.symlink_to(elsewhere)
    else:
        record = json.loads(source.read_text())
        record[{'version': 'cache_version', 'key': 'key_blob_sha256', 'response': 'response'}[damage]] = 'invalid'
        source.write_text(json.dumps(record))
    target, private = seed()
    assert json.loads((target / 'prepared.json').read_text())['seeded_statement_transforms'] == 0
    with private._workspace_lock:
        actual, code, extra = transform.handle(private, request)
    assert code == 0 and actual == expected
    assert extra == {'statement_transform_cache_hit': False}
    assert len(calls) == 2


def test_copied_cache_corruption_is_a_consumer_miss(derivation):
    source, expected, request, calls, seed = derivation
    target, private = seed()
    copied = target / 'runtime/checker-state/statement-transforms' / source.name
    copied.write_text('{')
    with private._workspace_lock:
        actual, code, extra = transform.handle(private, request)
    assert code == 0 and actual == expected
    assert extra == {'statement_transform_cache_hit': False}
    assert len(calls) == 2


def test_seed_ignores_agent_repository_cache(derivation):
    source, expected, request, calls, seed = derivation
    repo = source.parents[2].parent / 'repo'
    poisoned = repo / '.trellis/runtime/checker-state/statement-transforms' / source.name
    poisoned.parent.mkdir(parents=True)
    record = json.loads(source.read_text())
    record['response']['result']['refutation_type'] = 'agent supplied'
    poisoned.write_text(json.dumps(record))
    target, private = seed()
    with private._workspace_lock:
        actual, code, extra = transform.handle(private, request)
    assert code == 0 and actual == expected
    assert extra == {'statement_transform_cache_hit': True}
    assert len(calls) == 1


@pytest.mark.parametrize('change', ['input', 'helper'])
def test_seeded_derivation_still_binds_complete_input_and_helper(derivation, monkeypatch, change):
    source, expected, request, calls, seed = derivation
    target, private = seed()
    if change == 'input':
        request.raw['input']['statement'] = 'theorem Original : False := by'
    else:
        original = transform._hash_file
        monkeypatch.setattr(transform, '_hash_file', lambda path:
            'f' * 64 if path.name == 'lean_statement_transform.lean' else original(path))
    with private._workspace_lock:
        actual, code, extra = transform.handle(private, request)
    assert code == 0
    assert extra == {'statement_transform_cache_hit': False}
    assert len(calls) == 2
