"""Lexical optimization must preserve capture labels and live path evidence."""
from pathlib import Path
import itertools

import pytest

from trellis.checker import portable_replay as portable


def legacy_identity(self, raw, sysroot):
    path = self.resolve(raw)
    if path.is_relative_to(sysroot):
        return 'sysroot/' + path.relative_to(sysroot).as_posix()
    if path.is_relative_to(self.repo):
        return 'repo/' + path.relative_to(self.repo).as_posix()
    return 'external/' + str(path)


def test_lexical_prefix_matches_pathlib_including_dotdot_and_similar_prefixes():
    values = ['', '.', '/', '//', '/repo', '/repo/a', '/repo-other/a',
              '/repo/../outside', '/repo/a/./b', '/repo/λ/証明', 'repo',
              'repo/a', '../repo', 'a/../../b']
    for path, root in itertools.product(map(Path, values), repeat=2):
        assert (portable._relative_parts(path, root) is not None) == path.is_relative_to(root)
        try:
            expected = path.relative_to(root).parts
        except ValueError:
            expected = None
        assert portable._relative_parts(path, root) == expected


@pytest.mark.parametrize('relationship', ['separate', 'sysroot_in_repo', 'repo_in_sysroot'])
def test_identity_matches_legacy_labels_and_records_all_lexical_paths(tmp_path, relationship):
    repo, sysroot = tmp_path / 'repo', tmp_path / 'toolchain'
    if relationship == 'sysroot_in_repo': sysroot = repo / 'toolchain'
    if relationship == 'repo_in_sysroot': repo = sysroot / 'repo'
    repo.mkdir(parents=True, exist_ok=True); sysroot.mkdir(parents=True, exist_ok=True)
    (repo / 'child').mkdir()
    old, new = portable._CapturePaths(repo), portable._CapturePaths(repo)
    for raw in [repo, sysroot, repo / 'λ', repo / 'child/../missing',
                tmp_path / 'repo-other/x', tmp_path / 'external', 'child/../missing']:
        assert new.identity(raw, sysroot) == legacy_identity(old, raw, sysroot)
    assert new.states == old.states
    assert new.resolved == old.resolved
    assert new.initial_states == old.initial_states


def test_repeated_label_does_not_bypass_alias_or_negative_path_guards(tmp_path):
    repo = tmp_path / 'repo'; repo.mkdir(); (repo / 'child').mkdir()
    paths = portable._CapturePaths(repo)
    missing = repo / 'missing'
    assert paths.identity(missing, tmp_path / 'toolchain') == 'repo/missing'
    assert paths.identity(repo / 'child/../missing', tmp_path / 'toolchain') == 'repo/missing'
    assert repo / 'child/../missing' in paths.states
    (repo / 'alias').symlink_to(repo / 'child', target_is_directory=True)
    with pytest.raises(portable.Unavailable, match='alias'):
        paths.identity(repo / 'alias/../missing', tmp_path / 'toolchain')
    missing.write_text('newly present')
    with pytest.raises(portable.Unavailable, match='resolution changed'):
        paths.validate(portable.HashBudget())
