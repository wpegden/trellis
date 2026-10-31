"""Replay execution shares admission without changing coverage or attestations."""
import threading
from types import SimpleNamespace

import pytest

from trellis.atomic_actions import observations as obs
from trellis.checker import admission, kernel_replay_driver as driver, portable_replay


def test_batches_cover_each_requested_module_and_balance_large_inputs():
    nodes = ['A', 'B', 'C', 'D', 'E', 'F', 'G']
    bundles = {n: [{'size_bytes': size}] for n, size in zip(nodes, [100, 90, 30, 20, 10, 5, 1])}
    batches = driver.replay_batches(nodes, bundles, 2)
    assert len(batches) == 2
    assert sorted(n for batch in batches for n in batch) == nodes
    assert next(b for b in batches if 'A' in b) != next(b for b in batches if 'B' in b)
    assert driver.replay_batches(nodes, bundles, 1) == [nodes]


def test_namespace_targets_stay_with_their_descendants():
    nodes = ['A.Child.More', 'AB', 'A', 'B', 'A.Child', 'B.Other']
    batches = driver.replay_batches(nodes, {}, 6)
    assert sorted(node for batch in batches for node in batch) == sorted(nodes)
    assert next(batch for batch in batches if 'A' in batch) == ['A.Child.More', 'A', 'A.Child']
    assert next(batch for batch in batches if 'B' in batch) == ['B', 'B.Other']
    assert next(batch for batch in batches if 'AB' in batch) == ['AB']


@pytest.mark.parametrize('failure', [None, 'returncode', 'timed_out', 'spawn_error', 'resource_exhausted'])
def test_parallel_replay_drains_and_only_attests_complete_success(tmp_path, monkeypatch, failure):
    nodes = ['A', 'B', 'C']
    scheduler = admission.Admission(3, readonly_slots=3, cpu_budget=3)
    monkeypatch.setattr(admission, 'ACTIVE', scheduler)
    monkeypatch.setattr(portable_replay, 'acceptance_workspace', lambda _: False)
    monkeypatch.setattr(obs, '_artifact_bundle', lambda _, n: [{'size_bytes': 1, 'sha256': n, 'level': 'exported'}])
    monkeypatch.setattr(obs, 'olean_has_current_kernel_replay', lambda *a, **k: False)
    root = tmp_path / '.lake/build/lib/lean/Tablet'
    root.mkdir(parents=True)
    for node in nodes:
        (root / f'{node}.olean').write_bytes(b'x')
    prepared = driver.PreparedReplay(['env', 'lean', '--run', 'Pinned.lean', 'Tablet'], tmp_path, {}, {})
    monkeypatch.setattr(driver, 'prepare', lambda *a, **k: prepared)
    barrier = threading.Barrier(3)
    finished, attested = [], []
    @admission.admitted
    def run(repo, command, **kwargs):
        node = command[-1].removeprefix('Tablet.')
        lease = scheduler.active[threading.get_ident()]
        assert lease['readonly'] and lease['operation'] == 'kernel_replay'
        assert lease['cpu_units'] == 1
        assert set(admission._replay_input.get()) == {node}
        barrier.wait(3)
        result = dict(returncode=0, stdout='', stderr='', timed_out=False, spawn_error='')
        if node == 'A' and failure:
            result[failure] = 1 if failure == 'returncode' else 'failed'
        finished.append(node)
        return result
    monkeypatch.setattr(obs, '_run_lake_command', run)
    def manifests(repo, requested, **kwargs):
        assert sorted(finished) == nodes
        assert failure is None, 'failed replay reached attestation'
        return ({n: [] for n in nodes}, dict(returncode=0,
            artifact_parts={n: [{'level': 'exported'}] for n in nodes}))
    monkeypatch.setattr(obs, '_capture_replay_declaration_manifests', manifests)
    monkeypatch.setattr(obs, '_write_kernel_replay_attestation', lambda _, n, **k: attested.append(n) or True)
    result = obs._kernel_replay_tablet_modules(tmp_path, nodes, timeout_secs=10, bwrap_role='lake_compiler')
    assert sorted(finished) == nodes and not scheduler.active and not scheduler.waiters
    if failure:
        assert not attested and result[failure]
    else:
        assert attested == nodes and result['attested_nodes'] == nodes
        assert result['replay_batches'] == 3


def test_parallel_replay_refuses_changed_artifact_after_join(tmp_path, monkeypatch):
    # Reuse the production before/after attestation path, including a change
    # that leaves replay itself successful.
    from test_atomic_actions import _write_olean, _manifest_stdout_for_args
    for node in ['A', 'B']:
        _write_olean(tmp_path, node, node.encode())
    monkeypatch.setattr(admission, 'ACTIVE', admission.Admission(2, readonly_slots=2, cpu_budget=2))
    monkeypatch.setattr(portable_replay, 'acceptance_workspace', lambda _: False)
    prepared = driver.PreparedReplay(['env', 'lean', '--run', 'Pinned.lean', 'Tablet'], tmp_path, {}, {})
    monkeypatch.setattr(driver, 'prepare', lambda *a, **k: prepared)
    def run(repo, args, **kwargs):
        if 'Pinned.lean' in args:
            if args[-1] == 'Tablet.A':
                _write_olean(tmp_path, 'A', b'changed')
            return dict(returncode=0, stdout='', stderr='', timed_out=False, spawn_error='')
        return dict(returncode=0, stdout=_manifest_stdout_for_args(args), stderr='', timed_out=False, spawn_error='')
    monkeypatch.setattr(obs, '_run_lake_command', run)
    result = obs._kernel_replay_tablet_modules(tmp_path, ['A', 'B'], timeout_secs=10, bwrap_role='lake_compiler')
    assert result['returncode'] != 0
    assert 'changed' in result['stderr']


def test_later_batches_share_the_original_replay_deadline(tmp_path, monkeypatch):
    nodes = ['A', 'B', 'C', 'D']
    monkeypatch.setattr(admission, 'ACTIVE', admission.Admission(2, readonly_slots=2, cpu_budget=2))
    monkeypatch.setattr(portable_replay, 'acceptance_workspace', lambda _: False)
    monkeypatch.setattr(obs, '_artifact_bundle', lambda _, n: [{'size_bytes': 1, 'sha256': n, 'level': 'exported'}])
    monkeypatch.setattr(obs, 'olean_has_current_kernel_replay', lambda *a, **k: False)
    prepared = driver.PreparedReplay(['env', 'lean', '--run', 'Pinned.lean', 'Tablet'], tmp_path, {}, {})
    monkeypatch.setattr(driver, 'prepare', lambda *a, **k: prepared)
    clock = [0]
    monkeypatch.setattr(obs, 'time', SimpleNamespace(monotonic=lambda: clock[0]))
    launched = []
    def run(repo, command, **kwargs):
        assert kwargs['timeout_secs'] == 1
        launched.append(command[-1])
        clock[0] = 2
        return dict(returncode=0, stdout='', stderr='', timed_out=False, spawn_error='')
    monkeypatch.setattr(obs, '_run_lake_command', run)
    # Simulate a pool whose later leaf starts after earlier work consumed the
    # deadline; every leaf must use the same deadline even under contention.
    monkeypatch.setattr(admission, 'map_prepared', lambda operation, fn, batches:
                        [fn(batch) for batch in batches])
    monkeypatch.setattr(obs, '_capture_replay_declaration_manifests',
                        lambda *a, **k: pytest.fail('expired replay reached attestation'))
    result = obs._kernel_replay_tablet_modules(tmp_path, nodes, timeout_secs=1, bwrap_role='lake_compiler')
    assert result['timed_out'] and len(launched) == 1
