import copy
import fcntl
import json
from pathlib import Path
import subprocess
import sys

import pytest

from trellis.dispatch import terminal_retention as retention
from trellis.dispatch import retained_files as retained


def put(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value))
    return path


def git(repo, *args):
    return subprocess.check_output(['git', '-C', str(repo), *args], stderr=subprocess.PIPE).decode().strip()


@pytest.fixture
def setup(tmp_path):
    repo = tmp_path / 'project'
    repo.mkdir()
    git(repo, 'init', '-q')
    git(repo, 'config', 'user.email', 'test@example.invalid')
    git(repo, 'config', 'user.name', 'Test')
    task = dict(id=1, review_id=2, reserved_at_ms=12, assignment=dict(assignment_id='worker-one'),
                status=dict(status='resolved', outcome='accepted'))
    state = dict(dispatch=dict(tasks={'1': task}, verifiers=dict(jobs={})))
    put(repo / retention.HISTORY, dict(state=state))
    git(repo, 'add', '.')
    git(repo, 'commit', '-qm', 'Resolve task')
    root = tmp_path / 'runtime/dispatch'
    directory = root / '1'
    put(root / 'coordinator.lock', {})
    put(root.parent / 'protocol_state.json', state)
    put(directory / 'prepared.json', dict(source_repo=str(repo), request_id=7,
        history=dict(cycle=3, kind='Worker', active_node='A', mode='work')))
    raw = put(directory / 'runtime/protocol_state.json', dict(cycle=3,
        in_flight_request=dict(id=7, kind='Worker', cycle=3, active_node='A', mode='work')))
    staged = put(directory / 'runtime/staged-context.json', dict(context='redundant'))
    replay = put(directory / ('repo/.trellis/kernel-replay-environments/' + 'a' * 64 + '.json'), dict(replay='redundant'))
    for name in ('repo/Tablet/A.lean', 'repo/reference/paper.txt', 'execution/response.json',
                 'execution/result.json', 'base-manifest.json', 'sealed.json', 'scratch/findings.json'):
        put(directory / name, dict(retain=name))
    return retention.Collector(root, repo), directory, retention.terminal_tasks(state), (raw, staged, replay), state


def test_retire_committed_terminal_private_inputs_preserves_evidence(setup):
    collector, directory, observed, paths, _ = setup
    before = {p: p.read_bytes() for p in directory.rglob('*') if p.is_file() and p not in paths}
    dry = collector.retire(directory, observed)
    assert dry['files'] == 3 and all(p.exists() for p in paths)
    result = collector.retire(directory, observed, apply=True, budget={'files': 20})
    assert result['removed'] == 3 and not any(p.exists() for p in paths)
    assert all(p.read_bytes() == data for p, data in before.items())
    display = json.loads((directory / 'runtime/display-state.json').read_text())
    assert display['in_flight_request']['id'] == 7 and display['cycle'] == 3
    assert display['checkpoint_commit'] == git(collector.repo, 'rev-parse', 'HEAD')
    assert collector.retire(directory, observed, apply=True)['removed'] == 0


def test_full_and_native_projection_have_same_identity(setup):
    _, _, _, _, state = setup
    full = state['dispatch']['tasks']['1']
    full['assignment'].update(review={'arbitrary': 'large'}, authorized_worker='profile')
    native = copy.deepcopy(full)
    del native['assignment']['review'], native['assignment']['authorized_worker']
    native['assignment']['proof_body_rebase_inputs'] = {}
    assert retention.task_identity(full) == retention.task_identity(native)


@pytest.mark.parametrize('changed', ['id', 'assignment', 'reserved_at_ms', 'status'])
def test_different_committed_identity_is_not_retired(setup, changed):
    collector, directory, _, paths, state = setup
    task = state['dispatch']['tasks']['1']
    if changed == 'assignment':
        task['assignment']['assignment_id'] = 'other'
    elif changed == 'status':
        task['status']['outcome'] = 'failed'
    else:
        task[changed] += 1
    result = collector.retire(directory, retention.terminal_tasks(state), apply=True)
    assert result['status'] == 'not_committed' and all(p.exists() for p in paths)


@pytest.mark.parametrize('guard', ['pin', 'nested_pin', 'live', 'cleanup', 'other_repo', 'symlink', 'hardlink'])
def test_unsafe_tasks_preserved(setup, monkeypatch, guard):
    collector, directory, observed, paths, _ = setup
    if guard in ('pin', 'nested_pin'):
        put(directory / ('acceptance/1/' if guard == 'nested_pin' else '') / 'retain-builds.json', {})
    elif guard in ('live', 'cleanup'):
        put(directory / 'execution/process.json', dict(pid=123))
        if guard == 'live':
            put(directory / 'execution/cleanup.json', dict(pid=123))
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: guard == 'live')
    elif guard == 'other_repo':
        value = json.loads((directory / 'prepared.json').read_text())
        value['source_repo'] = str(directory)
        put(directory / 'prepared.json', value)
    elif guard == 'symlink':
        paths[0].rename(paths[0].with_name('original'))
        paths[0].symlink_to('original')
    else:
        import os
        os.link(paths[0], paths[0].with_name('alias'))
    with pytest.raises((ValueError, FileNotFoundError)):
        collector.retire(directory, observed, apply=True)
    assert all(p.exists() for p in paths)


def test_native_unresolved_never_retired(setup):
    collector, directory, _, paths, _ = setup
    assert collector.retire(directory, {}, apply=True)['removed'] == 0
    assert all(p.exists() for p in paths)


def test_missing_viewer_request_preserves_only_protocol(setup):
    collector, directory, observed, paths, _ = setup
    prepared = json.loads((directory / 'prepared.json').read_text())
    prepared.pop('history')
    put(directory / 'prepared.json', prepared)
    put(paths[0], dict(cycle=3, in_flight_request=None))
    result = collector.retire(directory, observed, apply=True, budget={'files': 20})
    assert result['removed'] == 2 and result['skipped']
    assert paths[0].exists() and not paths[1].exists() and not paths[2].exists()


def test_prepared_history_fastpath_does_not_parse_huge_state(setup):
    collector, directory, observed, paths, _ = setup
    paths[0].write_bytes(b'intentionally unparsable historical protocol')
    assert collector.retire(directory, observed, apply=True, budget={'files': 20})['removed'] == 3


def test_fallback_projects_valid_legacy_state(setup):
    collector, directory, observed, _, _ = setup
    prepared = json.loads((directory / 'prepared.json').read_text())
    prepared.pop('history')
    put(directory / 'prepared.json', prepared)
    assert collector.retire(directory, observed, apply=True, budget={'files': 20})['removed'] == 3


def test_replay_sidecars_finish_after_bounded_interruption(setup, monkeypatch):
    collector, directory, observed, paths, _ = setup
    monkeypatch.setattr(retained, 'MIN_BYTES', 1)
    paths[2].write_bytes(b' ' * 10000)
    assert retained.archive(paths[2])
    for _ in range(8):
        collector.retire(directory, observed, apply=True, budget={'files': 1})
    assert not list(retained.eligible_paths(directory))


def test_coordinator_contention_defers_instead_of_blocking(setup):
    collector, directory, observed, paths, _ = setup
    with (collector.root / 'coordinator.lock').open('rb') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        result = collector.retire(directory, observed, apply=True)
    assert result['status'] == 'coordinator_busy' and all(p.exists() for p in paths)


def test_real_cli_dryrun(setup):
    collector, directory, _, paths, _ = setup
    output = subprocess.check_output([sys.executable, '-m', 'trellis.dispatch.terminal_retention',
        '--runtime', str(collector.root.parent), '--repo', str(collector.repo), '--task', '1'])
    assert json.loads(output)['files'] == 3 and all(p.exists() for p in paths)


def test_background_drains_all_and_uses_observed_native_state(setup):
    from trellis.dispatch.storage import BackgroundCollector
    collector, directory, _, paths, state = setup
    (collector.root.parent / 'protocol_state.json').write_text('must not parse again')
    background = BackgroundCollector(collector.root, repo=collector.repo)
    background._review_snapshots = None
    assert background.schedule(state)
    background._thread.join(10)
    assert not background._thread.is_alive()
    assert sum(not p.exists() for p in paths) == 3
    assert background.schedule(state)
    background._thread.join(10)
    assert not any(p.exists() for p in paths)


@pytest.mark.parametrize('pending_task', [1, 2])
def test_only_same_task_publication_defers(setup, pending_task):
    collector, directory, observed, paths, state = setup
    state['dispatch']['tasks']['1']['pending_retry'] = {'some': 'retry'}
    # Retry preparation needs preserved Tablet/response, never these snapshots.
    put(collector.root / 'publication/pending.json', dict(identity='b' * 64))
    put(collector.root / ('publication/' + 'b' * 64 + '/record.json'), dict(task_id=pending_task))
    result = collector.retire(directory, observed, apply=True, budget={'files': 20})
    assert result['removed'] == (0 if pending_task == 1 else 3)
    assert paths[0].exists() == (pending_task == 1)


def test_nested_git_tracked_private_input_survives(setup):
    collector, directory, observed, paths, _ = setup
    git(directory, 'init', '-q')
    git(directory, 'add', 'runtime/protocol_state.json')
    result = collector.retire(directory, observed, apply=True, budget={'files': 20})
    assert result['removed'] == 2 and paths[0].exists()


def test_receipt_is_durable_before_unlink_and_changed_files_are_preserved(setup):
    collector, directory, observed, paths, _ = setup
    collector.retire(directory, observed, apply=True, budget={'files': 0})
    assert (directory / retention.RECEIPT).exists() and all(p.exists() for p in paths)
    paths[1].write_text('changed after inventory')
    with pytest.raises(ValueError, match='input changed'):
        collector.retire(directory, observed, apply=True, budget={'files': 20})
    assert paths[1].read_text() == 'changed after inventory'


def test_receipt_cannot_redirect_display_write(setup):
    collector, directory, observed, paths, _ = setup
    collector.retire(directory, observed, apply=True, budget={'files': 0})
    receipt = directory / retention.RECEIPT
    record = json.loads(receipt.read_text())
    for item in record['files']:
        item['display_source'] = '../other/runtime/protocol_state.json'
    put(receipt, record)
    with pytest.raises(ValueError, match='display path'):
        collector.retire(directory, observed, apply=True)
    assert all(p.exists() for p in paths)


def test_receipt_checkpoint_cannot_survive_git_rewind(setup):
    collector, directory, observed, paths, _ = setup
    old = git(collector.repo, 'rev-parse', 'HEAD')
    put(collector.repo / 'second', {})
    git(collector.repo, 'add', '.')
    git(collector.repo, 'commit', '-qm', 'Later checkpoint')
    collector.retire(directory, observed, apply=True, budget={'files': 0})
    git(collector.repo, 'reset', '--hard', old)
    with pytest.raises(subprocess.CalledProcessError):
        collector.retire(directory, observed, apply=True)
    assert all(p.exists() for p in paths)


@pytest.mark.parametrize('field', ['id', 'cycle'])
def test_negative_display_request_values_are_invalid(field):
    value = dict(cycle=1, in_flight_request=dict(id=1, cycle=1, kind='Worker'))
    value['in_flight_request'][field] = -1
    assert not retention._valid_display(value)


def test_actual_removed_bytes_reported(setup):
    collector, directory, observed, paths, _ = setup
    size = sum(p.stat().st_size for p in paths)
    allocated = sum(p.stat().st_blocks * 512 for p in paths)
    result = collector.retire(directory, observed, apply=True, budget={'files': 20})
    assert result['removed_bytes'] == size and result['removed_allocated_bytes'] == allocated


def test_invalid_display_prefix_does_not_starve_valid_later_input(setup):
    collector, directory, observed, paths, _ = setup
    prepared = json.loads((directory / 'prepared.json').read_text())
    for name, valid in (('', False), ('acceptance/1', False), ('acceptance/2', True)):
        metadata = dict(prepared)
        if not valid:
            metadata.pop('history')
        put(directory / name / 'prepared.json', metadata)
        put(directory / name / 'runtime/protocol_state.json', {'in_flight_request': None})
    # Each pass removes at most two inputs, but invalid display inputs do not
    # permanently occupy those slots and prevent independent retirement.
    for _ in range(3):
        collector.retire(directory, observed, apply=True, budget={'files': 2})
    assert paths[0].exists()
    assert (directory / 'acceptance/1/runtime/protocol_state.json').exists()
    assert not (directory / 'acceptance/2/runtime/protocol_state.json').exists()
    assert not paths[1].exists() and not paths[2].exists()
