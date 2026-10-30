from pathlib import Path

import pytest

from trellis.atomic_actions import observations as obs
from trellis.checker import semantic_groups
from test_compiled_semantic_probe import semantic_server, request


def test_grouping_uses_requested_roots_only(tmp_path):
    (tmp_path / 'Tablet').mkdir()
    for name, text in {'A': 'import Tablet.B\n', 'B': '', 'C': '', 'Extra': 'import Tablet.A Tablet.C\n'}.items():
        (tmp_path / 'Tablet' / f'{name}.lean').write_text(text)
    assert semantic_groups.plan_groups(tmp_path, ['B', 'A', 'C']) == [('A', ['B', 'A'])]
    assert semantic_groups.plan_groups(tmp_path, ['B', 'C']) == []


def test_grouping_is_optional_on_drift_or_oversize(tmp_path, monkeypatch):
    monkeypatch.setattr(obs.SourceClosureSnapshot, 'is_current', lambda _: False)
    assert semantic_groups.plan_groups(tmp_path, ['A', 'B']) == []
    def overflow(*args):
        raise obs.SourceClosureSnapshotTooLarge()
    monkeypatch.setattr(obs.SourceClosureSnapshot, 'materialization_order', overflow)
    assert semantic_groups.plan_groups(tmp_path, ['A', 'B']) == []


@pytest.mark.parametrize('fault', ['none', 'missing', 'error', 'foreign', 'exit', 'timeout', 'resource'])
def test_shared_probe_falls_back_without_poisoning_success(tmp_path, monkeypatch, fault):
    script = tmp_path / 'collector.lean'; script.write_text('import Lean\n')
    calls = []
    def run(repo, args, **kwargs):
        calls.append(args)
        if '--shared-root' not in args:
            return dict(returncode=0, stdout=f'FP\t{args[-2]}\tsingle-{args[-2]}')
        stdout = 'FP\tRoot\tshared-Root\nFP\tDep\tshared-Dep\n'
        if fault == 'missing': stdout = 'FP\tRoot\tshared-Root\n'
        if fault == 'error': stdout = 'FP\tRoot\tshared-Root\nERR\tDep\tforeign principal\n'
        if fault == 'foreign': stdout = 'FP\tRoot\tshared-Root\nFP\tOther\tbad\n'
        return dict(returncode=1 if fault == 'exit' else 0, stdout=stdout,
                    timed_out=fault == 'timeout', resource_exhausted='memory' if fault == 'resource' else '')
    monkeypatch.setattr(obs, '_run_lake_command', run)
    result = obs.observe_lean_semantic_payloads(tmp_path, ['Root', 'Dep'], bwrap_role='lake_compiler',
        script_path=script, shared_root='Root', principal_names={'Root': 'R', 'Dep': 'D'})
    assert all(value['ok'] for value in result.values())
    both_fallback = fault in ('exit', 'timeout', 'resource')
    assert result['Root']['payload'] == ('single-Root' if both_fallback else 'shared-Root')
    assert result['Dep']['payload'] == ('shared-Dep' if fault == 'none' else 'single-Dep')
    assert len(calls) == (1 if fault == 'none' else 3 if both_fallback else 2)
    assert calls[0][-6:] == ['--shared-root', 'Root', 'Root', 'R', 'Dep', 'D']


def test_extra_root_is_rejected_before_execution(tmp_path, monkeypatch):
    monkeypatch.setattr(obs, '_run_lake_command', lambda *a, **kw: pytest.fail('unexpected process'))
    with pytest.raises(ValueError, match='must be requested'):
        obs.observe_lean_semantic_payloads(tmp_path, ['A'], bwrap_role='lake_compiler', shared_root='B')


@pytest.mark.parametrize('drift', ['none', 'Root', 'Dep', 'readiness'])
def test_server_validates_whole_shared_basis_before_returning(semantic_server, monkeypatch, drift):
    from trellis.checker import server as server_mod
    server, _, _, _ = semantic_server
    monkeypatch.setattr(semantic_groups, 'plan_groups', lambda repo, nodes: [('Root', ['Root', 'Dep'])])
    original_keys = server_mod.compute_semantic_payload_cache_keys
    calls = []; key_passes = 0; readiness_passes = 0
    def keys(*args, **kwargs):
        nonlocal key_passes
        key_passes += 1
        result = original_keys(*args, **kwargs)
        if key_passes > 1 and drift in ('Root', 'Dep'):
            result[drift] += '-changed'
        return result
    monkeypatch.setattr(server_mod, 'compute_semantic_payload_cache_keys', keys)
    original_readiness = server._semantic_replay_readiness
    def readiness(nodes):
        nonlocal readiness_passes
        readiness_passes += 1
        result = original_readiness(nodes)
        if drift == 'readiness' and readiness_passes == 2:
            result['Root'] = False
        return result
    monkeypatch.setattr(server, '_semantic_replay_readiness', readiness)
    def run(repo, args, **kwargs):
        calls.append(args)
        if '--shared-root' in args:
            return dict(returncode=0, stdout='FP\tRoot\tshared-Root\nFP\tDep\tshared-Dep\n')
        return dict(returncode=0, stdout=f'FP\t{args[-2]}\tsingle-{args[-2]}')
    monkeypatch.setattr(obs, '_run_lake_command', run)
    result, _, _ = server._handle_lean_semantic_payloads(request())
    expected = 'shared' if drift == 'none' else 'single'
    assert {n: v['payload'] for n, v in result['nodes'].items()} == {n: f'{expected}-{n}' for n in ('Root', 'Dep')}
    assert len(calls) == (1 if drift == 'none' else 3)


def test_cached_root_is_not_used_as_unguarded_shared_basis(semantic_server, monkeypatch):
    server, _, _, _ = semantic_server
    from trellis.checker.protocol import validate_request
    server._handle_lean_semantic_payloads(validate_request(dict(op='lean_semantic_payloads', request_id=0,
        nodes=['Root'], principal_names={'Root': 'root'})))
    planned = []
    def plan(repo, nodes):
        planned.append(list(nodes)); return []
    monkeypatch.setattr(semantic_groups, 'plan_groups', plan)
    result, _, extra = server._handle_lean_semantic_payloads(request())
    assert planned == [['Dep']]
    assert extra['cache_hits'] == 1 and all(v['ok'] for v in result['nodes'].values())


def test_planner_bounds_aggregate_closure_memberships(tmp_path, monkeypatch):
    (tmp_path / 'Tablet').mkdir()
    (tmp_path / 'Tablet/B.lean').write_text('')
    (tmp_path / 'Tablet/A.lean').write_text('import Tablet.B\n')
    (tmp_path / 'Tablet/C.lean').write_text('import Tablet.B\n')
    monkeypatch.setattr(obs.SourceClosureSnapshot, 'MAX_CLOSURE_MEMBERS', 3)
    # Two valid two-member closures exceed the aggregate bound despite only
    # three distinct files. Do not retain an unbounded matrix of overlap sets.
    assert semantic_groups.plan_groups(tmp_path, ['A', 'C']) == []


def test_second_group_drift_during_first_fallback_is_not_returned_shared(semantic_server, monkeypatch):
    from trellis.checker import server as server_mod
    from trellis.checker.protocol import validate_request
    server, _, _, _ = semantic_server
    nodes = ['A', 'a', 'B', 'b']; phase = 0; calls = []
    monkeypatch.setattr(server, '_ensure_closures_have_current_kernel_replay', lambda *a, **k: None)
    monkeypatch.setattr(server, '_semantic_replay_readiness', lambda ns: {n: True for n in ns})
    monkeypatch.setattr(semantic_groups, 'plan_groups', lambda repo, ns: [('A', ['A', 'a']), ('B', ['B', 'b'])])
    def keys(repo, ns, *args):
        return {n: n + ('-changed' if (phase >= 1 and n == 'A') or (phase >= 2 and n == 'B') else '') for n in ns}
    monkeypatch.setattr(server_mod, 'compute_semantic_payload_cache_keys', keys)
    def run(repo, args, **kwargs):
        nonlocal phase
        if '--shared-root' in args:
            root = args[args.index('--shared-root') + 1]; calls.append('shared-' + root)
            if root == 'B': phase = 1
            return dict(returncode=0, stdout=f'FP\t{root}\tshared-{root}\nFP\t{root.lower()}\tshared-{root.lower()}\n')
        node = args[-2]; calls.append('single-' + node)
        if node == 'A': phase = 2
        return dict(returncode=0, stdout=f'FP\t{node}\tsingle-{node}\n')
    monkeypatch.setattr(obs, '_run_lake_command', run)
    response, _, _ = server._handle_lean_semantic_payloads(validate_request(dict(
        op='lean_semantic_payloads', request_id=7, nodes=nodes, principal_names={n: n for n in nodes})))
    assert all(response['nodes'][n]['payload'] == 'single-' + n for n in nodes)
    assert calls == ['shared-A', 'shared-B', 'single-A', 'single-a', 'single-B', 'single-b']


LEAN = Path.home() / '.elan/toolchains/leanprover--lean4---v4.33.0/bin/lean'


@pytest.mark.skipif(not LEAN.is_file(), reason='pinned Lean4.33 required')
def test_real_common_root_private_generated_and_provenance(tmp_path):
    import os
    import subprocess
    (tmp_path / 'Tablet').mkdir()
    (tmp_path / 'Tablet/Base.lean').write_text('private def secret : Nat := 7\nopaque hidden : Nat := secret\ninductive Token where\n | mk : Nat → Token\ndef generated : Token → Nat\n | .mk n => n\ntheorem Base : hidden = hidden := rfl\n')
    (tmp_path / 'Tablet/Consumer.lean').write_text('import Tablet.Base\ndef consume : Token → Nat := generated\ntheorem Consumer : consume (.mk 2) = 2 := rfl\n')
    env = dict(os.environ, LEAN_PATH=str(tmp_path), PATH=str(LEAN.parent) + ':' + os.environ['PATH'])
    def run(args, code=0):
        p = subprocess.run([str(LEAN), *args], cwd=tmp_path, env=env, capture_output=True, timeout=60)
        assert p.returncode == code, p.stderr.decode() + p.stdout.decode()
        return p.stdout
    for name in ['Base', 'Consumer']:
        run(['-o', f'Tablet/{name}.olean', f'Tablet/{name}.lean'])
    source = Path(__file__).resolve().parents[1] / 'scripts/lean_semantic_fingerprint.lean'
    (tmp_path / 'Collector.lean').write_bytes(source.read_bytes())
    run(['-o', 'Collector.olean', 'Collector.lean'])
    (tmp_path / 'Run.lean').write_text('import Collector\n')
    pairs = [('Base', 'Base'), ('Base', 'hidden'), ('Base', 'generated'), ('Base', 'Token.rec'),
             ('Base', 'secret'), ('Consumer', 'Consumer'), ('Consumer', 'Base'), ('Consumer', 'hidden'), ('Base', 'absent')]
    individual = b''.join(run(['--run', 'Run.lean', *pair]) for pair in pairs)
    shared = run(['--run', 'Run.lean', '--shared-root', 'Consumer', *[v for pair in pairs for v in pair]])
    assert shared == individual
    assert [line.split(b'\t', 1)[0] for line in shared.splitlines()] == [b'FP'] * 4 + [b'ERR', b'FP', b'ERR', b'ERR', b'ERR']
    run(['--run', 'Run.lean', '--shared-root', 'Consumer', 'Base', 'Base'], code=1)
