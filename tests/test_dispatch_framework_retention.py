import json
import os
from pathlib import Path
import shutil

import pytest

from test_dispatch_terminal_retention import setup, put, git
from trellis.dispatch import framework_retention as framework


@pytest.fixture
def trees(setup):
    collector, directory, observed, _, _ = setup
    for name in ('src/trellis/main.py', 'src/kernel/a.rs', 'skills/test/SKILL.md',
                 'bin/trellis_runtime_cli', 'runtime/staging/result.raw.json'):
        put(directory / 'repo/.trellis/runtime' / name, {'file': name})
    return collector, directory, observed


def test_generated_trees_drain_with_diagnostics_and_binary_preserved(trees):
    collector, directory, observed = trees
    root = directory / 'repo/.trellis/runtime'
    before = {name: (root / name).read_bytes() for name in ('bin/trellis_runtime_cli', 'runtime/staging/result.raw.json')}
    dry = framework.retire(collector, directory, observed)
    assert dry['files'] == 3
    assert framework.retire(collector, directory, observed, apply=True)['removed'] == 2
    assert not any((root / name).exists() for name in ('src', 'skills'))
    assert framework.retire(collector, directory, observed, apply=True)['removed'] == 0
    assert all((root / name).read_bytes() == value for name, value in before.items())


@pytest.mark.parametrize('unsafe', ['symlink', 'hardlink', 'nested_git', 'large', 'pin', 'live'])
def test_framework_guards_keep_data(trees, monkeypatch, unsafe):
    collector, directory, observed = trees
    root = directory / 'repo/.trellis/runtime'
    if unsafe == 'symlink':
        (root / 'src/link').symlink_to(collector.repo)
    elif unsafe == 'hardlink':
        os.link(root / 'src/trellis/main.py', root / 'src/alias')
    elif unsafe == 'nested_git':
        put(root / 'src/nested/.git/config', {})
    elif unsafe == 'large':
        monkeypatch.setattr(framework, 'MAX_ENTRIES', 1)
    elif unsafe == 'pin':
        put(directory / 'retain-builds.json', {})
    elif unsafe == 'live':
        put(directory / 'execution/process.json', dict(pid=1))
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: True)
    with pytest.raises((ValueError, FileNotFoundError)):
        framework.retire(collector, directory, observed, apply=True)
    assert (root / 'src/trellis/main.py').exists()


def test_indexed_descendant_preserves_entire_tree(trees):
    collector, directory, observed = trees
    git(directory, 'init', '-q')
    git(directory, 'add', 'repo/.trellis/runtime/src/trellis/main.py')
    result = framework.retire(collector, directory, observed, apply=True)
    assert result['removed'] == 1
    root = directory / 'repo/.trellis/runtime'
    assert (root / 'src/kernel/a.rs').exists()
    assert not (root / 'skills').exists()


@pytest.mark.parametrize('after_partial', [False, True])
def test_interrupted_renamed_tree_removal_resumes(trees, monkeypatch, after_partial):
    collector, directory, observed = trees
    original = shutil.rmtree
    def interrupt(path):
        if after_partial:
            next(p for p in Path(path).rglob('*') if p.is_file()).unlink()
        raise RuntimeError('injected interrupted removal')
    with monkeypatch.context() as patch:
        patch.setattr(framework.shutil, 'rmtree', interrupt)
        with pytest.raises(RuntimeError, match='injected'):
            framework.retire(collector, directory, observed, apply=True)
    assert list((directory / 'repo/.trellis/runtime').glob('.*.terminal-retiring'))
    assert framework.retire(collector, directory, observed, apply=True)['removed'] == 2
    assert not list((directory / 'repo/.trellis/runtime').glob('.*.terminal-retiring'))


def test_changed_tree_kept_after_inventory(trees):
    collector, directory, observed = trees
    framework.retire(collector, directory, observed, apply=True, budget={'files': 0})
    put(directory / 'repo/.trellis/runtime/skills/new.txt', {})
    with pytest.raises(ValueError, match='changed after'):
        framework.retire(collector, directory, observed, apply=True)


def test_framework_receipt_cannot_name_other_paths(trees):
    collector, directory, observed = trees
    framework.retire(collector, directory, observed, apply=True, budget={'files': 0})
    receipt = directory / framework.RECEIPT
    value = json.loads(receipt.read_text())
    value['trees']['repo/Tablet'] = next(iter(value['trees'].values()))
    put(receipt, value)
    with pytest.raises(ValueError, match='allowlist'):
        framework.retire(collector, directory, observed, apply=True)


def test_framework_requires_matching_committed_terminal(trees):
    collector, directory, observed = trees
    assert framework.retire(collector, directory, {}, apply=True)['removed'] == 0
    observed['1']['sha256'] = 'wrong'
    assert framework.retire(collector, directory, observed, apply=True)['status'] == 'not_committed'


def test_nested_supervisor_framework_and_real_bulk_cli(trees):
    import subprocess
    import sys
    collector, directory, observed = trees
    nested = directory / 'acceptance/1/repo/.trellis/supervisor/repo/.trellis/runtime/src/trellis/check.py'
    put(nested, {})
    output = subprocess.check_output([sys.executable, '-m', 'trellis.dispatch.framework_retention',
        '--runtime', str(collector.root.parent), '--repo', str(collector.repo), '--task', '1', '--apply'])
    result = json.loads(output)
    assert result['removed'] == 3 and result['removed_bytes'] > 0
    assert not nested.exists()
    assert (directory / 'repo/.trellis/runtime/bin/trellis_runtime_cli').exists()


def test_pin_installed_after_rename_keeps_tombstone_and_can_resume(trees, monkeypatch):
    collector, directory, observed = trees
    original = framework._sync_dir
    def pin(parent):
        original(parent)
        if list(parent.glob('.*.terminal-retiring')):
            put(directory / 'retain-builds.json', {'why': 'operator investigation'})
    with monkeypatch.context() as patch:
        patch.setattr(framework, '_sync_dir', pin)
        with pytest.raises(ValueError, match='operator pin'):
            framework.retire(collector, directory, observed, apply=True)
    trash = list((directory / 'repo/.trellis/runtime').glob('.*.terminal-retiring'))
    assert len(trash) == 1 and list(trash[0].rglob('*'))
    (directory / 'retain-builds.json').unlink()
    assert framework.retire(collector, directory, observed, apply=True)['removed'] == 2
