"""Exact per-pass replay inventories; no reuse through collector execution."""
import hashlib
import json
import os
from pathlib import Path
import weakref

import pytest

from trellis.atomic_actions import observations as obs
from trellis.checker import server as mod
from trellis.checker.protocol import CheckerRequest
from test_local_closure_cache_prefilter import make_server


def fixture(tmp_path, count=12, artifact_bytes=128):
    server = make_server(tmp_path)
    repo = server.supervisor_repo
    names = [f'N{i}' for i in range(count)]
    for i, node in enumerate(names):
        source = f'import Tablet.{names[i-1] if i else "Preamble"}\ndef x{i} := {i}\n'
        (repo / f'Tablet/{node}.lean').write_text(source)
        artifact = obs._tablet_olean_path(repo, node)
        for suffix in ('', '.server', '.private'):
            Path(str(artifact) + suffix).write_bytes(bytes([i % 256]) * artifact_bytes)
    for node in names:
        obs.write_olean_srcclosure(repo, node, obs.tablet_source_closure_hash(repo, node))
        assert obs._write_kernel_replay_attestation(repo, node, olean_sha256='unused')
    return server, names


def scalar(server, names):
    return {node: server._closures_have_current_kernel_replay([node]) for node in names}


def test_exact_readiness_with_linear_bundle_work(tmp_path, monkeypatch):
    server, names = fixture(tmp_path)
    original = obs._artifact_bundle
    calls = []
    def bundle(repo, node):
        calls.append(node)
        return original(repo, node)
    monkeypatch.setattr(obs, '_artifact_bundle', bundle)
    expected = scalar(server, names)
    assert all(expected.values())
    assert len(calls) == sum(range(2, len(names) + 2))
    calls.clear()
    assert server._semantic_replay_readiness(names) == expected
    assert len(calls) == 2 * (len(names) + 1)
    assert all(calls.count(node) == 2 for node in ['Preamble', *names])


@pytest.mark.parametrize('change', ['exported', 'server', 'private', 'add_optional',
    'remove_optional', 'orphan_private', 'source', 'new_import', 'provenance', 'toolchain'])
def test_mutation_during_pass_and_next_pass_are_cold(tmp_path, monkeypatch, change):
    server, names = fixture(tmp_path, count=4)
    repo = server.supervisor_repo
    artifact = obs._tablet_olean_path(repo, 'N0')
    if change == 'add_optional':
        Path(str(artifact) + '.private').unlink()
        Path(str(artifact) + '.server').unlink()
        assert obs._write_kernel_replay_attestation(repo, 'N0', olean_sha256='unused')
    original = obs.olean_has_current_kernel_replay
    mutated = False
    def replay(root, node, **kwargs):
        nonlocal mutated
        result = original(root, node, **kwargs)
        if node == names[-1] and not mutated:
            mutated = True
            if change in {'remove_optional', 'orphan_private'}:
                Path(str(artifact) + '.server').unlink()
                if change == 'remove_optional':
                    Path(str(artifact) + '.private').unlink()
            elif change == 'add_optional':
                Path(str(artifact) + '.server').write_bytes(b'new server')
            else:
                target = {
                    'exported': artifact, 'server': Path(str(artifact) + '.server'),
                    'private': Path(str(artifact) + '.private'),
                    'source': repo / 'Tablet/N0.lean', 'new_import': repo / 'Tablet/N0.lean',
                    'provenance': Path(str(artifact) + '.srcclosure'),
                    'toolchain': repo / 'lean-toolchain',
                }[change]
                stat = target.stat()
                data = target.read_bytes()
                target.write_bytes(b'import Tablet.Missing\n' + data if change == 'new_import'
                                   else bytes([data[0] ^ 1]) + data[1:])
                os.utime(target, ns=(stat.st_atime_ns, stat.st_mtime_ns))
        return result
    monkeypatch.setattr(obs, 'olean_has_current_kernel_replay', replay)
    assert server._semantic_replay_readiness(names) == dict.fromkeys(names, False)
    assert server._semantic_replay_readiness(names) == scalar(server, names)
    assert not any(scalar(server, names).values())


def test_cold_branch_does_not_poison_independent_readiness(tmp_path):
    server, names = fixture(tmp_path, count=4)
    obs._tablet_olean_path(server.supervisor_repo, 'N2').unlink()
    requested = [*names, 'Root', 'Missing']
    result = server._semantic_replay_readiness(requested)
    assert result == scalar(server, requested)
    assert result['Root'] and result['N1'] and not result['N3'] and not result['Missing']


def test_bounded_fallback_releases_inventory_and_lock(tmp_path, monkeypatch):
    server, names = fixture(tmp_path, count=4)
    expected = scalar(server, names)
    references = []
    base = obs.SourceClosureSnapshot
    class Limited(base):
        MAX_BYTES = 1
        def __init__(self, *args):
            super().__init__(*args)
            references.append(weakref.ref(self))
    monkeypatch.setattr(obs, 'SourceClosureSnapshot', Limited)
    original = server._closures_have_current_kernel_replay
    def fallback(nodes):
        assert references and all(ref() is None for ref in references)
        return original(nodes)
    monkeypatch.setattr(server, '_closures_have_current_kernel_replay', fallback)
    assert server._semantic_replay_readiness(names) == expected


@pytest.mark.parametrize("mutate", [False, True])
def test_handler_starts_fresh_readiness_after_collector(tmp_path, monkeypatch, mutate):
    server, names = fixture(tmp_path, count=3)
    repo = server.supervisor_repo
    cache = {p.name: {'sha256': hashlib.sha256(p.read_bytes()).hexdigest()}
             for p in (repo / 'Tablet').glob('*.lean')}
    server.fingerprint_cache_path.write_text(json.dumps(cache))
    server._fingerprint_script_path = repo / '.trellis/scripts/fingerprints.lean'
    server._fingerprint_script_path.write_text('-- fixture collector\n')
    identity = {'checker_script_hash': mod._sha256_file_or_empty(server._fingerprint_script_path),
                'toolchain_hash': mod._sha256_file_or_empty(server._toolchain_path)}
    monkeypatch.setattr(server, '_semantic_execution_identity', lambda: (identity, Path('lean'), Path('lake')))
    monkeypatch.setattr(server, '_pin_collector', lambda *_args: server._fingerprint_script_path)
    monkeypatch.setattr(server._compiled_semantic_probe, 'prepare', lambda *a, **kw: (Path('probe'), None))
    events = []
    readiness = server._semantic_replay_readiness
    def ready(nodes):
        events.append(('readiness', tuple(nodes)))
        return readiness(nodes)
    monkeypatch.setattr(server, '_semantic_replay_readiness', ready)
    def collector(_repo, nodes, **kwargs):
        events.append(('collector', tuple(nodes)))
        # An elaborator may write build outputs despite the host workspace gate.
        path = obs._tablet_olean_path(repo, 'N0')
        if mutate:
            path.write_bytes(b'changed artifact')
        return {node: {'ok': True, 'payload': 'typed fixture', 'error': ''} for node in nodes}
    monkeypatch.setattr(obs, 'observe_lean_semantic_payloads', collector)
    stores = []
    monkeypatch.setattr(mod, 'store_semantic_payload', lambda *a, **kw: stores.append((a[1], kw)))
    request = CheckerRequest(op='lean_semantic_payloads', request_id=1, nodes=names,
        timeout_secs=1, raw={'principal_names': dict(zip(names, names))})
    with server._workspace_lock:
        response, _, log = server._handle_lean_semantic_payloads(request)
    expected_events = ['readiness', 'collector', 'readiness']
    if mutate:
        # Existing shared-group drift handling discards the group and retries
        # once through the single-node collector path, then rechecks readiness.
        expected_events += ['collector', 'readiness']
    assert [event[0] for event in events] == expected_events
    assert all(nodes == tuple(names) for kind, nodes in events)
    assert all(row['ok'] for row in response['nodes'].values())  # unchanged delivery semantics
    assert log['cache_misses'] == len(names)
    if mutate:
        assert stores == []  # successful output cannot seed a stale artifact cache
    else:
        from trellis.checker.sync import compute_semantic_payload_cache_key
        expected = {
            compute_semantic_payload_cache_key(repo, node, cache,
                identity['checker_script_hash'], identity['toolchain_hash'],
                mod._sha256_file_or_empty(server._lake_manifest_path),
                mod.SEMANTIC_PAYLOAD_CACHE_VERSION)
            + '-principal-' + hashlib.sha256(node.encode()).hexdigest()
            for node in names
        }
        assert {key for key, _ in stores} == expected
        assert all(row['ok'] and row['payload'] == 'typed fixture' for _, row in stores)
