import json
import os
from contextlib import contextmanager

import pytest

from test_dispatch_terminal_retention import setup, put, git
from test_dispatch_nonworker_retention import owner_setup
from trellis.dispatch import compiled_probe_retention as probes
from trellis.dispatch import storage


def cache(directory, kind='closure', *, repo='repo'):
    prefix = 'TrellisClosure' if kind == 'closure' else 'TrellisSemantic'
    key = 'a' * 64
    root = directory / repo / f'.trellis/{kind}-collectors/compiled-v1' / key
    return [put(root / name, {'generated': name}) for name in (
        f'{prefix}_{key}.lean', f'{prefix}_{key}.olean', f'{prefix}_{key}.olean.server',
        f'{prefix}_{key}.olean.private', 'RunClosure.lean' if kind == 'closure' else 'RunSemantic.lean',
        'receipt.json')]


def test_compiled_scope_exact_allowlist_and_independent_restart(setup):
    collector, directory, observed, _, _ = setup
    collector.retire(directory, observed, apply=True)
    paths = cache(directory) + cache(directory, 'semantic', repo='repo/.trellis/supervisor/repo')
    keep = [put(directory / name, {}) for name in (
        'repo/.trellis/closure-collectors/' + 'a' * 64 + '.lean',
        'repo/.trellis/closure-collectors/compiled-v1/' + 'a' * 64 + '/unique.txt',
        'repo/.trellis/closure-collectors/compiled-v1/unknown-key/receipt.json',
        'runtime/checker-state/local-closure-axioms-cache/evidence.json',
        'repo/.trellis/stuck-math-audit/findings.md')]
    size = sum(p.stat().st_blocks * 512 for p in paths)
    first = collector.retire_compiled_probes(directory, observed, apply=True, budget={'files': 6})
    assert first['removed'] == 6 and not paths[5].exists()  # cache receipt gone
    second = collector.retire_compiled_probes(directory, observed, apply=True)
    assert second['removed'] == 6
    assert first['removed_allocated_bytes'] + second['removed_allocated_bytes'] == size
    assert not any(p.exists() for p in paths) and all(p.exists() for p in keep)
    assert collector.retire_compiled_probes(directory, observed, apply=True)['status'] == 'absent'


def test_compiled_typed_owners(owner_setup):
    collector, directory, observed, _, _ = owner_setup
    paths = cache(directory)
    assert collector.retire_compiled_probes(directory, observed, apply=True)['removed'] == 6
    assert not any(path.exists() for path in paths)
    assert json.loads((directory / probes.RECEIPT).read_text())['format'] == probes.OWNER_FORMAT


@pytest.mark.parametrize('guard', ['active', 'identity', 'pin', 'live', 'cleanup', 'parent_symlink', 'file_symlink', 'hardlink', 'tracked', 'publication'])
def test_compiled_cache_guards(setup, monkeypatch, guard):
    collector, directory, observed, _, _ = setup
    paths = cache(directory)
    if guard == 'active': observed = {}
    elif guard == 'identity': observed['1'] = 'different'
    elif guard == 'pin': put(directory / 'retain-builds.json', {})
    elif guard in ('live', 'cleanup'):
        put(directory / 'execution/process.json', {'pid': 123})
        put(directory / 'execution/cleanup.json', {'pid': 123 if guard == 'live' else 456})
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: guard == 'live')
    elif guard == 'parent_symlink':
        root = paths[0].parent
        root.rename(root.with_name('saved-cache'))
        root.symlink_to(root.with_name('saved-cache'))
    elif guard == 'file_symlink':
        paths[0].rename(paths[0].with_name('saved'))
        paths[0].symlink_to('saved')
    elif guard == 'hardlink': os.link(paths[0], directory / 'alias')
    elif guard == 'tracked':
        git(directory, 'init', '-q')
        git(directory, 'add', str(paths[0].relative_to(directory)))
    elif guard == 'publication':
        identity = 'b' * 64
        put(collector.root / 'publication/pending.json', {'identity': identity})
        put(collector.root / 'publication' / identity / 'record.json', {'task_id': 1})
    if guard in ('pin', 'live', 'cleanup', 'parent_symlink', 'file_symlink', 'hardlink'):
        with pytest.raises(ValueError): collector.retire_compiled_probes(directory, observed, apply=True)
    else:
        result = collector.retire_compiled_probes(directory, observed, apply=True)
        assert result['removed'] == (5 if guard == 'tracked' else 0)
    assert paths[0].exists()


def test_compiled_cache_rechecks_signature_after_queue(setup, monkeypatch):
    collector, directory, observed, _, _ = setup
    paths = cache(directory)
    @contextmanager
    def lock():
        paths[0].write_text('changed after inventory')
        yield True
    monkeypatch.setattr(collector, 'retirement_lock', lock)
    with pytest.raises(ValueError, match='changed'):
        collector.retire_compiled_probes(directory, observed, apply=True)
    assert all(path.exists() for path in paths)


def test_compiled_receipt_cannot_delete_pinned_collector_source(setup):
    collector, directory, observed, _, _ = setup
    paths = cache(directory)
    source = put(directory / ('repo/.trellis/closure-collectors/' + 'a' * 64 + '.lean'), {})
    collector.retire_compiled_probes(directory, observed, apply=True, budget={'files': 0})
    record = json.loads((directory / probes.RECEIPT).read_text())
    record['files'][0]['path'] = str(source.relative_to(directory))
    put(directory / probes.RECEIPT, record)
    with pytest.raises(ValueError, match='outside its allowlist'):
        collector.retire_compiled_probes(directory, observed, apply=True)
    assert source.exists() and all(path.exists() for path in paths)


def test_background_collects_all_compiled_files(setup):
    original, directory, _, _, state = setup
    paths = cache(directory) + cache(directory / 'acceptance/1', 'semantic')
    collector = storage.BackgroundCollector(original.root, repo=original.repo)
    collector._review_snapshots = None
    assert collector.schedule(state) and collector.wait_idle(15)
    assert collector.summary['compiled_probe_files'] == 12 and collector.summary['errors'] == 0
    assert not any(path.exists() for path in paths)
