import copy
import hashlib
import json
from pathlib import Path
import zipfile

import pytest

from trellis.dispatch import history
from trellis.history_artifacts import decode_shared_state, load_supervisor_state
from trellis.runtime import git_checkpoint_hook as hook


def object_fixture(repo, *, text='exact frozen proof context λ\n' * 1000, lightweight=None):
    binding = dict(provider='fixture', model='fixture', reasoning_effort='high',
                   timeout_seconds=10, max_timeout_seconds=10,
                   allowed_tasks=['ordinary'], cost_guidance='')
    if lightweight is not None:
        binding['lightweight'] = lightweight
    context = dict(task_id=1, profile='fixture', binding=binding, compiler='managed', task_kind='ordinary')
    request = dict(id=7, cycle=3, kind='Worker', phase='ProofFormalization',
                   task_execution=context, reviewer_comments=text)
    raw = json.dumps(request, ensure_ascii=False, separators=(',', ':'), sort_keys=True).encode()
    reference = {'$dispatch_request': history.FORMAT, 'sha256': hashlib.sha256(raw).hexdigest(),
                 'bytes': len(raw), 'task_execution': context}
    directory = repo / history.DIRECTORY
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / f"{reference['sha256']}.zip"
    with zipfile.ZipFile(path, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr('request.json', raw)
    task = dict(id=1, binding=binding, status=dict(status='running'), pending_retry=None,
                assignment=dict(profile='fixture', compiler='managed', task_kind='ordinary', request=request))
    state = dict(phase='ProofFormalization', dispatch=dict(tasks={'1': task}))
    return state, reference, request, path, raw


@pytest.mark.parametrize('terminal', [False, True])
def test_checkpoint_storage_roundtrip_preserves_legacy_inline_state(tmp_path, terminal):
    state, reference, request, path, _ = object_fixture(tmp_path)
    if terminal:
        state['dispatch']['tasks']['1']['status'] = dict(status='resolved', outcome=dict(kind='timed_out'), resolved_at_ms=20)
    original = copy.deepcopy(state)
    stored = history.checkpoint_requests(dict(state=state), tmp_path, store=True, references={'1': reference})
    assert stored['state']['dispatch']['tasks']['1']['assignment']['request'] == {'$stored_dispatch_request': reference}
    restored = history.checkpoint_requests(stored, tmp_path)
    assert restored == dict(state=original)
    assert state == original
    assert path.stat().st_size < reference['bytes'] // 2
    # An older hook caller without native object references remains inline.
    assert history.checkpoint_requests(dict(state=state), tmp_path, store=True) == dict(state=state)


def test_archived_request_remains_compact_and_exact_object_survives_copy(tmp_path):
    source, destination = tmp_path / 'source', tmp_path / 'destination'
    source.mkdir()
    destination.mkdir()
    state, reference, request, path, _ = object_fixture(source)
    task = state['dispatch']['tasks']['1']
    task['status'] = dict(status='resolved', outcome=dict(kind='timed_out'), resolved_at_ms=20)
    task['assignment']['request'] = reference
    assert history.checkpoint_requests(state, source) == state
    for _ in range(2):
        history.copy_state_objects(state, source, destination)
    objects = list((destination / history.DIRECTORY).iterdir())
    assert len(objects) == 1
    assert objects[0].read_bytes() == path.read_bytes()
    assert history.resolve(destination, reference) == request
    objects[0].unlink()
    with pytest.raises(ValueError):
        history.checkpoint_requests(state, destination)


@pytest.mark.parametrize('mutation', ['digest', 'size', 'binding', 'extra', 'format'])
def test_reference_corruption_fails_closed(tmp_path, mutation):
    _, reference, _, _, _ = object_fixture(tmp_path)
    if mutation == 'digest':
        reference['sha256'] = 'a' * 64
    elif mutation == 'size':
        reference['bytes'] += 1
    elif mutation == 'binding':
        reference['task_execution'] = None
    elif mutation == 'extra':
        reference['path'] = '/outside'
    else:
        reference['$dispatch_request'] = 'unknown'
    with pytest.raises((ValueError, FileNotFoundError)):
        history.resolve(tmp_path, reference)


@pytest.mark.parametrize('entry_shape', ['extra', 'wrong_name', 'wrong_body'])
def test_zip_entry_shape_and_content_are_verified(tmp_path, entry_shape):
    _, reference, _, path, raw = object_fixture(tmp_path)
    with zipfile.ZipFile(path, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr('other.json' if entry_shape == 'wrong_name' else 'request.json',
                         raw[:-1] + b' ' if entry_shape == 'wrong_body' else raw)
        if entry_shape == 'extra':
            archive.writestr('extra', b'unbound bytes')
    with pytest.raises(ValueError):
        history.resolve(tmp_path, reference)


def test_symlink_and_task_binding_are_not_accepted(tmp_path):
    state, reference, _, path, _ = object_fixture(tmp_path)
    path.rename(tmp_path / 'outside.zip')
    path.symlink_to(tmp_path / 'outside.zip')
    with pytest.raises(ValueError):
        history.resolve(tmp_path, reference)
    state['dispatch']['tasks']['1']['assignment']['request'] = reference
    state['dispatch']['tasks']['1']['binding'] = dict(state['dispatch']['tasks']['1']['binding'], model='different')
    with pytest.raises(ValueError, match='task execution binding'):
        history.referenced_requests(state)


def test_checkpoint_hook_uses_native_objects_and_verifies_before_publication(tmp_path, monkeypatch):
    state, reference, request, path, _ = object_fixture(tmp_path)
    monkeypatch.setattr(hook, '_shared_state_enabled', lambda _: True)
    payload = dict(event_count=3, metadata={}, checkpoint={}, commands=[], state=state,
                   dispatch_request_references={'1': reference})
    hook._write_canonical_history(payload, tmp_path)
    checkpoint = hook.supervisor_state_path(tmp_path)
    stored = decode_shared_state(json.loads(checkpoint.read_text()))
    assert stored['state']['dispatch']['tasks']['1']['assignment']['request'] == {'$stored_dispatch_request': reference}
    assert load_supervisor_state(checkpoint)['state'] == state
    before = checkpoint.read_bytes()
    path.write_bytes(b'interrupted/corrupt archive')
    with pytest.raises(ValueError):
        hook._write_canonical_history(payload, tmp_path)
    assert checkpoint.read_bytes() == before


def test_checkpoint_refuses_mismatched_full_contract(tmp_path):
    state, reference, _, _, _ = object_fixture(tmp_path)
    state['dispatch']['tasks']['1']['assignment']['request']['reviewer_comments'] = 'changed'
    with pytest.raises(ValueError, match='exact frozen request'):
        history.checkpoint_requests(state, tmp_path, store=True, references={'1': reference})


def init_git(repo):
    import subprocess
    def git(*args):
        return subprocess.check_output(['git', '-C', str(repo), *args])
    git('init', '-q')
    git('config', 'user.name', 'archive regression')
    git('config', 'user.email', 'archive@example.invalid')
    return git


def test_checkpoint_commits_ignored_event_objects_and_does_not_inflate_unchanged_history(tmp_path, monkeypatch):
    state, reference, _, path, _ = object_fixture(tmp_path)
    state['dispatch']['tasks']['1']['assignment']['request'] = reference
    state['dispatch']['tasks']['1']['status'] = {'status': 'resolved'}
    git = init_git(tmp_path)
    (tmp_path / '.gitignore').write_text('*.zip\n.trellis-history/dispatch-requests/\n')
    payload = dict(event_count=1, metadata={'repo_path': str(tmp_path)}, checkpoint={}, commands=[], state=state)
    monkeypatch.setattr(hook, '_shared_state_enabled', lambda _: False)
    hook.commit_checkpoint(payload)
    assert git('cat-file', 'blob', f'HEAD:{path.relative_to(tmp_path)}') == path.read_bytes()
    # A subsequent ordinary checkpoint must not open any historical ZIP.
    def no_history_read(*args, **kwargs):
        raise AssertionError('ordinary checkpoint inflated unchanged historical request')
    monkeypatch.setattr(history, 'resolve', no_history_read)
    monkeypatch.setattr(history.zipfile, 'ZipFile', no_history_read)
    payload['event_count'] = 2
    hook.commit_checkpoint(payload)
    # Modified immutable bytes still fail, before any new checkpoint commits.
    before = git('rev-parse', 'HEAD')
    path.write_bytes(b'changed evidence')
    with pytest.raises(RuntimeError, match='immutable dispatch request'):
        hook.commit_checkpoint(dict(payload, event_count=3))
    assert git('rev-parse', 'HEAD') == before


def test_migrated_resume_and_differential_checkpoint_read_committed_request_bytes(tmp_path):
    import ast
    import importlib.util
    import re
    import subprocess
    state, reference, request, path, _ = object_fixture(tmp_path)
    stored = history.checkpoint_requests(dict(state=state, event_count=3), tmp_path,
                                        store=True, references={'1': reference})
    snapshot = tmp_path / '.trellis-history/supervisor_state.json'
    snapshot.write_text(json.dumps(stored))
    git = init_git(tmp_path)
    git('add', '-A')
    git('commit', '-qm', 'supervisor2 checkpoint 000003')
    # Both migration and differential replay must use committed evidence,
    # even if the worktree object is corrupt.
    path.write_bytes(b'uncommitted corruption')
    root = Path(__file__).resolve().parents[1]
    script = (root / 'scripts/prepare_migrated_resume.sh').read_text()
    embedded = re.search(r'python3 - "\$ROOT_DIR" "\$REPO_PATH" "\$RUNTIME_ROOT"[^\n]* <<\'PY\'\n(.*?)\nPY', script, re.S).group(1)
    tree = ast.parse(embedded)
    assignments = [node for node in tree.body if isinstance(node, ast.Assign)
        and any(isinstance(target, ast.Name) and target.id == 'supervisor_state' for target in node.targets)]
    namespace = dict(json=json, subprocess=subprocess, supervisor_state_path=snapshot,
                     repo_path=tmp_path, checkpoint_requests=history.checkpoint_requests,
                     decode_shared_state=decode_shared_state)
    exec(compile(ast.Module(body=assignments, type_ignores=[]), 'prepare_migrated_resume.sh', 'exec'), namespace)
    assert namespace['supervisor_state']['state'] == state
    spec = importlib.util.spec_from_file_location('archive_replay_fixture', root / 'scripts/replay_kernel_history.py')
    replay = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(replay)
    assert replay.checkpoint_document(tmp_path / '.git', 'HEAD')['state'] == state
    # A committed corruption fails closed at both boundaries.
    git('add', '-A')
    git('commit', '-qm', 'corrupt object')
    with pytest.raises(ValueError):
        exec(compile(ast.Module(body=assignments, type_ignores=[]), 'prepare_migrated_resume.sh', 'exec'), namespace)
    with pytest.raises(replay.ReplayFailure):
        replay.checkpoint_document(tmp_path / '.git', 'HEAD')


def test_historical_binding_defaults_use_existing_launch_normalization(tmp_path):
    state, reference, request, path, _ = object_fixture(tmp_path)
    # Historical binding omitted the conservative default and explicitly
    # stored empty cost guidance, while newer native serde expands/skips them.
    request['task_execution']['binding'].pop('allowed_tasks')
    raw = json.dumps(request, separators=(',', ':'), sort_keys=True).encode()
    reference['sha256'] = hashlib.sha256(raw).hexdigest()
    reference['bytes'] = len(raw)
    reference['task_execution'] = copy.deepcopy(request['task_execution'])
    reference['task_execution']['binding']['allowed_tasks'] = ['lean_closure']
    reference['task_execution']['binding'].pop('cost_guidance')
    destination = path.parent / f"{reference['sha256']}.zip"
    with zipfile.ZipFile(destination, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr('request.json', raw)
    assert history.resolve(tmp_path, reference) == request
    state['dispatch']['tasks']['1']['assignment']['request'] = reference
    state['dispatch']['tasks']['1']['binding'] = copy.deepcopy(reference['task_execution']['binding'])
    assert history.referenced_requests(state)[reference['sha256']] == reference
    reference['task_execution']['binding']['allowed_tasks'] = ['ordinary']
    with pytest.raises(ValueError, match='binding'):
        history.resolve(tmp_path, reference)


def test_filesystem_free_driver_replays_stored_legacy_request_with_verified_archive(tmp_path):
    import base64
    import importlib.util
    import os
    binary = os.environ.get('TRELLIS_HISTORY_REPLAY_DRIVER')
    if not binary:
        pytest.skip('requires built history_replay_driver example')
    root = Path(__file__).resolve().parents[1]
    spec = importlib.util.spec_from_file_location('archive_driver_fixture', root / 'scripts/replay_kernel_history.py')
    replay = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(replay)
    state, reference, request, path, _ = object_fixture(tmp_path, text='historical defaults λ')
    git = init_git(tmp_path)
    git('add', '-A')
    git('commit', '-qm', 'exact legacy request')
    task = state['dispatch']['tasks']['1']
    task.update(review_id=1, retry_of=None, reserved_at_ms=1, launched_at_ms=None, process=None, checker_wait_ms=0,
                retry=dict(outcome_kind='None', attempt=0, transport_attempt=0,
                           consecutive_transport_failure_node=None, consecutive_transport_failure_count=0))
    task['status'] = dict(status='resolved', outcome=dict(kind='timed_out'), resolved_at_ms=2)
    task['assignment'].update(assignment_id='legacy', timeout_seconds=None, scope=dict(nodes=['A'],
        helper_prefixes=[], shared_writes=[], exclusive=False), base_snapshot='fixture', input_hashes={'A': 'a' * 64})
    state['dispatch'].update(config=dict(worker_slots=1, compiler_slots=1, verifier_slots=1,
        profiles={'fixture': task['binding']}), phase=state['phase'], receipts={}, next_task_id=2,
        event_seq=1, reviewed_event=0, reviewer=None, draining=None)
    task['assignment']['request'] = {'$stored_dispatch_request': reference}
    snapshot = tmp_path / '.trellis-history/supervisor_state.json'
    snapshot.write_text(json.dumps(dict(state=state)))
    git('add', '-A')
    git('commit', '-qm', 'stored checkpoint')
    hydrated = replay.checkpoint_document(tmp_path / '.git', 'HEAD')['state']
    driver = replay.KernelDriver(Path(binary))
    try:
        driver.set_state(hydrated)
        event = dict(event='dispatch', payload=dict(kind='requests_archived', requests={'1': reference}))
        archives = replay.hydrate_dispatch_event(tmp_path / '.git', event)
        assert base64.b64decode(archives[reference['sha256']]) == path.read_bytes()
        driver.apply_event(event, archives)
        assert driver.get_state()['dispatch']['tasks']['1']['assignment']['request']['sha256'] == reference['sha256']
        # Stored request events use the same exact committed object boundary.
        assignment = copy.deepcopy(task['assignment'])
        prepared = dict(event='dispatch', payload=dict(kind='retry_prepared', assignment=assignment))
        assert replay.hydrate_dispatch_event(tmp_path / '.git', prepared) == {}
        assert prepared['payload']['assignment']['request'] == request
    finally:
        driver.close()


def test_differential_replay_refuses_unverified_exclusive_checkpoint_event(tmp_path):
    import importlib.util
    root = Path(__file__).resolve().parents[1]
    spec = importlib.util.spec_from_file_location('checkpoint_replay_fixture', root / 'scripts/replay_kernel_history.py')
    replay = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(replay)
    with pytest.raises(replay.ReplayFailure, match="requires native replay with Git evidence"):
        replay.hydrate_dispatch_event(tmp_path, {"event": "rebind_exclusive_checkpoint"})


@pytest.mark.parametrize('lightweight', [None, True, False])
def test_frozen_lightweight_designation_retains_exact_history_bytes(tmp_path, lightweight):
    state, reference, request, archive, raw = object_fixture(tmp_path, lightweight=lightweight)
    assert ('lightweight' in request['task_execution']['binding']) == (lightweight is not None)
    original = archive.read_bytes()
    restored = history.resolve(tmp_path, reference)
    assert restored == request
    assert archive.read_bytes() == original
    assert hashlib.sha256(raw).hexdigest() == reference['sha256']
    stored = history.checkpoint_requests(dict(state=state), tmp_path, store=True, references={'1': reference})
    assert history.checkpoint_requests(stored, tmp_path) == dict(state=state)
