"""Real tiny Lean control for the existing exact-scope warmup machinery."""
import json
import os
from pathlib import Path

import pytest

from trellis.atomic_actions import observations as obs
from trellis.checker import admission, telemetry
from trellis.host_runtime import worker_elan_home
from trellis.memory_guard import system_available_bytes


def test_real_tablet_batch_warmup_reuses_replay_preserves_diagnostics_and_admission(tmp_path, monkeypatch):
    tools = worker_elan_home() / 'toolchains/leanprover--lean4---v4.33.0/bin'
    if not (tools / 'lean').is_file():
        pytest.skip('requires installed Lean4.33')
    monkeypatch.setenv('TRELLIS_LEAN_PARALLELISM', '1')
    gate = admission.Admission(1, available=system_available_bytes,
        floor_bytes=6 << 30, reserve_bytes=1 << 30)
    monkeypatch.setattr(admission, 'ACTIVE', gate)
    run_lake = obs._run_lake_command
    calls = []

    def run(repo, args, **kwargs):
        calls.append(list(args))
        # Trusted, locally authored fixture only. Exercise the real existing
        # admitted subprocess path; no live server or package tree is touched.
        kwargs['bwrap_role'] = None
        return run_lake(repo, args, **kwargs, lake_executable=tools / 'lake',
            environment={'PATH': str(tools) + os.pathsep + os.environ['PATH']})

    monkeypatch.setattr(obs, '_run_lake_command', run)
    results = {}
    for mode in ('serial', 'batch', 'failed_batch'):
        repo = tmp_path / mode / 'acceptance/1/repo/.trellis/supervisor/repo'
        (repo / 'Tablet').mkdir(parents=True)
        (repo / '.trellis/scripts').mkdir(parents=True)
        (repo / '.trellis/scripts/check.py').write_text('# fixed fixture identity\n')
        (repo / 'lakefile.toml').write_text('name = "observation_warmup"\n[[lean_lib]]\nname = "Tablet"\n')
        (repo / 'lean-toolchain').write_text('leanprover/lean4:v4.33.0\n')
        # Production workspaces already have the pinned manifest. Creating it
        # during the first build correctly invalidates pre-build source keys.
        (repo / 'lake-manifest.json').write_text(json.dumps(dict(version='1.2.0',
            packagesDir='.lake/packages', packages=[], name='observation_warmup',
            lakeDir='.lake', fixedToolchain=False)))
        for node, source in {
            'Preamble': 'import Init\n',
            'A': 'import Tablet.Preamble\ntheorem A : True := trivial\n',
            'B': 'import Tablet.A\ntheorem B : True := A\n',
            'Open': 'import Tablet.Preamble\ntheorem Open : True := by sorry\n',
            'Bad': 'import Tablet.Preamble\ntheorem Bad : False := True.intro\n',
        }.items():
            (repo / f'Tablet/{node}.lean').write_text(source)
        state = telemetry.Measurement()
        token = telemetry._current.set(state)
        calls.clear()
        try:
            with admission.authoritative_workspace(mode):
                selected = ['A', 'B', 'Open'] if mode != 'failed_batch' else ['A', 'Bad']
                if mode != 'serial':
                    warm = obs.materialize_tablet_oleans(repo, selected, timeout_secs=90, bwrap_role='fixture')
                    assert (warm['returncode'] == 0) == (mode == 'batch'), warm
                observed = {}
                for node in selected:
                    compile_result = obs.compile_node(repo, node, timeout_secs=90, bwrap_role='fixture')
                    diagnostic = compile_result.get('stdout', '') + compile_result.get('stderr', '')
                    observed[node] = dict(returncode=compile_result['returncode'],
                        sorry_warning='declaration uses `sorry`' in diagnostic,
                        actual_type_error='True.intro' in diagnostic and compile_result['returncode'] != 0)
                    if node in ('A', 'B'):
                        audit = obs.print_axioms(repo, node, timeout_secs=90, bwrap_role='fixture', principal_name=node)
                        assert audit['returncode'] == 0 and 'does not depend on any axioms' in audit['stdout'], audit
                assert not gate.active and not gate.waiters
        finally:
            telemetry._current.reset(token)
        measurement = state.snapshot()
        children = measurement['subprocesses']
        leases = [lock for lock in measurement['locks'] if lock['mode'] == 'admission']
        assert measurement['subprocess_records_dropped'] == 0
        assert measurement['lock_count'] == len(children)
        assert len(leases) + measurement['lock_records_dropped'] == len(children)
        assert all(lock['acquired_ms'] <= lock['released_ms'] for lock in leases)
        results[mode] = dict(observed=observed, replay_calls=sum('leanchecker' in call for call in calls),
            compiler_children=len(children), admission_leases=measurement['lock_count'],
            compiler_seconds=sum(child['elapsed_ms'] for child in children) / 1000,
            wall_seconds=measurement['request_elapsed_ms'] / 1000)
        if mode != 'failed_batch':
            assert not obs._tablet_olean_path(repo, 'Bad').exists()  # Unrequested broken source stays excluded.
    assert results['serial']['observed'] == results['batch']['observed']
    assert results['serial']['replay_calls'] == 3
    assert results['batch']['replay_calls'] == 1
    assert results['batch']['observed']['Open']['sorry_warning']
    assert results['failed_batch']['observed']['A']['returncode'] == 0
    assert results['failed_batch']['observed']['Bad']['actual_type_error']
    (tmp_path / 'warmup-measurement.json').write_text(json.dumps(results, indent=2))
