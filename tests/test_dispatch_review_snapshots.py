import copy
import json
from pathlib import Path
import subprocess

import pytest

from trellis.dispatch import review_snapshots as snapshots
from trellis.dispatch.candidate import canonical_json
from trellis.history_artifacts import encode_shared_state


def git(repo, *args):
    return subprocess.check_output(['git', '-C', str(repo), *args], stderr=subprocess.PIPE).decode().strip()


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(canonical_json(value))


@pytest.fixture
def history(tmp_path):
    repo, runtime = tmp_path / 'repo', tmp_path / 'runtime'
    repo.mkdir()
    git(repo, 'init', '-q')
    git(repo, 'config', 'user.name', 'Test')
    git(repo, 'config', 'user.email', 'test@example.invalid')
    state = dict(request_seq=6, in_flight_request=None, large_evidence='proof' * 10000,
        dispatch=dict(reviewer=None, receipts={}, tasks={}), optional='remove me')
    path = repo / snapshots.HISTORY
    write(path, encode_shared_state(dict(state=state, event_count=10)))
    git(repo, 'add', '.')
    git(repo, 'commit', '-qm', 'base checkpoint')
    base = git(repo, 'rev-parse', 'HEAD')
    captured = copy.deepcopy(state)
    captured.update(request_seq=7, in_flight_request=dict(id=7, kind='Review'))
    captured['dispatch']['reviewer'] = dict(request_id=7, observed_event=3)
    captured.pop('optional')
    captured['new_field'] = ['preserve', {'exact': True}]
    root = runtime / 'dispatch'
    snapshot = root / 'reviews/7/state.json'
    write(snapshot, captured)
    (root / 'coordinator.lock').touch()
    write(root / 'review-7/prepared.json', dict(request_id=7, source_repo=str(repo), source_head=base))
    state['dispatch']['receipts']['7'] = dict(review_id=7, response_sha256='result')
    write(path, encode_shared_state(dict(state=state, event_count=15)))
    git(repo, 'add', '.')
    git(repo, 'commit', '-qm', 'completed review')
    write(runtime / 'protocol_state.json', state)
    return repo, runtime, captured, snapshot


def test_completed_snapshot_round_trips_from_git_after_raw_deletion(history):
    repo, runtime, captured, path = history
    collector = snapshots.Collector(runtime / 'dispatch', repo)
    before = path.read_bytes()
    result = collector.retire(7)
    assert result['status'] == 'eligible'
    assert path.read_bytes() == before
    assert not path.with_name('state.git.json').exists()
    assert collector.retire(7, apply=True)['status'] == 'retired'
    assert not path.exists()
    record = json.loads(path.with_name('state.git.json').read_bytes())
    assert len(canonical_json(record)) < 2000
    # Recovery must use immutable Git blobs, not the latest worktree state.
    (repo / snapshots.HISTORY).write_text('broken dirty history')
    assert snapshots.restore(repo, record) == captured
    assert collector.retire(7, apply=True)['status'] == 'absent'


def test_uncommitted_receipt_is_not_authority(history):
    repo, runtime, _, path = history
    git(repo, 'reset', '--soft', 'HEAD^')
    assert snapshots.Collector(runtime / 'dispatch', repo).retire(7, apply=True)['status'] == 'not_committed'
    assert path.exists()


@pytest.mark.parametrize('change', ['active', 'receipt_changed', 'receipt_removed'])
def test_live_review_and_receipt_drift_are_preserved(history, change):
    repo, runtime, _, path = history
    state_path = runtime / 'protocol_state.json'
    state = json.loads(state_path.read_bytes())
    if change == 'active':
        state['dispatch']['reviewer'] = dict(request_id=7)
    elif change == 'receipt_changed':
        state['dispatch']['receipts']['7']['response_sha256'] = 'changed'
    else:
        state['dispatch']['receipts'].clear()
    write(state_path, state)
    with pytest.raises(ValueError, match='active or no longer'):
        snapshots.Collector(runtime / 'dispatch', repo).retire(7, apply=True)
    assert path.exists()
    assert not path.with_name('state.git.json').exists()


@pytest.mark.parametrize('name', ['reviews/7/retain-state.json', 'review-7/retain-builds.json'])
def test_retention_pins_win(history, name):
    repo, runtime, _, path = history
    write(runtime / 'dispatch' / name, {})
    assert snapshots.Collector(runtime / 'dispatch', repo).retire(7, apply=True)['status'] == 'pinned'
    assert path.exists()


def test_large_unrecorded_difference_is_retained(history, monkeypatch):
    repo, runtime, captured, path = history
    monkeypatch.setattr(snapshots, 'MAX_DELTA_BYTES', 20)
    with pytest.raises(ValueError, match='delta exceeds'):
        snapshots.Collector(runtime / 'dispatch', repo).retire(7, apply=True)
    assert path.exists()


def test_failed_recovery_record_write_keeps_raw_snapshot(history, monkeypatch):
    repo, runtime, _, path = history
    def fail(*args):
        raise OSError('disk full')
    monkeypatch.setattr(snapshots, 'atomic', fail)
    with pytest.raises(OSError, match='disk full'):
        snapshots.Collector(runtime / 'dispatch', repo).retire(7, apply=True)
    assert path.exists()


def test_restart_after_record_write_before_unlink(history, monkeypatch):
    repo, runtime, captured, path = history
    original = Path.unlink
    def fail(self, *args, **kwargs):
        if self == path:
            raise OSError('interrupted unlink')
        return original(self, *args, **kwargs)
    with monkeypatch.context() as context:
        context.setattr(Path, 'unlink', fail)
        with pytest.raises(OSError, match='interrupted unlink'):
            snapshots.Collector(runtime / 'dispatch', repo).retire(7, apply=True)
    assert path.exists() and path.with_name('state.git.json').exists()
    assert snapshots.Collector(runtime / 'dispatch', repo).retire(7, apply=True)['status'] == 'retired'
    assert snapshots.restore(repo, json.loads(path.with_name('state.git.json').read_bytes())) == captured


def test_snapshot_mutation_during_verification_is_retained(history, monkeypatch):
    repo, runtime, captured, path = history
    original = snapshots.git_state
    def mutate(*args):
        state = original(*args)
        path.write_bytes(path.read_bytes() + b'\n')
        return state
    monkeypatch.setattr(snapshots, 'git_state', mutate)
    with pytest.raises(ValueError, match='snapshot changed'):
        snapshots.Collector(runtime / 'dispatch', repo).retire(7, apply=True)
    assert path.exists()


def test_git_rewind_during_verification_is_retained(history, monkeypatch):
    repo, runtime, _, path = history
    original = snapshots.git_state
    def rewind(*args):
        state = original(*args)
        git(repo, 'reset', '--soft', 'HEAD^')
        return state
    monkeypatch.setattr(snapshots, 'git_state', rewind)
    with pytest.raises(subprocess.CalledProcessError):
        snapshots.Collector(runtime / 'dispatch', repo).retire(7, apply=True)
    assert path.exists()


@pytest.mark.parametrize('name', ['state.json', 'state.git.json', 'prepared.json'])
def test_symlinks_are_never_followed_for_retirement(history, name, tmp_path):
    repo, runtime, captured, path = history
    target = path if name == 'state.json' else path.with_name(name)
    if name == 'prepared.json':
        target = runtime / 'dispatch/review-7/prepared.json'
    outside = tmp_path / 'outside'
    outside.write_bytes(target.read_bytes() if target.exists() else b'{}')
    target.unlink(missing_ok=True)
    target.symlink_to(outside)
    with pytest.raises(ValueError, match='symlink'):
        snapshots.Collector(runtime / 'dispatch', repo).retire(7, apply=True)
    assert outside.exists()


def test_recovery_detects_modified_delta(history):
    repo, runtime, _, path = history
    snapshots.Collector(runtime / 'dispatch', repo).retire(7, apply=True)
    record = json.loads(path.with_name('state.git.json').read_bytes())
    record['delta'].append(dict(path=['request_seq'], value=999))
    with pytest.raises(ValueError, match='digest differs'):
        snapshots.restore(repo, record)


def test_numeric_type_difference_cannot_silently_round_trip(history):
    repo, runtime, captured, path = history
    # Python equality alone treats True and 1 as equal; the JSON digest must not.
    captured['request_seq'] = 7.0
    write(path, captured)
    collector = snapshots.Collector(runtime / 'dispatch', repo)
    collector.retire(7, apply=True)
    restored = snapshots.restore(repo, json.loads(path.with_name('state.git.json').read_bytes()))
    assert type(restored['request_seq']) is float


def test_background_collector_retires_snapshot_without_touching_other_evidence(history):
    from trellis.dispatch.storage import BackgroundCollector
    repo, runtime, _, path = history
    root = runtime / 'dispatch'
    write(root / 'reviews/7/batch.json', {'receipt': 'must remain'})
    write(root / 'review-7/runtime/transcript.json', {'messages': ['must remain']})
    state = json.loads((runtime / 'protocol_state.json').read_bytes())
    state['dispatch']['verifiers'] = {'jobs': {}}
    collector = BackgroundCollector(root, repo=repo)
    collector.schedule(state)
    collector._thread.join(5)
    assert not collector._thread.is_alive()
    assert not path.exists()
    assert path.with_name('state.git.json').exists()
    assert json.loads((root / 'reviews/7/batch.json').read_bytes()) == {'receipt': 'must remain'}
    assert (root / 'review-7/runtime/transcript.json').exists()


def test_stored_request_recovery_uses_git_object_not_worktree(history):
    import hashlib
    import io
    import zipfile
    from trellis.dispatch.history import FORMAT, DIRECTORY
    repo, runtime, captured, path = history
    request = {'id': 1, 'task_execution': None, 'payload': 'frozen'}
    raw = canonical_json(request)
    reference = {'$dispatch_request': FORMAT, 'sha256': hashlib.sha256(raw).hexdigest(),
                 'bytes': len(raw), 'task_execution': None}
    archive = io.BytesIO()
    with zipfile.ZipFile(archive, 'w', compression=zipfile.ZIP_DEFLATED) as stream:
        stream.writestr('request.json', raw)
    object_path = repo / DIRECTORY / (reference['sha256'] + '.zip')
    object_path.parent.mkdir(parents=True)
    object_path.write_bytes(archive.getvalue())
    state = dict(dispatch=dict(tasks={'1': {'assignment': {'request': {'$stored_dispatch_request': reference}}}}))
    write(repo / snapshots.HISTORY, dict(state=state))
    git(repo, 'add', '.')
    git(repo, 'commit', '-qm', 'stored request')
    commit = git(repo, 'rev-parse', 'HEAD')
    object_path.write_bytes(b'corrupt working copy')
    assert snapshots.git_state(repo, commit)['dispatch']['tasks']['1']['assignment']['request'] == request


@pytest.mark.parametrize('compressed', [False, True])
def test_adopted_review_restores_retired_input_instead_of_current_canonical(history, monkeypatch, compressed):
    if compressed:
        monkeypatch.setattr(snapshots.review_snapshot_files, 'COMPRESS_AT', 1)
    from trellis.dispatch.native import NativeLedger
    repo, runtime, captured, path = history
    collector = snapshots.Collector(runtime / 'dispatch', repo)
    collector.retire(7, apply=True)
    write(runtime / 'runtime_metadata.json', dict(repo_path=str(repo)))
    ledger = NativeLedger(runtime, kernel=lambda _: dict(status='dispatch_ok',
        output=dict(reviewer=dict(request_id=7, observed_event=3))))
    assert ledger.start_review()['request_id'] == 7
    assert snapshots.review_snapshot_files.read(path) == captured
    assert snapshots.review_snapshot_files.read(path) != json.loads((runtime / 'protocol_state.json').read_bytes())
    # Restoring changes whitespace, not state; subsequent retirement remains safe.
    if compressed:
        import gzip
        snapshots.review_snapshot_files.location(path).write_bytes(
            gzip.compress(json.dumps(captured, indent=2).encode(), compresslevel=0))
    else:
        path.write_text(json.dumps(captured, indent=2))
    assert collector.retire(7, apply=True)['status'] == 'retired'


def test_new_task_since_base_uses_exact_request_object_from_completed_git_tree(history, monkeypatch):
    import hashlib
    import io
    import zipfile
    from trellis.dispatch.history import FORMAT, DIRECTORY
    repo, runtime, captured, path = history
    request = {'id': 8, 'task_execution': None, 'payload': 'worker input' * 10000}
    raw = canonical_json(request)
    reference = {'$dispatch_request': FORMAT, 'sha256': hashlib.sha256(raw).hexdigest(),
                 'bytes': len(raw), 'task_execution': None}
    archive = io.BytesIO()
    with zipfile.ZipFile(archive, 'w', compression=zipfile.ZIP_DEFLATED) as stream:
        stream.writestr('request.json', raw)
    object_path = repo / DIRECTORY / (reference['sha256'] + '.zip')
    object_path.parent.mkdir(parents=True)
    object_path.write_bytes(archive.getvalue())
    captured['dispatch']['tasks']['8'] = {'assignment': {'request': request}, 'status': 'running'}
    write(path, captured)
    current = json.loads((runtime / 'protocol_state.json').read_bytes())
    current['dispatch']['tasks']['8'] = {'assignment': {'request': reference}, 'status': 'resolved'}
    write(repo / snapshots.HISTORY, encode_shared_state(dict(state=current)))
    git(repo, 'add', '.')
    git(repo, 'commit', '-qm', 'archive task created after reviewer base')
    write(runtime / 'protocol_state.json', current)
    monkeypatch.setattr(snapshots, 'MAX_DELTA_BYTES', 10000)
    collector = snapshots.Collector(runtime / 'dispatch', repo)
    assert collector.retire(7, apply=True)['status'] == 'retired'
    record = json.loads(path.with_name('state.git.json').read_bytes())
    assert len(canonical_json(record)) < 10000
    assert record['request_objects'][0]['reference'] == reference
    object_path.write_bytes(b'dirty working copy must not affect restoration')
    assert snapshots.restore(repo, record) == captured
    assert snapshots.restore(repo, record) == captured  # Restoration must not mutate the record.
    write(path, captured)
    git(repo, 'commit', '--allow-empty', '-qm', 'later witness')
    assert collector.retire(7, apply=True)['status'] == 'retired'
