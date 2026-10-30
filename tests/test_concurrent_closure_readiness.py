"""Concurrent closure checks share exact reads, never old readiness verdicts."""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import os
import threading

import pytest

from trellis.atomic_actions import observations as obs
from trellis.checker.concurrent_reads import ConcurrentReadBatch
from test_semantic_replay_readiness import fixture


def together(fn, nodes):
    barrier = threading.Barrier(len(nodes))
    def run(node):
        barrier.wait(3)
        return fn(node)
    with ThreadPoolExecutor(len(nodes)) as pool:
        return list(pool.map(run, nodes))


def test_simultaneous_readers_share_bundle_hashes_but_next_pass_is_fresh(tmp_path, monkeypatch):
    server, names = fixture(tmp_path, count=10)
    server.parallelism = 6
    server._closure_readiness_batch.window_seconds = .03
    original = obs._artifact_bundle
    calls = []
    def bundle(repo, node):
        calls.append(node)
        return original(repo, node)
    monkeypatch.setattr(obs, '_artifact_bundle', bundle)
    requested = names[-6:]
    assert together(server._prepared_closure_readiness, requested) == [True] * 6
    assert len(calls) == 2 * (len(names) + 1)
    calls.clear()
    artifact = obs._tablet_olean_path(server.supervisor_repo, 'N0')
    before = artifact.stat()
    artifact.write_bytes(b'changed without changing mtime')
    os.utime(artifact, ns=(before.st_atime_ns, before.st_mtime_ns))
    assert together(server._prepared_closure_readiness, requested) == [False] * 6
    assert calls, 'a previous readiness verdict was reused'


def test_arrival_during_read_waits_for_a_new_pass():
    entered, release = threading.Event(), threading.Event()
    calls = []
    def read(nodes):
        calls.append(nodes)
        if len(calls) == 1:
            entered.set()
            assert release.wait(3)
        return dict.fromkeys(nodes, len(calls))
    batch = ConcurrentReadBatch(read)
    with ThreadPoolExecutor(2) as pool:
        first = pool.submit(batch, 'A')
        assert entered.wait(3)
        second = pool.submit(batch, 'A')
        # Register the later reader before completing the old pass.
        with batch.condition:
            assert batch.condition.wait_for(lambda: bool(batch.pending), 3)
        release.set()
        assert first.result(3) == 1
        assert second.result(3) == 2


def test_failure_wakes_all_readers_and_does_not_poison_next_batch():
    failed = [True]
    def read(nodes):
        if failed[0]:
            raise ValueError('read failed')
        return dict.fromkeys(nodes, True)
    batch = ConcurrentReadBatch(read, window_seconds=.03)
    def run(node):
        with pytest.raises(ValueError, match='read failed'):
            batch(node)
    together(run, ['A', 'B', 'C'])
    failed[0] = False
    assert batch('A') is True
    assert not batch.pending and not batch.running


def test_concurrent_keys_match_scalar_and_revalidate_each_pass(tmp_path, monkeypatch):
    from trellis.checker import server as mod
    server, names = fixture(tmp_path, count=10)
    server.parallelism = 6
    server._closure_key_batch.window_seconds = .03
    cache = {p.name: {'sha256': hashlib.sha256(p.read_bytes()).hexdigest()}
             for p in (server.supervisor_repo / 'Tablet').glob('*.lean')}
    monkeypatch.setattr(mod, 'load_fingerprint_cache', lambda _: cache)
    requested = names[-6:]
    def scalar(node, script='script'):
        return mod.compute_semantic_payload_cache_key(server.supervisor_repo, node,
            cache, script, 'toolchain', 'manifest', mod.LOCAL_CLOSURE_AXIOMS_CACHE_VERSION)
    expected = [scalar(node) for node in requested]
    assert all(expected)
    calls = []
    original = obs._artifact_bundle
    def bundle(repo, node):
        calls.append(node)
        return original(repo, node)
    monkeypatch.setattr(obs, '_artifact_bundle', bundle)
    read = lambda node: server._prepared_closure_cache_key(node, cache,
        'script', 'toolchain', 'manifest')
    assert together(read, requested) == expected
    assert len(calls) == 2 * (len(names) + 1)
    artifact = obs._tablet_olean_path(server.supervisor_repo, 'N0')
    stat = artifact.stat()
    artifact.write_bytes(b'new artifact')
    os.utime(artifact, ns=(stat.st_atime_ns, stat.st_mtime_ns))
    changed = together(read, requested)
    assert all(a != b for a, b in zip(changed, expected))
    assert changed == [scalar(node) for node in requested]
    # Distinct collector identities must not share keys, even for one owner.
    contexts = ['first-script', 'second-script']
    actual = together(lambda script: server._prepared_closure_cache_key(names[-1],
        cache, script, 'toolchain', 'manifest'), contexts)
    assert actual == [scalar(names[-1], script) for script in contexts]
    assert actual[0] != actual[1]
