import os
from pathlib import Path
import subprocess

import pytest

from trellis.dispatch import retained_files as retained


def git(path, *args, **kwargs):
    return subprocess.run(['git', '-C', str(path), *args], check=True,
                          capture_output=True, **kwargs)


@pytest.fixture
def external(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith('GIT_'):
            monkeypatch.delenv(key)
    root = tmp_path / 'runtime' / 'dispatch'
    path = root / 'batch-caches' / '23' / ('a' * 64) / 'repo' / 'Tablet' / 'A.lean'
    path.parent.mkdir(parents=True)
    path.write_text('evidence')
    return root, path


def test_external_scopes_do_not_spawn_git(external, monkeypatch):
    root, path = external
    pub = root / 'publication' / ('b' * 64) / 'artifacts' / 'data'
    pub.parent.mkdir(parents=True)
    pub.write_bytes(b'cache')
    monkeypatch.setattr(retained, '_untracked', lambda p: pytest.fail('unnecessary Git'))
    assert retained._untracked_cache(path, root)
    assert retained._untracked_cache(pub, root)
    assert retained._untracked_cache(pub.parent / 'future-marker', root)


@pytest.mark.parametrize('kind', ['nested', 'outer', 'gitfile', 'worktree', 'bare', 'environment'])
def test_actual_git_layouts_keep_tracked_or_error_behavior(external, tmp_path, monkeypatch, kind):
    root, path = external
    repo = path.parent if kind != 'outer' else tmp_path
    if kind == 'bare':
        git(repo, 'init', '--bare')
        with pytest.raises(ValueError, match='Git ownership'):
            retained._untracked_cache(path, root)
        return
    git(repo, 'init')
    if kind == 'gitfile':
        metadata = tmp_path / 'private-git'
        (repo / '.git').rename(metadata)
        (repo / '.git').write_text('gitdir: ' + str(metadata) + '\n')
    elif kind == 'worktree':
        git(repo, 'add', '.')
        git(repo, '-c', 'user.name=Test', '-c', 'user.email=test@example.com', 'commit', '-m', 'initial')
        target = root / 'publication' / ('c' * 64)
        git(repo, 'worktree', 'add', '-b', 'linked', str(target))
        path = target / 'A.lean'
    elif kind == 'environment':
        metadata = tmp_path / 'environment-git'
        (repo / '.git').rename(metadata)
        monkeypatch.setenv('GIT_DIR', str(metadata))
        monkeypatch.setenv('GIT_WORK_TREE', str(repo))
    git(repo if kind != 'worktree' else path.parent, 'add', str(path))
    assert retained._untracked_cache(path, root) is False
    untracked = path.with_name('untracked')
    untracked.write_text('unique')
    assert retained._untracked_cache(untracked, root) is True


@pytest.mark.parametrize('marker', ['.git', 'HEAD', 'objects', 'refs'])
def test_any_marker_arriving_between_calls_forces_git(external, monkeypatch, marker):
    root, path = external
    assert retained._untracked_cache(path, root)
    seen = []
    monkeypatch.setattr(retained, '_untracked', lambda p: seen.append(p) or False)
    (root / marker).write_text('even an incomplete layout is conservative')
    assert retained._untracked_cache(path, root) is False
    assert seen == [path]


def test_real_repository_added_after_initial_negative_is_rechecked(external):
    root, path = external
    assert retained._untracked_cache(path, root)
    git(root, 'init')
    git(root, 'add', str(path))
    assert retained._untracked_cache(path, root) is False


@pytest.mark.parametrize('change', ['alias', 'broken_gitfile', 'env', 'error', 'outside', 'invalid_hash'])
def test_uncertain_or_out_of_scope_paths_use_original_probe(external, monkeypatch, change):
    root, path = external
    if change == 'alias':
        original = path.parent.with_name('original')
        path.parent.rename(original)
        path.parent.symlink_to(original, target_is_directory=True)
    elif change == 'broken_gitfile':
        (root / '.git').symlink_to(root / 'missing')
    elif change == 'env':
        monkeypatch.setenv('GIT_TRACE', '0')
    elif change == 'error':
        original = os.stat
        def denied(p, *args, **kwargs):
            if p == '.git' and 'dir_fd' in kwargs:
                raise PermissionError('uncertain metadata')
            return original(p, *args, **kwargs)
        monkeypatch.setattr(os, 'stat', denied)
    elif change == 'outside':
        path = root / 'worker' / 'data'
    else:
        path = root / 'publication' / 'not-a-digest' / 'data'
    seen = []
    monkeypatch.setattr(retained, '_untracked', lambda p: seen.append(p) or False)
    assert retained._untracked_cache(path, root) is False
    assert seen == [path]
