"""Consumer key batching preserves exact recursive source/artifact identities."""
import hashlib
from pathlib import Path
import os
import weakref

import pytest

from trellis.atomic_actions import observations as obs
from trellis.checker import sync


def fixture(repo, count=8):
    (repo / 'Tablet').mkdir(parents=True)
    build = repo / '.lake/build/lib/lean/Tablet'
    build.mkdir(parents=True)
    cache = {}
    for i in range(count):
        name = f'N{i}'
        # Shared recursive closure, multi-import lines, CRLF and nested comments.
        data = ((f'public import Tablet.N{i-1} Tablet.N0\r\n' if i else '')
                + '/- import Tablet.Missing /- nested -/ -/\r\n'
                + f'def n{i} := {i}\r\n').encode()
        (repo / 'Tablet' / (name + '.lean')).write_bytes(data)
        cache[name + '.lean'] = {'sha256': hashlib.sha256(data).hexdigest()}
        for suffix in ['.olean', '.olean.server', '.olean.private']:
            (build / (name + suffix)).write_bytes((name + suffix).encode() * 16)
    return cache


def scalar(repo, names, cache):
    return {name: sync.compute_semantic_payload_cache_key(repo, name, cache, 's', 't', 'm', 5)
            for name in names}


def batch(repo, names, cache):
    return sync.compute_semantic_payload_cache_keys(repo, names, cache, 's', 't', 'm', 5)


def test_batch_exact_keys_and_each_artifact_read_twice(tmp_path, monkeypatch):
    cache = fixture(tmp_path)
    names = [f'N{i}' for i in range(8)]
    expected = scalar(tmp_path, names, cache)
    calls = []
    original = obs._artifact_bundle
    def bundle(repo, node):
        calls.append(node)
        return original(repo, node)
    monkeypatch.setattr(obs, '_artifact_bundle', bundle)
    assert batch(tmp_path, names, cache) == expected
    assert all(key for key in expected.values())
    assert len(calls) == 2 * len(names)
    assert all(calls.count(node) == 2 for node in names)


def test_singleton_uses_one_bundle_pass_and_is_fresh_between_calls(tmp_path, monkeypatch):
    cache = fixture(tmp_path)
    original = obs._artifact_bundle
    calls = []
    def bundle(repo, node):
        calls.append(node)
        return original(repo, node)
    expected = scalar(tmp_path, ['N7'], cache)
    monkeypatch.setattr(obs, '_artifact_bundle', bundle)
    assert batch(tmp_path, ['N7'], cache) == expected
    assert len(calls) == 8
    (tmp_path / '.lake/build/lib/lean/Tablet/N0.olean.private').write_bytes(b'changed')
    assert batch(tmp_path, ['N7'], cache) != expected


@pytest.mark.parametrize('change', ['source', 'exported', 'server', 'private',
                                     'new_optional', 'removed_optional', 'orphan_private'])
def test_batch_drift_is_cold_even_with_restored_mtime(tmp_path, monkeypatch, change):
    cache = fixture(tmp_path)
    names = ['N6', 'N7']
    build = tmp_path / '.lake/build/lib/lean/Tablet'
    if change == 'new_optional':
        (build / 'N0.olean.server').unlink()
        (build / 'N0.olean.private').unlink()
    original = obs._artifact_bundle
    calls = 0
    def bundle(repo, node):
        nonlocal calls
        value = original(repo, node)
        calls += 1
        # Every original bundle has now been read, before validation rereads.
        if calls == 8:
            if change == 'source':
                path = repo / 'Tablet/N0.lean'
            elif change in ['exported', 'server', 'private']:
                path = build / ('N0.olean' + ('' if change == 'exported' else '.' + change))
            else:
                path = build / 'N0.olean.server'
            if change in ['removed_optional', 'orphan_private']:
                path.unlink()
                if change == 'removed_optional':
                    (build / 'N0.olean.private').unlink()
            elif change == 'new_optional':
                path.write_bytes(b'new part')
            else:
                st = path.stat()
                data = path.read_bytes()
                path.write_bytes(bytes([data[0] ^ 1]) + data[1:])
                os.utime(path, ns=(st.st_atime_ns, st.st_mtime_ns))
        return value
    monkeypatch.setattr(obs, '_artifact_bundle', bundle)
    assert batch(tmp_path, names, cache) == dict.fromkeys(names)


def test_fresh_passes_observe_changed_import_closure_and_parts(tmp_path):
    cache = fixture(tmp_path)
    names = ['N7', 'N0']
    before = batch(tmp_path, names, cache)
    (tmp_path / 'Tablet/N7.lean').write_bytes(b'import Tablet.N0\ndef n7 := 7\n')
    cache['N7.lean']['sha256'] = hashlib.sha256((tmp_path / 'Tablet/N7.lean').read_bytes()).hexdigest()
    after = batch(tmp_path, names, cache)
    assert after == scalar(tmp_path, names, cache)
    assert before['N7'] != after['N7']
    assert before['N0'] == after['N0']
    (tmp_path / '.lake/build/lib/lean/Tablet/N0.olean.private').unlink()
    assert batch(tmp_path, names, cache) == scalar(tmp_path, names, cache)
    assert batch(tmp_path, names, cache) != after


def test_missing_dependency_is_cold_without_poisoning_independent_owner(tmp_path):
    cache = fixture(tmp_path)
    del cache['N3.lean']
    assert batch(tmp_path, ['N7', 'N0'], cache) == scalar(tmp_path, ['N7', 'N0'], cache)
    assert batch(tmp_path, ['N7', 'N0'], cache)['N7'] is None
    # A stale sync row/olean must not mask an unreadable or missing source.
    (tmp_path / 'Tablet/N0.lean').unlink()
    assert batch(tmp_path, ['N0'], cache) == {'N0': None}


def test_oversize_releases_snapshot_before_scalar_fallback(tmp_path, monkeypatch):
    cache = fixture(tmp_path)
    expected = scalar(tmp_path, ['N7', 'N0'], cache)
    snapshots = []
    base = obs.SourceClosureSnapshot
    class Limited(base):
        MAX_BYTES = 1
        def __init__(self, *args):
            super().__init__(*args)
            snapshots.append(weakref.ref(self))
    monkeypatch.setattr(obs, 'SourceClosureSnapshot', Limited)
    original = sync.compute_semantic_payload_cache_key
    def fallback(*args):
        assert snapshots and all(ref() is None for ref in snapshots)
        return original(*args)
    monkeypatch.setattr(sync, 'compute_semantic_payload_cache_key', fallback)
    assert batch(tmp_path, ['N7', 'N0'], cache) == expected


def test_cycles_and_optional_artifacts_preserve_scalar_keys(tmp_path):
    cache = fixture(tmp_path)
    path = tmp_path / 'Tablet/N0.lean'
    path.write_bytes(b'import Tablet.N7\n')
    cache['N0.lean']['sha256'] = hashlib.sha256(path.read_bytes()).hexdigest()
    assert batch(tmp_path, ['N7', 'N0', 'N7'], cache) == scalar(tmp_path, ['N7', 'N0'], cache)
    assert batch(tmp_path, [], cache) == {}


def test_concurrent_batches_do_not_share_workspace_observations(tmp_path):
    from concurrent.futures import ThreadPoolExecutor
    left, right = tmp_path / 'left', tmp_path / 'right'
    caches = {repo: fixture(repo) for repo in [left, right]}
    (right / '.lake/build/lib/lean/Tablet/N0.olean.private').write_bytes(b'other workspace')
    names = ['N7', 'N6']
    expected = {repo: scalar(repo, names, caches[repo]) for repo in [left, right]}
    assert expected[left] != expected[right]
    with ThreadPoolExecutor(max_workers=2) as pool:
        actual = list(pool.map(lambda repo: batch(repo, names, caches[repo]), [left, right]))
    assert actual == [expected[left], expected[right]]
