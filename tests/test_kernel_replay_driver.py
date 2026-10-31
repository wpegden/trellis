from __future__ import annotations

import hashlib
import os
from pathlib import Path
import shutil
import subprocess

import pytest

from trellis.checker import kernel_replay_driver as driver

TOOLCHAIN = Path.home() / '.elan/toolchains/leanprover--lean4---v4.33.0'


def upstream():
    path = TOOLCHAIN / 'src/lean/LeanChecker.lean'
    if not path.exists():
        pytest.skip('pinned Lean 4.33 toolchain unavailable')
    data = path.read_bytes()
    assert hashlib.sha256(data).hexdigest() == driver.UPSTREAM_SHA256
    return data


def test_exact_transform_preserves_every_other_upstream_byte():
    original = upstream()
    changed = driver.transformed_source(original)
    assert (changed.replace(driver._SINGLE_PASS.encode(), driver._DISCOVERY.encode(), 1)
            .replace(driver._RESTORE_SEARCH_PATH.encode(), driver._INIT.encode(), 1)
            .replace(driver._SEQUENTIAL_REPLAY.encode(), driver._PARALLEL_REPLAY.encode(), 1)) == original
    assert changed.count(b'SearchPath.findAllWithExt') == 1
    assert driver.transformed_source(original + b'\n') is None


def test_transform_refuses_unexpected_match_count(monkeypatch):
    monkeypatch.setattr(driver, '_DISCOVERY', 'not in upstream source')
    with pytest.raises(RuntimeError, match='discovery block'):
        driver.transformed_source(upstream())


def test_empty_and_disabled_paths_do_not_resolve_tools(tmp_path, monkeypatch):
    monkeypatch.setattr(driver.subprocess, 'run', lambda *a, **k: pytest.fail('resolved tool'))
    assert driver.prepare(tmp_path, [], timeout_secs=1) is None
    monkeypatch.setenv('TRELLIS_KERNEL_REPLAY_PRECOMPILE', '0')
    assert driver.prepare(tmp_path, ['Tablet.A', 'Tablet.B'], timeout_secs=1) is None


@pytest.mark.parametrize('threads', ['1', '6'])
def test_protected_command_passes_effective_thread_option_before_run(tmp_path, threads):
    lean = tmp_path / 'bin/lean'
    lean.parent.mkdir()
    lean.write_text('#!/bin/sh\nprintf "%s\\n" "$@"\n')
    lean.chmod(0o755)
    command = driver.protected_command(lean, tmp_path / 'Run.lean', ['Tablet.A'], tmp_path)
    result = subprocess.run(command[1:], env={**os.environ, 'LEAN_NUM_THREADS': threads},
                            text=True, capture_output=True, check=True)
    assert result.stdout.splitlines()[:2] == [f'-j{threads}', '--run']


@pytest.mark.parametrize('mutation', ['bytes', 'missing', 'alias'])
def test_driver_execution_fence_rejects_mutation(tmp_path, mutation):
    path = tmp_path / 'input'
    path.write_bytes(b'original')
    prepared = driver.PreparedReplay([], tmp_path, {}, {path: driver.sha(path)})
    prepared.validate()
    if mutation == 'bytes':
        path.write_bytes(b'changed')
    elif mutation == 'missing':
        path.unlink()
    else:
        other = tmp_path / 'same'
        other.write_bytes(b'original')
        path.unlink()
        path.symlink_to(other)
    with pytest.raises((RuntimeError, OSError)):
        prepared.validate()


def test_execution_inputs_only_for_exact_upstream(tmp_path):
    source = tmp_path / 'src/lean/LeanChecker.lean'
    source.parent.mkdir(parents=True)
    source.write_bytes(upstream())
    assert driver.execution_inputs(tmp_path) == [Path(driver.__file__).resolve(),
        Path(driver.__file__).with_name('compiled_probe.py').resolve(), source]
    source.write_bytes(source.read_bytes() + b'\n')
    assert driver.execution_inputs(tmp_path) == []


@pytest.fixture(scope='module')
def compiled(tmp_path_factory):
    root = tmp_path_factory.mktemp('replay-driver')
    env = {k: v for k, v in os.environ.items() if not k.startswith(('LEAN_', 'TRELLIS_'))}
    env['PATH'] = str(TOOLCHAIN / 'bin') + os.pathsep + env['PATH']
    env['LEAN_PATH'] = str(root)
    trusted = root / 'trusted'
    trusted.mkdir()
    source = trusted / 'Optimized.lean'
    source.write_bytes(driver.transformed_source(upstream()))
    lean = str(TOOLCHAIN / 'bin/lean')
    def build(path):
        output = path.with_suffix('.olean')
        result = subprocess.run([lean, '-o', str(output), str(path)], cwd=trusted if path.parent == trusted else root,
                                env=env, text=True, capture_output=True, timeout=120)
        assert result.returncode == 0, result.stdout + result.stderr
    build(source)
    runner = trusted / 'Run.lean'
    runner.write_text('import Optimized\n')
    fixtures = {
        'Tiny.lean': 'prelude\naxiom tiny : Sort 0\n',
        'Group/A.lean': 'def goodA : Nat := 1\n',
        'Group/A/Sub.lean': 'def goodSub : Nat := 2\n',
        'Group/B.lean': 'def goodB : Nat := 3\n',
        'Bad.lean': '''import Lean
set_option debug.skipKernelTC true in
run_cmd
  Lean.Elab.Command.liftCoreM <| Lean.addDecl <| .thmDecl {
    name := `badProof
    levelParams := []
    type := .const ``False []
    value := .const ``True.intro []
  }
''',
    }
    for name, content in fixtures.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        build(path)
    return root, env, lean, runner


@pytest.mark.parametrize('args', [
    ['--verbose', 'Group.B', 'Group.A'],
    ['--verbose', 'Group.A', 'Group.A.Sub', 'Group.B', 'Group.A'],
    ['Group', 'Missing'],
    ['Missing', 'Group'],
    ['--fresh', 'Tiny'],
    ['--fresh', 'Group.A', 'Group.B'],
    ['--unknown', '-v', 'Group.B'],
    ['Bad', 'Group.A'],
    ['Group.A', 'Bad'],
])
def test_real_upstream_output_error_and_bad_proof_parity(compiled, args):
    root, env, lean, runner = compiled
    results = []
    for command in ([str(TOOLCHAIN / 'bin/leanchecker')], driver.protected_command(Path(lean), runner, [], runner.parent)[1:]):
        result = subprocess.run([*command, *args], cwd=root, env=env, text=True,
                                capture_output=True, timeout=120)
        results.append((result.returncode, result.stdout, result.stderr))
    assert results[0] == results[1]
    if 'Bad' in args:
        assert results[0][0] != 0 and 'badProof' in ''.join(results[0][1:])


def test_real_replay_targets_do_not_overlap_with_multiple_runtime_threads(compiled):
    root, env, lean, runner = compiled
    source = driver.transformed_source(upstream()).decode().replace('replayFromImports m', 'tracedReplay m')
    trace = '''unsafe def tracedReplay (m : Name) : IO Unit := do
  IO.println s!"start {m}"
  IO.sleep 50
  replayFromImports m
  IO.println s!"end {m}"

'''
    source = source.replace('unsafe def main (args : List String)', trace + 'unsafe def main (args : List String)')
    path = runner.parent / 'SerialTrace.lean'
    path.write_text(source)
    built = subprocess.run([lean, '-o', str(path.with_suffix('.olean')), str(path)],
        cwd=runner.parent, env=env, text=True, capture_output=True, timeout=120)
    assert built.returncode == 0, built.stdout + built.stderr
    traced_runner = runner.parent / 'RunTrace.lean'
    traced_runner.write_text('import SerialTrace\n')
    command = driver.protected_command(Path(lean), traced_runner, ['Group'], runner.parent)
    result = subprocess.run(command[1:], cwd=root, env={**env, 'LEAN_NUM_THREADS': '8'},
                            text=True, capture_output=True, timeout=120)
    assert result.returncode == 0, result.stdout + result.stderr
    lines = result.stdout.splitlines()
    assert len(lines) == 6
    assert {line[6:] for line in lines[::2]} == {'Group.A', 'Group.A.Sub', 'Group.B'}
    assert all(lines[i].startswith('start ') and lines[i + 1] == 'end ' + lines[i][6:]
               for i in range(0, len(lines), 2))


@pytest.mark.parametrize('bad', [False, True])
def test_real_partitioned_replay_preserves_bad_proof_refusal(compiled, bad):
    from concurrent.futures import ThreadPoolExecutor
    root, env, lean, runner = compiled
    nodes = ['Group.A', 'Group.A.Sub', 'Group.B', 'Bad' if bad else 'Tiny']
    command = driver.protected_command(Path(lean), runner, nodes, runner.parent)
    prepared = driver.PreparedReplay(command, Path(lean), {}, {})
    batches = driver.replay_batches(nodes, {}, 2)
    def replay(batch):
        return subprocess.run(prepared.command_for(batch)[1:], cwd=root,
            env={**env, 'LEAN_NUM_THREADS': '1'}, text=True, capture_output=True, timeout=120)
    with ThreadPoolExecutor(2) as pool:
        results = list(pool.map(replay, batches))
    assert all(result.returncode == 0 for result in results) == (not bad)
    if bad:
        assert any('badProof' in result.stdout + result.stderr for result in results)


def test_directory_insertion_and_alias_refused(tmp_path):
    directory = tmp_path / 'compiled'
    directory.mkdir()
    path = directory / 'program'
    path.write_bytes(b'pinned')
    metadata = directory.stat()
    prepared = driver.PreparedReplay([], tmp_path, {}, {path: driver.sha(path)}, directory,
                                     (metadata.st_dev, metadata.st_ino))
    prepared.validate()
    extra = directory / 'Lean.olean'
    extra.write_bytes(b'new shadow')
    with pytest.raises(RuntimeError, match='membership'):
        prepared.validate()
    extra.unlink()
    moved = tmp_path / 'moved'
    directory.rename(moved)
    directory.symlink_to(moved, target_is_directory=True)
    with pytest.raises(RuntimeError, match='membership'):
        prepared.validate()


@pytest.mark.parametrize("shadow_name", ["Lean/Replay", "Lake/Load/Manifest"])
def test_actual_candidate_dependency_shadow_does_not_change_replay(compiled, shadow_name):
    root, env, lean, runner = compiled
    shadow = root / (shadow_name + '.lean')
    shadow.parent.mkdir(parents=True)
    shadow.write_text('prelude\naxiom candidateShadow : Sort 0\n')
    build = subprocess.run([lean, '-o', str(shadow.with_suffix('.olean')), str(shadow)],
                           cwd=root, env=env, capture_output=True, text=True, timeout=60)
    assert build.returncode == 0, build.stdout + build.stderr
    try:
        for args in (['Group.A', 'Group.B'], ['Bad', 'Group.A']):
            results = []
            for command in ([str(TOOLCHAIN / 'bin/leanchecker')],
                            driver.protected_command(Path(lean), runner, [], runner.parent)[1:]):
                result = subprocess.run([*command, *args], cwd=root, env=env,
                                        text=True, capture_output=True, timeout=60)
                results.append((result.returncode, result.stdout, result.stderr))
            assert results[0] == results[1]
            assert (results[0][0] == 0) == ('Bad' not in args)
    finally:
        shutil.rmtree(root / shadow_name.split("/")[0])


def test_uninstalled_toolchain_keeps_stock_failure_path(tmp_path, monkeypatch):
    (tmp_path / 'lean-toolchain').write_text('leanprover/lean4:missing\n')
    monkeypatch.setattr(driver.subprocess, 'run', lambda *a, **k: (_ for _ in ()).throw(
        subprocess.CalledProcessError(1, ['elan', 'which'])))
    assert driver.prepare(tmp_path, ['A', 'B'], timeout_secs=1) is None


def test_prepare_and_real_lake_compiler_confinement(tmp_path, monkeypatch):
    from trellis.atomic_actions import observations as obs
    repo = tmp_path / 'repo'
    repo.mkdir()
    (repo / 'lean-toolchain').write_text('leanprover/lean4:v4.33.0\n')
    (repo / 'lakefile.toml').write_text('name = "replay_driver_test"\nversion = "0.1.0"\n[[lean_lib]]\nname = "Tablet"\n')
    (repo / 'Tablet').mkdir()
    for name in ('A', 'B'):
        (repo / f'Tablet/{name}.lean').write_text(f'def value{name} : Nat := 1\n')
    env = {k: v for k, v in os.environ.items() if not k.startswith(('LEAN_', 'TRELLIS_'))}
    env['PATH'] = str(TOOLCHAIN / 'bin') + os.pathsep + env['PATH']
    for key in list(os.environ):
        if key.startswith(('LEAN_', 'TRELLIS_')):
            monkeypatch.delenv(key)
    monkeypatch.setenv('PATH', env['PATH'])
    built = subprocess.run([str(TOOLCHAIN / 'bin/lake'), 'build', 'Tablet.A', 'Tablet.B'], cwd=repo,
                           env=env, text=True, capture_output=True, timeout=90)
    assert built.returncode == 0, built.stdout + built.stderr
    prepared = driver.prepare(repo, ['Tablet.A', 'Tablet.B'], timeout_secs=90)
    assert prepared is not None
    assert prepared.evidence['mode'] == 'sequential_discovery_v2'
    prepared.validate()
    result = obs._run_lake_command(repo, prepared.command, timeout_secs=90,
                                  bwrap_role='lake_compiler', lake_executable=prepared.lake)
    assert result['returncode'] == 0, result
    prepared.validate()
    from trellis.sandbox import _repo_writable_paths
    assert not any(prepared.directory.is_relative_to(path) for path in
                   _repo_writable_paths(repo, role='lake_compiler', is_isabelle=False))
    custody = obs._run_lake_command(repo, ['env', '/bin/sh', '-c',
        'if touch "$1/forged.olean" 2>/dev/null; then exit 9; fi; '
        'touch "$2/write-control"', 'custody', str(prepared.directory),
        str(repo / '.lake/build')], timeout_secs=90, bwrap_role='lake_compiler',
        lake_executable=prepared.lake)
    assert custody['returncode'] == 0, custody
    assert (repo / '.lake/build/write-control').exists()
    assert not (prepared.directory / 'forged.olean').exists()
    # Actual cached preparation still authenticates its exact generated bytes.
    again = driver.prepare(repo, ['Tablet.B', 'Tablet.A'], timeout_secs=90)
    assert again is not None and again.evidence == prepared.evidence
    bad = tmp_path / 'changed-implementation.py'
    bad.write_text('loaded')
    monkeypatch.setattr(driver, '_IMPLEMENTATION_INPUTS', {bad: driver.sha(bad)})
    bad.write_text('changed')
    with pytest.raises(RuntimeError, match='implementation changed'):
        driver.prepare(repo, ['Tablet.A', 'Tablet.B'], timeout_secs=90)


def test_captured_job_inputs_exclude_precreated_driver_cache(tmp_path):
    import json
    from trellis.checker.job_workspace import capture
    source = tmp_path / 'Tablet/A.lean'
    source.parent.mkdir()
    source.write_text('axiom A : Nat\n')
    forged = tmp_path / '.trellis/kernel-replay-collectors/compiled-v1/fake/Evil.lean'
    forged.parent.mkdir(parents=True)
    forged.write_text('unsafe def main : IO Unit := pure ()\n')
    _, raw = capture(tmp_path)
    assert set(json.loads(raw)) == {'Tablet/A.lean'}


def test_direct_replay_never_considers_precreated_compiled_cache(tmp_path, monkeypatch):
    from trellis.atomic_actions import observations as obs
    from test_atomic_actions import _write_olean, _manifest_stdout_for_args
    for name in ('A', 'B', 'Unrequested'):
        _write_olean(tmp_path, name, name.encode())
    forged = tmp_path / '.trellis/kernel-replay-collectors/compiled-v1/fake'
    forged.mkdir(parents=True)
    (forged / 'receipt.json').write_text('forged')
    monkeypatch.setattr(driver, 'prepare', lambda *a, **k: pytest.fail('direct caller consumed cache'))
    calls = []
    def run(repo, args, **kwargs):
        calls.append(args)
        return dict(returncode=0, stdout=_manifest_stdout_for_args(args), stderr='',
                    timed_out=False, spawn_error='')
    monkeypatch.setattr(obs, '_run_lake_command', run)
    result = obs._kernel_replay_tablet_modules(tmp_path, ['A', 'B'], timeout_secs=10, bwrap_role=None)
    assert result['returncode'] == 0 and result['replay_driver'] == {'mode': 'stock'}
    assert calls[:2] == [['env', 'leanchecker', 'Tablet.A'], ['env', 'leanchecker', 'Tablet.B']]
