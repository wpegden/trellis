"""The original delivery directories guard the existing Git commit boundary."""
from contextlib import ExitStack
import copy
import os
from pathlib import Path

import pytest

from trellis.agents.host_files import safe_directory
from trellis.runtime import git_checkpoint_hook as hook
from test_git_checkpoint_hook_trust_decisions import _git, _init_repo, _payload


@pytest.fixture
def checkpoint(tmp_path):
    repo = _init_repo(tmp_path)
    view = tmp_path / 'delivery/view'
    (view / '.trellis').mkdir(parents=True)
    with ExitStack() as stack:
        roots = []
        for path in (view, view / '.trellis'):
            _, fd = stack.enter_context(safe_directory(path, create=False))
            info = os.fstat(fd)
            roots.append(dict(path=str(path), fd=fd, device=info.st_dev, inode=info.st_ino))
        payload = _payload(repo, tmp_path)
        consumption = dict(request_id=1, turn=1, request_sha256='1'*64,
                           candidate_sha256='2'*64, checked_sha256='3'*64)
        payload['state'] = dict(phase='Engineering', stage='Engineering',
                               engineering=dict(last_consumption={**consumption, 'predecessor_sha256': None}))
        payload['engineering_guard'] = {**consumption, 'roots': roots}
        yield repo, payload, view


def test_original_roots_allow_one_commit_and_exact_reconciliation(checkpoint):
    repo, payload, view = checkpoint
    committed = hook.commit_checkpoint(payload)
    count = _git(repo, 'rev-list', '--count', 'HEAD')
    view.rename(view.with_name('previous'))
    (view / '.trellis').mkdir(parents=True)
    assert hook.commit_checkpoint(payload) == committed
    assert _git(repo, 'rev-list', '--count', 'HEAD') == count


def test_native_checkpoint_retains_witness_despite_preparation_ignore_rules(checkpoint):
    import json
    repo, payload, _ = checkpoint
    (repo / '.gitignore').write_text('.trellis-history/\n')
    hook.commit_checkpoint(payload)
    document = json.loads(_git(repo, 'show', 'HEAD:.trellis-history/supervisor_state.json'))
    assert hook.decode_shared_state(document) == hook._checkpoint_document(payload)


@pytest.mark.parametrize('change', ['missing', 'replacement', 'alias', 'fd', 'binding', 'bookkeeping'])
def test_invalid_original_roots_refuse_commit(checkpoint, change):
    repo, payload, view = checkpoint
    before = _git(repo, 'rev-parse', 'HEAD')
    if change == 'missing': payload.pop('engineering_guard')
    elif change == 'fd': payload['engineering_guard']['roots'][0]['fd'] = 987654
    elif change == 'binding': payload['engineering_guard']['checked_sha256'] = '4'*64
    else:
        path = view / '.trellis' if change == 'bookkeeping' else view
        prior = path.with_name(path.name + '-old')
        path.rename(prior)
        if change == 'alias': path.symlink_to(prior)
        else: path.mkdir()
    with pytest.raises((RuntimeError, ValueError, OSError)):
        hook.commit_checkpoint(payload)
    assert _git(repo, 'rev-parse', 'HEAD') == before


def test_replacement_during_staging_is_checked_at_commit(checkpoint, monkeypatch):
    repo, payload, view = checkpoint
    before = _git(repo, 'rev-parse', 'HEAD')
    protect = hook._protect_dispatch_objects
    def replace(root):
        protect(root)
        view.rename(view.with_name('old-view'))
        (view / '.trellis').mkdir(parents=True)
    monkeypatch.setattr(hook, '_protect_dispatch_objects', replace)
    with pytest.raises(RuntimeError, match='directory changed'):
        hook.commit_checkpoint(payload)
    assert _git(repo, 'rev-parse', 'HEAD') == before


def test_empty_diff_is_not_an_engineering_witness(checkpoint, monkeypatch):
    repo, payload, _ = checkpoint
    monkeypatch.setattr(hook, '_write_canonical_history', lambda *args: None)
    with pytest.raises(RuntimeError, match='no exact committed witness'):
        hook.commit_checkpoint(payload)


def test_other_checkpoint_cannot_reconcile_after_root_loss(checkpoint):
    repo, payload, view = checkpoint
    hook.commit_checkpoint(payload)
    changed = copy.deepcopy(payload)
    changed['event_count'] += 1
    view.rename(view.with_name('old-view'))
    with pytest.raises((RuntimeError, ValueError, OSError)):
        hook.commit_checkpoint(changed)
