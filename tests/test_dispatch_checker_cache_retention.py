"""Only whole-task-terminal private host observation caches are disposable."""
import json
import os
from contextlib import contextmanager

import pytest

from test_dispatch_terminal_retention import setup, put, git
from trellis.dispatch import checker_cache_retention as scope, storage
from trellis.checker import sync


KEY = 'a' * 64

def cache(attempt):
    state = attempt / 'runtime/checker-state'
    return [put(state / name, {'derived': name}) for name in (
        'sync-fingerprints.json', f'semantic-payloads/{KEY}.json',
        f'print-axioms-cache/{KEY}.json',
        f'local-closure-axioms-cache/{KEY}-principal-{KEY}-premises-{KEY}-noax.json')]


def test_exact_host_scope_preserves_evidence_and_restarts_after_partial_retirement(setup):
    collector, directory, observed, _, _ = setup
    collector.retire(directory, observed, apply=True)
    paths = cache(directory / 'acceptance/1') + cache(directory / 'acceptance/1/finalization/1')
    keep = cache(directory) + cache(directory / 'acceptance/diagnostic')
    state = directory / 'acceptance/1/runtime/checker-state'
    keep += [put(state / name, {'evidence': name}) for name in (
        'server.log', f'statement-transforms/{KEY}.json', 'semantic-payloads/evidence.json',
        f'semantic-payloads/{KEY}.json.saved', f'unknown-cache/{KEY}.json')]
    keep += [put(directory / name, {'evidence': name}) for name in (
        'repo/Tablet/A.lean', 'repo/.trellis/runtime/runtime/staging/7.raw.json',
        'acceptance/1/canonical.json', 'acceptance/1/execution/response.json')]
    before = {p: p.read_bytes() for p in keep}
    size = sum(p.stat().st_blocks * 512 for p in paths)
    first = collector.retire_checker_caches(directory, observed, apply=True, budget={'files': 3})
    second = collector.retire_checker_caches(directory, observed, apply=True)
    assert first['removed'] == 3 and second['removed'] == 5
    assert first['removed_allocated_bytes'] + second['removed_allocated_bytes'] == size
    assert all(not p.exists() for p in paths)
    assert all(p.read_bytes() == data for p, data in before.items())
    assert collector.retire_checker_caches(directory, observed, apply=True)['status'] == 'absent'


@pytest.mark.parametrize('guard', ['active', 'identity', 'pin', 'live', 'cleanup',
    'parent_symlink', 'file_symlink', 'hardlink', 'tracked', 'publication', 'foreign_project'])
def test_whole_owner_guards_preserve_ready_sibling_round(setup, monkeypatch, guard):
    collector, directory, observed, _, _ = setup
    paths = cache(directory / 'acceptance/1')
    put(directory / 'acceptance/1/completed.json', {'ready': True})
    if guard == 'active': observed = {}
    elif guard == 'identity': observed['1'] = 'different'
    elif guard == 'pin': put(directory / 'acceptance/1/retain-builds.json', {})
    elif guard in ('live', 'cleanup'):
        put(directory / 'acceptance/1/execution/process.json', {'pid': 123})
        put(directory / 'acceptance/1/execution/cleanup.json', {'pid': 123 if guard == 'live' else 456})
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: guard == 'live')
    elif guard == 'parent_symlink':
        parent = paths[1].parent
        parent.rename(parent.with_name('saved-cache')); parent.symlink_to('saved-cache')
    elif guard == 'file_symlink':
        paths[0].rename(paths[0].with_name('saved')); paths[0].symlink_to('saved')
    elif guard == 'hardlink': os.link(paths[0], directory / 'alias')
    elif guard == 'tracked':
        git(directory, 'init', '-q'); git(directory, 'add', str(paths[0].relative_to(directory)))
    elif guard == 'publication':
        identity = 'b' * 64
        put(collector.root / 'publication/pending.json', {'identity': identity})
        put(collector.root / 'publication' / identity / 'record.json', {'task_id': 1})
    elif guard == 'foreign_project':
        prepared = directory / 'prepared.json'; value = json.loads(prepared.read_text())
        value['source_repo'] = str(directory); put(prepared, value)
    if guard in ('pin', 'live', 'cleanup', 'parent_symlink', 'file_symlink', 'hardlink', 'foreign_project'):
        with pytest.raises(ValueError): collector.retire_checker_caches(directory, observed, apply=True)
    else:
        result = collector.retire_checker_caches(directory, observed, apply=True)
        assert result['removed'] == (3 if guard == 'tracked' else 0)
    assert paths[0].exists()


def test_changed_cache_after_queue_is_retained(setup, monkeypatch):
    collector, directory, observed, _, _ = setup
    paths = cache(directory / 'acceptance/1')
    @contextmanager
    def lock():
        paths[0].write_text('new observation after inventory')
        yield True
    monkeypatch.setattr(collector, 'retirement_lock', lock)
    with pytest.raises(ValueError, match='changed'):
        collector.retire_checker_caches(directory, observed, apply=True)
    assert all(p.exists() for p in paths)


def test_retirement_receipt_cannot_target_logs(setup):
    collector, directory, observed, _, _ = setup
    cache(directory / 'acceptance/1')
    log = put(directory / 'acceptance/1/runtime/checker-state/server.log', {})
    collector.retire_checker_caches(directory, observed, apply=True, budget={'files': 0})
    receipt = directory / scope.RECEIPT; value = json.loads(receipt.read_text())
    value['files'][0]['path'] = str(log.relative_to(directory)); put(receipt, value)
    with pytest.raises(ValueError, match='outside its allowlist'):
        collector.retire_checker_caches(directory, observed, apply=True)
    assert log.exists()


def test_real_cache_readers_return_cold_miss_after_retirement(setup):
    collector, directory, observed, _, _ = setup
    state = directory / 'acceptance/1/runtime/checker-state'
    payload = state / 'semantic-payloads'
    sync.store_semantic_payload(payload, KEY, node_name='A', payload='derived', ok=True,
                               error='', cache_version=sync.SEMANTIC_PAYLOAD_CACHE_VERSION)
    closure = state / 'local-closure-axioms-cache'
    sync.store_local_closure_axioms(closure, KEY, node_name='A', response={'status': 'ok'},
                                   cache_version=sync.LOCAL_CLOSURE_AXIOMS_CACHE_VERSION)
    transforms = state / 'statement-transforms'
    sync.store_local_closure_axioms(transforms, KEY, node_name='A', response={'status': 'ok'},
                                   cache_version=1)
    assert sync.load_semantic_payload(payload, KEY, expected_version=sync.SEMANTIC_PAYLOAD_CACHE_VERSION)
    assert sync.load_local_closure_axioms(closure, KEY, expected_version=sync.LOCAL_CLOSURE_AXIOMS_CACHE_VERSION)
    assert collector.retire_checker_caches(directory, observed, apply=True)['removed'] == 2
    assert sync.load_semantic_payload(payload, KEY, expected_version=sync.SEMANTIC_PAYLOAD_CACHE_VERSION) is None
    assert sync.load_local_closure_axioms(closure, KEY, expected_version=sync.LOCAL_CLOSURE_AXIOMS_CACHE_VERSION) is None
    from trellis.dispatch.workspace import _seed_statement_transforms
    destination = directory / 'future-runtime'
    assert _seed_statement_transforms(state.parent, destination) == 1
    assert sync.load_local_closure_axioms(destination / 'checker-state/statement-transforms',
                                        KEY, expected_version=1)['response'] == {'status': 'ok'}


@pytest.mark.parametrize('name', ['review-3', 'verifier-2'])
def test_nonworker_owner_paths_are_outside_this_scope(tmp_path, name):
    owner = tmp_path / name
    paths = cache(owner / 'acceptance/1')
    assert list(scope.eligible_paths(owner)) == []
    assert all(path.exists() for path in paths)


def test_background_collects_all_host_caches_and_preserves_canonical(setup):
    original, directory, _, _, state = setup
    paths = cache(directory / 'acceptance/1') + cache(directory / 'acceptance/1/finalization/1')
    canonical = [put(original.root.parent / 'checker-state' / name, {}) for name in (
        'sync-fingerprints.json', f'semantic-payloads/{KEY}.json',
        f'local-closure-axioms-cache/{KEY}.json')]
    collector = storage.BackgroundCollector(original.root, repo=original.repo)
    collector._review_snapshots = None
    try:
        assert collector.schedule(state) and collector.wait_idle(30)
        assert collector.summary['checker_cache_files'] == 8 and collector.summary['errors'] == 0
        assert not any(p.exists() for p in paths)
        assert all(p.exists() for p in canonical)
    finally:
        collector.close()
