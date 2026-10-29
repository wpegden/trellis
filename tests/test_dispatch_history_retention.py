"""Terminal indexes are dispensable; transcripts and advisory notes are not."""
import json
import os
from pathlib import Path
from contextlib import contextmanager

import pytest

from test_dispatch_terminal_retention import setup, put, git
from test_dispatch_nonworker_retention import owner_setup
from trellis.dispatch import history_retention as history
from trellis.dispatch import storage


def indexes(directory):
    return [put(directory / root / '.trellis/logs/burst-history.jsonl', {'copied': True})
            for root in ('repo', 'repo/.trellis/supervisor/repo')]


def test_new_history_scope_revisits_previously_retired_inputs(setup):
    collector, directory, observed, _, _ = setup
    collector.retire(directory, observed, apply=True)
    put(directory / 'dispatch-request.json', {})
    collector.retire_execution(directory, observed, apply=True)
    paths = indexes(directory)
    keep = [put(directory / name, {'preserve': name}) for name in (
        'repo/.trellis/logs/unrelated.jsonl', 'repo/.trellis/stuck-math-audit/old.md',
        'repo/.trellis/scratch/decision.md', 'repo/.trellis/chats/live/prompt.txt')]
    canonical = put(collector.repo / '.trellis/logs/burst-history.jsonl', {'canonical': True})
    expected = sum(path.stat().st_blocks * 512 for path in paths)
    assert collector.retire_history(directory, observed)['files'] == 2
    result = collector.retire_history(directory, observed, apply=True)
    assert result['removed'] == 2 and result['removed_allocated_bytes'] == expected
    assert not any(path.exists() for path in paths)
    assert all(path.exists() for path in (*keep, canonical))
    assert json.loads((directory / history.RECEIPT).read_text())['format'] == history.FORMAT
    assert collector.retire_history(directory, observed, apply=True)['status'] == 'absent'


def test_history_all_known_attempts_only(setup):
    collector, directory, observed, _, _ = setup
    paths = []
    for suffix in ('', 'turns/1', 'turns/1/transports/2', 'acceptance/1', 'acceptance/1/finalization/1'):
        paths += indexes(directory / suffix)
    unknown = indexes(directory / 'arbitrary-child')
    assert collector.retire_history(directory, observed, apply=True)['removed'] == 10
    assert not any(path.exists() for path in paths)
    assert all(path.exists() for path in unknown)


def test_history_typed_owner_and_partial_restart(owner_setup):
    collector, directory, observed, _, _ = owner_setup
    paths = indexes(directory)
    assert collector.retire_history(directory, observed, apply=True, budget={'files': 1})['removed'] == 1
    assert collector.retire_history(directory, observed, apply=True)['removed'] == 1
    assert not any(path.exists() for path in paths)
    assert json.loads((directory / history.RECEIPT).read_text())['format'] == history.OWNER_FORMAT


@pytest.mark.parametrize('guard', ['active', 'identity', 'pin', 'live', 'cleanup', 'symlink', 'hardlink', 'tracked', 'publication'])
def test_history_preserves_existing_guards(setup, monkeypatch, guard):
    collector, directory, observed, _, _ = setup
    paths = indexes(directory)
    if guard == 'active': observed = {}
    elif guard == 'identity': observed['1'] = 'different'
    elif guard == 'pin': put(directory / 'retain-builds.json', {})
    elif guard in ('live', 'cleanup'):
        put(directory / 'execution/process.json', {'pid': 123})
        put(directory / 'execution/cleanup.json', {'pid': 123 if guard == 'live' else 456})
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: guard == 'live')
    elif guard == 'symlink':
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
    if guard in ('pin', 'live', 'cleanup', 'symlink', 'hardlink'):
        with pytest.raises(ValueError): collector.retire_history(directory, observed, apply=True)
    else:
        result = collector.retire_history(directory, observed, apply=True)
        assert result['removed'] == (1 if guard == 'tracked' else 0)
    assert paths[0].exists()


@pytest.mark.parametrize('change', ['bytes', 'pin'])
def test_history_rechecks_after_queue(setup, monkeypatch, change):
    collector, directory, observed, _, _ = setup
    paths = indexes(directory)
    @contextmanager
    def lock():
        if change == 'bytes': paths[0].write_text('changed after inventory')
        else: put(directory / 'retain-builds.json', {})
        yield True
    monkeypatch.setattr(collector, 'retirement_lock', lock)
    with pytest.raises(ValueError): collector.retire_history(directory, observed, apply=True)
    assert all(path.exists() for path in paths)


def test_history_receipt_cannot_redirect_to_advisory(setup):
    collector, directory, observed, _, _ = setup
    paths = indexes(directory)
    keep = put(directory / 'repo/.trellis/scratch/keep.md', {})
    collector.retire_history(directory, observed, apply=True, budget={'files': 0})
    record = json.loads((directory / history.RECEIPT).read_text())
    record['files'][0]['path'] = str(keep.relative_to(directory))
    put(directory / history.RECEIPT, record)
    with pytest.raises(ValueError, match='outside its allowlist'):
        collector.retire_history(directory, observed, apply=True)
    assert keep.exists() and all(path.exists() for path in paths)


def test_background_retires_history_without_a_new_cleanup_budget(setup):
    original, directory, _, _, state = setup
    paths = indexes(directory)
    collector = storage.BackgroundCollector(original.root, repo=original.repo)
    collector._review_snapshots = None
    assert collector.schedule(state) and collector.wait_idle(15)
    assert collector.summary['history_indexes'] == 2 and collector.summary['errors'] == 0
    assert not any(path.exists() for path in paths)


def test_future_workspace_history_and_transcripts_survive_retirement(setup, monkeypatch):
    from test_dispatch_transcripts import completed
    from trellis.dispatch.candidate import source_manifest
    from trellis.dispatch.workspace import prepare
    collector, directory, observed, _, _ = setup
    saved = json.loads((directory / 'prepared.json').read_text())
    _, chat = completed(collector.root.parent, '1', 7)
    put(directory / 'prepared.json', saved)
    monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: False)
    old_chat = (chat / 'output.log').read_bytes()
    paths = indexes(directory)
    assert collector.retire_history(directory, observed, apply=True)['removed'] == 2
    config = put(collector.repo / 'trellis.config.json', {'repo_path': str(collector.repo)})
    put(collector.root.parent / 'runtime_metadata.json', {'repo_path': str(collector.repo), 'config_path': str(config)})
    canonical = put(collector.repo / '.trellis/logs/burst-history.jsonl', {'request_id': 6})
    canonical.write_bytes(canonical.read_bytes() + b'\n')
    canonical_before = canonical.read_bytes()
    target = collector.root / 'review-8'
    prepare(collector.root.parent, collector.repo, config, target,
            {'in_flight_request': {'id': 8, 'kind': 'Review'}},
            expected_manifest=source_manifest(collector.repo), seed_packages=False)
    rows = [json.loads(line) for line in (target / 'repo/.trellis/logs/burst-history.jsonl').read_text().splitlines()]
    assert [row['request_id'] for row in rows] == [6, 7]
    published = target / 'repo' / rows[1]['chats'][0] / 'output.log'
    assert published.read_bytes() == old_chat and canonical.read_bytes() == canonical_before
    assert not any(path.exists() for path in paths)
