"""Bounded negative metadata hints never authorize a cached observation."""
import errno
import json
import os
from pathlib import Path

import pytest

from trellis.checker import sync, server as mod
from test_local_closure_cache_prefilter import (
    candidate, no_external_processes, request, server,
)


def sidecar(root, index=0, node='Other'):
    root.mkdir(exist_ok=True)
    p = root / f'{index:064x}.json'
    p.write_text(json.dumps({'node_name': node}))
    return p


def test_missing_empty_and_other_node_are_only_negative_hints(tmp_path):
    root = tmp_path / 'cache'
    assert sync.local_closure_cache_definitely_lacks_node(root, 'Root') is True
    root.mkdir()
    assert sync.local_closure_cache_definitely_lacks_node(root, 'Root') is True
    sidecar(root)
    assert sync.local_closure_cache_definitely_lacks_node(root, 'Root') is True
    sidecar(root, 1, 'Root')
    assert sync.local_closure_cache_definitely_lacks_node(root, 'Root') is False


@pytest.mark.parametrize('data', [b'{', b'[]', b'{}', b'{"node_name":null}', b'{"node_name":""}', b'\xff'])
def test_uncertain_metadata_preserves_original_lookup(tmp_path, data):
    p = sidecar(tmp_path)
    p.write_bytes(data)
    assert sync.local_closure_cache_definitely_lacks_node(tmp_path, 'Root') is False


def test_exact_file_and_total_byte_boundaries(tmp_path, monkeypatch):
    p = sidecar(tmp_path)
    size = p.stat().st_size
    monkeypatch.setattr(sync, '_LOCAL_CLOSURE_SCAN_FILE_BYTES', size)
    monkeypatch.setattr(sync, '_LOCAL_CLOSURE_SCAN_BYTES', size)
    assert sync.local_closure_cache_definitely_lacks_node(tmp_path, 'Root') is True
    sidecar(tmp_path, 1)
    assert sync.local_closure_cache_definitely_lacks_node(tmp_path, 'Root') is False
    monkeypatch.setattr(sync, '_LOCAL_CLOSURE_SCAN_BYTES', size * 2)
    assert sync.local_closure_cache_definitely_lacks_node(tmp_path, 'Root') is True
    monkeypatch.setattr(sync, '_LOCAL_CLOSURE_SCAN_FILE_BYTES', size - 1)
    assert sync.local_closure_cache_definitely_lacks_node(tmp_path, 'Root') is False


def test_entry_budget_counts_noncache_names_too(tmp_path, monkeypatch):
    monkeypatch.setattr(sync, '_LOCAL_CLOSURE_SCAN_ENTRIES', 2)
    (tmp_path / 'unrelated').touch()
    sidecar(tmp_path)
    assert sync.local_closure_cache_definitely_lacks_node(tmp_path, 'Root') is True
    (tmp_path / 'third').touch()
    assert sync.local_closure_cache_definitely_lacks_node(tmp_path, 'Root') is False


@pytest.mark.parametrize('kind', ['file_symlink', 'directory_symlink', 'fifo'])
def test_aliases_and_nonregular_entries_defer(tmp_path, kind):
    root = tmp_path / 'cache'
    p = sidecar(root)
    if kind == 'file_symlink':
        target = tmp_path / 'target'
        target.write_text('{"node_name":"Other"}')
        p.unlink(); p.symlink_to(target)
    elif kind == 'directory_symlink':
        alias = tmp_path / 'alias'; alias.symlink_to(root, target_is_directory=True)
        root = alias
    else:
        p.unlink(); os.mkfifo(p)
    assert sync.local_closure_cache_definitely_lacks_node(root, 'Root') is False


@pytest.mark.parametrize('error', [errno.EACCES, errno.EIO, errno.ENOENT])
def test_entry_read_failure_does_not_masquerade_as_absent_directory(tmp_path, monkeypatch, error):
    sidecar(tmp_path)
    original = os.open
    def fail(path, *args, **kwargs):
        if kwargs.get('dir_fd') is not None:
            raise OSError(error, 'injected')
        return original(path, *args, **kwargs)
    monkeypatch.setattr(sync.os, 'open', fail)
    assert sync.local_closure_cache_definitely_lacks_node(tmp_path, 'Root') is False


def test_size_change_during_read_defers(tmp_path, monkeypatch):
    p = sidecar(tmp_path)
    original = os.fstat
    calls = []
    def changed(fd):
        calls.append(fd)
        if len(calls) == 2:
            with p.open('ab') as f: f.write(b' ')
        return original(fd)
    monkeypatch.setattr(sync.os, 'fstat', changed)
    assert sync.local_closure_cache_definitely_lacks_node(tmp_path, 'Root') is False


def test_cold_owner_avoids_advisory_artifact_hashes(server, monkeypatch):
    monkeypatch.setattr(mod, 'compute_semantic_payload_cache_key',
                        lambda *a, **k: pytest.fail('cold metadata hint hashed artifacts'))
    assert server._local_closure_axioms_cache_definitely_misses(request()) is True
    assert server._try_local_closure_axioms_cache_hit(request()) is None


def test_warm_owner_keeps_full_key_and_replay_validation(server, monkeypatch):
    req = request(); candidate(server, req)
    key = mod.compute_semantic_payload_cache_key
    ready = server._closures_have_current_kernel_replay
    calls = []
    def compute(*args, **kwargs):
        calls.append('key'); return key(*args, **kwargs)
    def replay(nodes):
        calls.append('replay'); return ready(nodes)
    monkeypatch.setattr(mod, 'compute_semantic_payload_cache_key', compute)
    monkeypatch.setattr(server, '_closures_have_current_kernel_replay', replay)
    assert server._try_local_closure_axioms_cache_hit(req)[2]['local_closure_axioms_cache_hit']
    assert calls == ['key', 'replay', 'key']


def test_cache_insertion_is_observed_on_next_attempt(server):
    req = request()
    assert server._try_local_closure_axioms_cache_hit(req) is None
    candidate(server, req)
    assert server._try_local_closure_axioms_cache_hit(req)[2]['local_closure_axioms_cache_hit']


def test_uncertain_scan_falls_through_existing_key_lookup(server, monkeypatch):
    req = request(); path = candidate(server, req)
    path.write_text('{')
    original = mod.compute_semantic_payload_cache_key
    calls = []
    def compute(*args, **kwargs):
        calls.append(True); return original(*args, **kwargs)
    monkeypatch.setattr(mod, 'compute_semantic_payload_cache_key', compute)
    assert server._try_local_closure_axioms_cache_hit(req) is None
    assert calls == [True]


def test_insertion_during_scan_cannot_authorize_reuse(server, monkeypatch):
    req = request()
    server.local_closure_axioms_cache_dir.mkdir(parents=True, exist_ok=True)
    original = os.scandir
    inserted = []
    class Scan:
        def __init__(self, directory): self.inner = original(directory)
        def __enter__(self): return self
        def __exit__(self, *args): self.inner.close()
        def __iter__(self): return self
        def __next__(self):
            try: return next(self.inner)
            except StopIteration:
                if not inserted:
                    inserted.append(candidate(server, req))
                raise
    def scandir(path):
        return Scan(path) if isinstance(path, int) else original(path)
    monkeypatch.setattr(sync.os, 'scandir', scandir)
    # The just-completed absent scan may miss this insertion, but cannot
    # return its response without the independent authoritative validator.
    assert server._try_local_closure_axioms_cache_hit(req) is None
    assert len(inserted) == 1
    assert server._try_local_closure_axioms_cache_hit(req)[2]['local_closure_axioms_cache_hit']
