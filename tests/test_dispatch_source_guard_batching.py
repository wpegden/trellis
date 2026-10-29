"""Root process admission stays cheap without weakening per-file safeguards."""
from contextlib import contextmanager
import json
from pathlib import Path
import threading

import pytest

from test_dispatch_terminal_retention import setup, put, git
from test_dispatch_auxiliary_retention import auxiliary_setup, targets
from trellis.dispatch import auxiliary_retention as engine, retained_files as retained


def test_process_guard_runs_once_per_root_with_many_files(auxiliary_setup, monkeypatch):
    collector, directory, observed, _, _ = auxiliary_setup
    for number in range(24):
        name = f'process-memory/extra-{number}.md'
        value = f'Committed finding {number}\n'
        (collector.repo / name).write_text(value)
        for prefix in ('repo', 'repo/.trellis/supervisor/repo'):
            (directory / prefix / name).write_text(value)
    git(collector.repo, 'add', '.')
    git(collector.repo, 'commit', '-qm', 'Additional recoverable memory')
    prepared = json.loads((directory / 'prepared.json').read_text())
    prepared['source_head'] = git(collector.repo, 'rev-parse', 'HEAD')
    put(directory / 'prepared.json', prepared)
    original = retained.terminal_owner_guard
    calls = []
    @contextmanager
    def lease():
        calls.append(0)
        yield True
    def counted(*args, **kwargs):
        guard = original(*args, **kwargs)
        def check():
            if calls:
                calls[-1] += 1
            guard()
        return check
    monkeypatch.setattr(collector, 'retirement_lock', lease)
    monkeypatch.setattr(retained, 'terminal_owner_guard', counted)
    result = engine.retire(collector, directory, observed, apply=True)
    assert result['removed'] == 54
    # Four roots and completion. Completion has an extra off-lock guard, which
    # is recorded in the previous bucket; no root repeats work for its 26 files.
    assert len(calls) == 5 and max(calls) <= 2


@pytest.mark.parametrize('change', ['pin', 'review_pin', 'cancel', 'file', 'parent'])
def test_mid_root_changes_stop_before_next_unlink(auxiliary_setup, monkeypatch, change):
    collector, directory, observed, _, _ = auxiliary_setup
    if change == 'review_pin' and not directory.name.startswith('review-'):
        pytest.skip('review pin is only an owner guard for reviewers')
    first = directory / 'repo/process-memory/INDEX.md'
    following = directory / 'repo/process-memory/global/pm-1.md'
    original = Path.unlink
    def unlink(path, *args, **kwargs):
        result = original(path, *args, **kwargs)
        if path == first:
            if change == 'pin':
                put(directory / 'retain-builds.json', {})
            elif change == 'review_pin':
                put(collector.root / 'reviews/2/retain-state.json', {})
            elif change == 'cancel':
                collector.stop_event = threading.Event(); collector.stop_event.set()
            elif change == 'file':
                following.write_text('new private bytes after first unlink')
            else:
                parent = following.parent
                parent.rename(parent.with_name('saved-global'))
                parent.mkdir()
                (parent.parent / 'saved-global' / following.name).rename(following)
        return result
    monkeypatch.setattr(Path, 'unlink', unlink)
    if change == 'cancel':
        assert engine.retire(collector, directory, observed, apply=True)['status'] == 'cancelled'
    else:
        with pytest.raises(ValueError):
            engine.retire(collector, directory, observed, apply=True)
    assert not first.exists() and following.exists()


@pytest.mark.parametrize('change', ['prepared', 'cleanup'])
def test_next_root_revalidates_shared_admission(auxiliary_setup, monkeypatch, change):
    collector, directory, observed, _, _ = auxiliary_setup
    first = directory / 'repo/paper/paper.tex'
    following = directory / 'repo/process-memory/INDEX.md'
    if change == 'cleanup':
        put(directory / 'execution/process.json', {'pid': 987})
        put(directory / 'execution/cleanup.json', {'pid': 987})
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: False)
    original = Path.unlink
    def unlink(path, *args, **kwargs):
        result = original(path, *args, **kwargs)
        if path == first:
            if change == 'prepared':
                p = directory / 'prepared.json'; p.write_bytes(p.read_bytes() + b' ')
            else:
                put(directory / 'execution/cleanup.json', {'pid': 988})
        return result
    monkeypatch.setattr(Path, 'unlink', unlink)
    with pytest.raises(ValueError):
        engine.retire(collector, directory, observed, apply=True)
    assert not first.exists() and following.exists()
