import json
import subprocess
import sys

import pytest

from test_dispatch_terminal_retention import setup, put, git
from test_dispatch_nonworker_retention import owner_setup
from test_dispatch_workspace import project
from trellis.dispatch import execution_retention as execution
from trellis.dispatch import terminal_retention as retention
from trellis.dispatch import storage


def inputs(directory):
    return [put(directory / name, {'redundant': name}) for name in (
        'dispatch-request.json', 'execution/recipe.json',
        'repo/.trellis/runtime/runtime/staging/result.request.json',
        'repo/.trellis/runtime/runtime/private/result.acceptance.json')]


def retained_inputs(directory):
    return [put(directory / name, {'retain': name}) for name in (
        'assignment.json', 'carrier-ready.json', 'execution/result.json', 'execution/response.json',
        'repo/.trellis/runtime/runtime/staging/result.raw.json',
        'repo/.trellis/runtime/runtime/staging/result.json',
        'repo/.trellis/runtime/runtime/staging/result.done',
        'repo/.trellis/runtime/runtime/private/other.json',
        'runtime/provider/transcript.jsonl')]


def test_old_private_receipt_does_not_hide_new_redundant_scope(setup):
    collector, directory, observed, _, _ = setup
    collector.retire(directory, observed, apply=True)
    paths = inputs(directory)
    keep = retained_inputs(directory)
    before = {p: p.read_bytes() for p in keep}
    stats = [p.stat() for p in paths]
    result = collector.retire_execution(directory, observed, apply=True)
    assert result['removed'] == 4
    assert result['removed_bytes'] == sum(s.st_size for s in stats)
    assert result['removed_allocated_bytes'] == sum(s.st_blocks * 512 for s in stats)
    assert not any(p.exists() for p in paths)
    assert all(p.read_bytes() == data for p, data in before.items())
    assert (directory / execution.RECEIPT).exists()
    assert collector.retire_execution(directory, observed, apply=True)['status'] == 'absent'


def test_all_explicit_attempts_and_supervisor_bridge_scopes(setup):
    collector, directory, observed, _, _ = setup
    paths = []
    for suffix in ('', 'turns/1', 'turns/1/transports/2', 'acceptance/1', 'acceptance/1/finalization/1'):
        attempt = directory / suffix
        paths += inputs(attempt)
        paths.append(put(attempt / 'repo/.trellis/supervisor/repo/.trellis/runtime/runtime/private/result.acceptance.json', {}))
    unexpected = inputs(directory / 'arbitrary-child')
    result = collector.retire_execution(directory, observed, apply=True)
    assert result['removed'] == 25
    assert not any(p.exists() for p in paths)
    assert all(p.exists() for p in unexpected)


def test_typed_owners_share_guards_and_preserve_raw_handoffs(owner_setup):
    collector, directory, observed, _, state = owner_setup
    paths = inputs(directory)
    keep = retained_inputs(directory)
    result = collector.retire_execution(directory, retention.terminal_owners(state, defer_verifiers=True), apply=True)
    assert result['removed'] == 4 and not any(p.exists() for p in paths)
    assert all(p.exists() for p in keep)
    assert json.loads((directory / execution.RECEIPT).read_text())['format'] == execution.OWNER_FORMAT


@pytest.mark.parametrize('guard', ['active', 'identity', 'pin', 'live', 'cleanup', 'symlink', 'hardlink', 'tracked'])
def test_redundant_inputs_keep_all_existing_safety_guards(setup, monkeypatch, guard):
    collector, directory, observed, _, _ = setup
    paths = inputs(directory)
    if guard == 'active':
        observed = {}
    elif guard == 'identity':
        observed['1'] = 'different-owner'
    elif guard == 'pin':
        put(directory / 'retain-builds.json', {})
    elif guard in ('live', 'cleanup'):
        put(directory / 'execution/process.json', {'pid': 123})
        put(directory / 'execution/cleanup.json', {'pid': 123 if guard == 'live' else 456})
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: guard == 'live')
    elif guard == 'symlink':
        target = directory / 'repo/.trellis/runtime/runtime/private'
        target.rename(target.with_name('saved-private'))
        target.symlink_to(target.with_name('saved-private'))
    elif guard == 'hardlink':
        import os
        os.link(paths[0], directory / 'duplicate.json')
    elif guard == 'tracked':
        git(directory, 'init', '-q')
        git(directory, 'add', 'execution/recipe.json')
    if guard in ('pin', 'live', 'cleanup', 'symlink', 'hardlink'):
        with pytest.raises(ValueError):
            collector.retire_execution(directory, observed, apply=True)
    else:
        result = collector.retire_execution(directory, observed, apply=True)
        assert result['removed'] == (3 if guard == 'tracked' else 0)
    assert paths[1].exists()


def test_interrupted_deletion_resumes_without_touching_new_or_changed_files(setup):
    collector, directory, observed, _, _ = setup
    paths = inputs(directory)
    assert collector.retire_execution(directory, observed, apply=True, budget={'files': 1})['removed'] == 1
    assert collector.retire_execution(directory, observed, apply=True)['removed'] == 3
    assert not any(p.exists() for p in paths)


def test_committed_batch_adoption_survives_redundant_input_retirement(project):
    from test_dispatch_batch_cache import make_batch
    from trellis.dispatch import batch_storage
    driver, path, workers = make_batch(project)
    driver._materialize_batch(workers, path=path, committed=False)
    state = dict(dispatch=dict(tasks={str(id): dict(id=id, review_id=7, reserved_at_ms=1,
        assignment={'assignment_id': f'assignment-{id}'}, status={'status': 'resolved'}) for id in (1, 2)}, verifiers={'jobs': {}}))
    put(driver.repo / retention.HISTORY, {'state': state})
    git(driver.repo, 'add', retention.HISTORY)
    git(driver.repo, 'commit', '-qm', 'Commit resolved batch')
    put(driver.root / 'coordinator.lock', {})
    collector = retention.Collector(driver.root, driver.repo)
    for id in ('1', '2'):
        directory = driver.root / id
        paths = inputs(directory)
        assert collector.retire_execution(directory, retention.terminal_owners(state), apply=True)['removed'] == 4
        batch_storage.adopt_capture(directory, workers[id])
        assert not any(p.exists() for p in paths)
    driver._materialize_batch(workers, path=path, committed=True)
    driver._materialize(workers, committed=True)


def test_background_counts_execution_and_private_bytes(setup):
    original, directory, _, paths, state = setup
    paths = (*paths, *inputs(directory))
    expected = sum(p.stat().st_blocks * 512 for p in paths)
    collector = storage.BackgroundCollector(original.root, repo=original.repo)
    collector._review_snapshots = None
    assert collector.schedule(state) and collector.wait_idle(15)
    assert collector.summary['execution_inputs'] == 4
    assert collector.summary['removed_allocated_bytes'] == expected
    assert collector.summary['errors'] == 0


def test_actual_execution_cli(setup):
    collector, directory, _, _, _ = setup
    paths = inputs(directory)
    result = subprocess.run([sys.executable, '-m', 'trellis.dispatch.terminal_retention', '--runtime', str(collector.root.parent), '--repo', str(collector.repo), '--task', '1', '--execution-inputs', '--apply'], check=True, text=True, capture_output=True)
    assert json.loads(result.stdout)['removed'] == 4
    assert not any(p.exists() for p in paths)


def test_execution_receipt_cannot_expand_into_raw_handoff_scope(setup):
    collector, directory, observed, _, _ = setup
    paths = inputs(directory)
    raw = put(directory / 'repo/.trellis/runtime/runtime/staging/result.raw.json', {'unique': 'findings'})
    collector.retire_execution(directory, observed, apply=True, budget={'files': 0})
    receipt = directory / execution.RECEIPT
    value = json.loads(receipt.read_text())
    value['files'][0]['path'] = str(raw.relative_to(directory))
    put(receipt, value)
    with pytest.raises(ValueError, match='outside its allowlist'):
        collector.retire_execution(directory, observed, apply=True)
    assert raw.exists() and all(path.exists() for path in paths)


@pytest.mark.parametrize('guard', ['active', 'publication', 'resolved'])
def test_legacy_acceptance_context_obeys_owner_lifecycle(setup, guard):
    collector, directory, observed, _, _ = setup
    path = put(directory / 'acceptance/1/runtime/acceptance-context.json', {'legacy': 'same-owner input'})
    put(directory / 'acceptance/1/prepared.json', {'source_repo': str(collector.repo)})
    if guard == 'active':
        observed = {}
    elif guard == 'publication':
        identity = 'b' * 64
        put(collector.root / 'publication/pending.json', {'identity': identity})
        put(collector.root / 'publication' / identity / 'record.json', {'task_id': 1})
    result = collector.retire_execution(directory, observed, apply=True)
    assert result['removed'] == (1 if guard == 'resolved' else 0)
    assert path.exists() == (guard != 'resolved')
