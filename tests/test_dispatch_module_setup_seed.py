"""Generated Lake invocation inputs are not optional cached output artifacts."""
import json
import os
from pathlib import Path
import shutil
import subprocess

import pytest

from trellis.dispatch import batch_cache, candidate, workspace
from test_dispatch_workspace import project
from test_dispatch_batch_cache import make_batch


def setup_files(repo):
    for relative in ('.lake/build/ir/Tablet.setup.json',
                     '.lake/build/ir/Tablet/A.setup.json',
                     '.lake/build/lib/lean/keep.setup.json',
                     '.lake/config/ir/keep.setup.json'):
        path = repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(relative)


def assert_seed(source, target):
    for relative in ('ir/Tablet.setup.json', 'ir/Tablet/A.setup.json'):
        assert (source / '.lake/build' / relative).is_file()
        assert not (target / '.lake/build' / relative).exists()
    for relative in ('.lake/build/project.olean', '.lake/build/lib/lean/keep.setup.json',
                     '.lake/config/ir/keep.setup.json'):
        assert (target / relative).read_bytes() == (source / relative).read_bytes()


def test_direct_preparation_omits_only_generated_setup(project):
    repo, runtime, config = project
    setup_files(repo)
    target = runtime / 'dispatch/1'
    workspace.prepare(runtime, repo, config, target, {'in_flight_request': {'id': 7}},
                      expected_manifest=candidate.source_manifest(repo), seed_packages=False)
    assert_seed(repo, target / 'repo')


@pytest.mark.parametrize('old_capture', [False, True])
def test_batch_capture_and_hydration_compatible_with_old_setup_inventory(project, monkeypatch, old_capture):
    repo, _, _ = project
    setup_files(repo)
    driver, path, workers = make_batch(project)
    original = workspace._copy_tree
    if old_capture:
        def old_copy(*args, **kwargs):
            kwargs.pop('omit_module_setup', None)
            return original(*args, **kwargs)
        monkeypatch.setattr(workspace, '_copy_tree', old_copy)
    driver._materialize_batch(workers, path=path, committed=False)
    monkeypatch.setattr(workspace, '_copy_tree', original)
    cache = driver.root / 'batch-caches/7' / ('a' * 64)
    receipt = (cache / 'capture.json').read_bytes()
    assert (cache / 'repo/.lake/build/ir/Tablet.setup.json').exists() == old_capture
    driver._materialize_batch(workers, path=path, committed=True)
    driver._hydrate_worker_cache(dict(id=1))
    assert_seed(repo, driver.root / '1/repo')
    assert (cache / 'capture.json').read_bytes() == receipt
    batch_cache.validate(cache, json.loads(receipt))


def test_generic_copy_and_resumed_existing_setup_are_preserved(tmp_path):
    source = tmp_path / 'source'; setup = source / 'ir/A.setup.json'
    setup.parent.mkdir(parents=True); setup.write_text('original')
    destination = tmp_path / 'destination'
    workspace._copy_tree(source, destination)
    copied = destination / 'ir/A.setup.json'
    copied.write_text('private invocation')
    workspace._copy_tree(source, destination, clone_builds=True, reuse_builds=True,
                         omit_module_setup=True)
    assert copied.read_text() == 'private invocation'
    assert setup.read_text() == 'original'


def test_omitted_setup_still_rejects_alias(tmp_path):
    source = tmp_path / 'source'; (source / 'ir').mkdir(parents=True)
    (source / 'ir/A.setup.json').symlink_to(tmp_path / 'absent')
    with pytest.raises(candidate.CandidateError, match='symlinks'):
        workspace._copy_tree(source, tmp_path / 'target', omit_module_setup=True)


def test_real_lake_seed_reuses_outputs_then_regenerates_setup(tmp_path):
    toolchain = Path.home() / '.elan/toolchains/leanprover--lean4---v4.33.0'
    lake = toolchain / 'bin/lake'
    if not lake.is_file():
        pytest.skip('requires installed Lean 4.33.0')
    source = tmp_path / 'source'; source.mkdir()
    (source / 'lakefile.toml').write_text('name = "setup_seed"\n[[lean_lib]]\nname = "Example"\n')
    (source / 'Example.lean').write_text('def exampleValue : Nat := 1\n')
    env = dict(os.environ, PATH=str(toolchain / 'bin') + os.pathsep + os.environ.get('PATH', ''),
               LEAN_NUM_THREADS='1')
    def build(repo):
        result = subprocess.run([str(lake), 'build', 'Example'], cwd=repo, env=env,
                                text=True, capture_output=True, timeout=90)
        assert result.returncode == 0, result.stdout + result.stderr
        return result
    build(source)
    setup = Path('.lake/build/ir/Example.setup.json')
    assert (source / setup).is_file()
    target = tmp_path / 'target'; target.mkdir()
    for name in ('lakefile.toml', 'Example.lean', 'lake-manifest.json'):
        shutil.copy2(source / name, target / name)
    workspace._copy_tree(source / '.lake/build', target / '.lake/build',
                         clone_builds=True, omit_module_setup=True)
    olean = target / '.lake/build/lib/lean/Example.olean'
    before = (olean.read_bytes(), olean.stat().st_mtime_ns)
    assert not (target / setup).exists()
    build(target)
    assert (olean.read_bytes(), olean.stat().st_mtime_ns) == before
    assert not (target / setup).exists(), 'warm Lake build should reuse outputs without setup'
    (target / 'Example.lean').write_text('def exampleValue : Nat := 2\n')
    build(target)
    assert (target / setup).is_file()
    assert olean.read_bytes() != before[0]
    assert (source / setup).is_file()
