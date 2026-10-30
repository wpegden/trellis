import asyncio
from functools import partial
import json
from pathlib import Path
from types import SimpleNamespace
import zipfile

import pytest

from trellis.dispatch import batch_cache, batch_storage, candidate, workspace
from trellis.dispatch.driver import ProductionDriver
from trellis.dispatch.preparation_capacity import PreparationDeferred
from test_dispatch_workspace import project


class Capacity:
    workspace_bytes = 1024
    workspace_inodes = 10
    blocked = False

    def __init__(self):
        self.calls = []

    def require_allocation(self, size, count, **kwargs):
        self.calls.append((size, count))
        if self.blocked:
            raise PreparationDeferred('test storage wait')

    def ready(self):
        return not self.blocked


def make_batch(project):
    repo, runtime, config = project
    driver = ProductionDriver(runtime, kernel=lambda _: pytest.fail('unexpected native call'),
                              prepare_workspace=partial(workspace.prepare, seed_packages=False))
    driver.preparation_capacity = Capacity()
    (runtime / 'protocol_state.json').write_text(json.dumps(dict(dispatch=dict(tasks={}))))
    manifest = candidate.source_manifest(repo)
    path = driver.root / 'reviews/7/batch.json'
    objects = path.parent / 'workers'; objects.mkdir(parents=True)
    refs = {}
    for id in ('1', '2'):
        execution = dict(task_id=int(id), profile='test')
        request = dict(id=7, kind='Worker', task_execution=execution)
        private = dict(in_flight_request=request, worker_execution=execution, dispatch=None)
        assignment = dict(request=request, compiler='managed', scope=dict(nodes=['A']),
                          base_snapshot=candidate.digest(candidate.canonical_json(manifest)))
        value = dict(assignment=assignment, private_state=private)
        data = candidate.canonical_json(value)
        checksum = candidate.digest(data)
        refs[id] = dict(format=batch_storage.WORKER_FORMAT, bytes=len(data), sha256=checksum)
        with zipfile.ZipFile(objects / (checksum + '.zip'), 'w', zipfile.ZIP_DEFLATED) as archive:
            archive.writestr('worker.json', data)
    saved = dict(base_sha256='a' * 64, prepared=dict(format=batch_storage.FORMAT,
        workers=refs, receipt=dict(response_sha256='original-response')))
    path.write_bytes(candidate.canonical_json(saved))
    return driver, path, batch_storage.workers(saved['prepared'], objects)


def test_all_sources_and_bindings_captured_before_commit_cache_expands_later(project):
    driver, path, workers = make_batch(project)
    driver._materialize_batch(workers, path=path, committed=False)
    original = (driver.repo / 'Tablet/A.lean').read_bytes()
    cache = driver.root / 'batch-caches/7' / ('a' * 64)
    for id in workers:
        target = driver.root / id
        assert (target / 'repo/Tablet/A.lean').read_bytes() == original
        assert not (target / 'repo/.lake/build').exists()
        batch_storage.adopt_capture(target, workers[id])
        assert not (target / 'build-cache-ready.json').exists()
    assert (cache / 'repo/.lake/build/project.olean').read_bytes() == b'build cache'
    # Publication between reservation and launch cannot change original inputs.
    (driver.repo / 'Tablet/A.lean').write_text('later publication')
    (driver.repo / '.lake/build/project.olean').write_bytes(b'later build')
    driver._materialize_batch(workers, path=path, committed=True)
    driver._hydrate_worker_cache(dict(id=1))
    private = driver.root / '1/repo'
    assert (private / 'Tablet/A.lean').read_bytes() == original
    assert (private / '.lake/build/project.olean').read_bytes() == b'build cache'
    assert (private / '.lake/build/project.olean').stat().st_ino != (cache / 'repo/.lake/build/project.olean').stat().st_ino
    # Readiness permits adoption and never seeds over worker edits again.
    (private / 'Tablet/A.lean').write_text('worker proof edit')
    (private / '.lake/build/project.olean').write_bytes(b'worker build')
    driver.preparation_capacity.blocked = True
    driver._hydrate_worker_cache(dict(id=1))
    assert (private / '.lake/build/project.olean').read_bytes() == b'worker build'
    assert driver.worker_preparation_ready(dict(id=1))
    assert not driver.worker_preparation_ready(dict(id=2))


@pytest.mark.parametrize('fault', ['missing', 'artifact', 'receipt', 'symlink'])
def test_missing_or_changed_capture_never_recaptures_current_sources(project, fault):
    driver, path, workers = make_batch(project)
    driver._materialize_batch(workers, path=path, committed=False)
    root = driver.root / 'batch-caches/7' / ('a' * 64)
    if fault == 'missing':
        (root / 'capture.json').unlink()
    elif fault == 'artifact':
        (root / 'repo/.lake/build/project.olean').write_bytes(b'corrupt')
    elif fault == 'receipt':
        (root / 'capture.json').write_text('{}')
    else:
        p = root / 'repo/.lake/build/project.olean'; p.unlink(); p.symlink_to(driver.repo / '.lake/build/project.olean')
    with pytest.raises((OSError, ValueError, RuntimeError)):
        driver._hydrate_worker_cache(dict(id=1))
    assert not (driver.root / '1/build-cache-ready.json').exists()
    assert not list((driver.root / '1').rglob('recipe.json'))


def test_deferred_storage_is_not_completion_and_both_launch_paths_hydrate_first(project):
    driver, path, workers = make_batch(project)
    driver._materialize_batch(workers, path=path, committed=False)
    driver.preparation_capacity.blocked = True
    def submission(directory, **kwargs):
        assert (directory / 'build-cache-ready.json').exists()
        assert kwargs['worker_turn']['turn'] == 1
        raise RuntimeError('submission process boundary reached')
    driver._process = submission
    task = dict(id=1, worker_attempt=dict(turn=1, allowance_ms=1000, consumed_ms=0),
                assignment={'compiler': 'managed'})
    with pytest.raises(PreparationDeferred):
        asyncio.run(driver.launch_or_adopt(task))
    driver.preparation_capacity.blocked = False
    with pytest.raises(RuntimeError, match='submission process boundary'):
        asyncio.run(driver.launch_or_adopt(task))
    def legacy(directory, **kwargs):
        assert (directory / 'build-cache-ready.json').exists()
        raise RuntimeError('legacy process boundary reached')
    driver._process = legacy
    with pytest.raises(RuntimeError, match='legacy process boundary'):
        asyncio.run(driver.launch_or_adopt(dict(id=2, assignment={'compiler':'managed'},
                                              binding={'timeout_seconds':1})))


def test_capture_survives_review_cleanup_until_all_consumers_resolved(project):
    from trellis.dispatch.storage import terminal_directories, prune
    driver, path, workers = make_batch(project)
    driver._materialize_batch(workers, path=path, committed=False)
    cache = driver.root / 'batch-caches/7' / ('a' * 64)
    tasks = {id: dict(review_id=7, status=dict(status='running')) for id in workers}
    state = dict(dispatch=dict(tasks=tasks, verifiers={'jobs':{}}, receipts={'7':{}}))
    assert cache not in list(terminal_directories(driver.root, state))
    tasks['1']['status']['status'] = 'resolved'
    assert cache not in list(terminal_directories(driver.root, state))
    tasks['2']['status']['status'] = 'resolved'
    assert cache in list(terminal_directories(driver.root, state))
    prune(cache)
    assert (cache / 'capture.json').exists()  # Evidence retained; only caches removed.
    assert not (cache / 'repo/.lake').exists()


def test_metadata_admission_cannot_borrow_initial_workspace_lease(project):
    from trellis.dispatch.preparation_capacity import PreparationCapacity
    _, runtime, _ = project
    status = SimpleNamespace(f_bavail=20, f_frsize=1, f_favail=1000)
    gate = PreparationCapacity(runtime, runtime, floor_bytes=10, workspace_bytes=3,
        floor_inodes=1, workspace_inodes=1, statvfs=lambda _: status)
    with gate.admitted(1):
        with pytest.raises(PreparationDeferred):
            gate.require_allocation(11, 1)
    assert json.loads((runtime / 'dispatch/preparation-capacity.json').read_text())['allocation_bytes'] == 11


def test_restart_before_commit_uses_same_capture_and_remaining_allocations(project):
    driver, path, workers = make_batch(project)
    original = driver._materialize
    def crash(values, **kwargs):
        original({'1': values['1']}, **kwargs)
        raise RuntimeError('crash after first source capture')
    driver._materialize = crash
    with pytest.raises(RuntimeError, match='crash after'):
        driver._materialize_batch(workers, path=path, committed=False)
    marker = driver.root / 'batch-caches/7' / ('a' * 64) / 'capture.json'
    capture = marker.read_bytes()
    driver._materialize = original
    driver._materialize_batch(workers, path=path, committed=False)
    assert marker.read_bytes() == capture
    assert driver.preparation_capacity.calls[-1][0] < driver.preparation_capacity.calls[0][0]
    for id in workers:
        batch_storage.adopt_capture(driver.root / id, workers[id])


@pytest.mark.parametrize('shared', [False, True])
def test_real_batch_entry_commits_captured_same_base_below_fresh_workspace_floor(project, shared):
    from trellis.dispatch.preparation_capacity import PreparationCapacity
    driver, path, workers = make_batch(project)
    state = dict(dispatch=dict(tasks={}, receipts={}, config={'profiles': {'test':{}}}))
    (driver.runtime / 'protocol_state.json').write_bytes(candidate.canonical_json(state))
    saved = json.loads(path.read_text())
    response = {'assignments': [dict(id='closure-' + id, profile='test', compiler='managed',
        task_kind='lean_closure', review={}) for id in workers]}
    saved['base_sha256'] = candidate.digest(candidate.canonical_json(state))
    saved['prepared']['receipt']['response_sha256'] = candidate.digest(candidate.canonical_json(response))
    saved['prepared']['event'] = {'kind':'batch_prepared'}
    if shared:
        saved['build_cache_mode'] = 1
    path.write_bytes(candidate.canonical_json(saved))
    if shared:
        driver._materialize_batch(workers, path=path, committed=False)
    else:
        driver.preparation_capacity.require = lambda *_: None
        driver._materialize(workers)
    status = SimpleNamespace(f_bavail=12, f_frsize=1, f_favail=1000)
    gate = PreparationCapacity(driver.runtime, driver.repo, floor_bytes=10,
        workspace_bytes=3, floor_inodes=1, workspace_inodes=1, statvfs=lambda _:status)
    driver.preparation_capacity = driver.native.preparation_capacity = gate
    driver.native.observe_sources = lambda *_: dict(graph={}, input_hashes={}, manifest={})
    calls=[]
    def call(payload):
        assert payload['action'] == 'dispatch_control'
        calls.append(payload)
        return {}
    driver.native._call = call
    driver.native.dispatch_current_batch(7, response, driver.repo)
    assert len(calls) == 1
    assert calls[0]['event'] == saved['prepared']['event']
    if not shared:
        assert not (driver.root / 'batch-caches').exists()


def test_interrupted_complete_project_cache_needs_no_second_full_allocation(project, monkeypatch):
    from trellis.dispatch.preparation_capacity import PreparationCapacity
    driver, path, workers = make_batch(project)
    driver._materialize_batch(workers, path=path, committed=False)
    status = SimpleNamespace(f_bavail=1 << 30, f_frsize=1, f_favail=1000000)
    gate = PreparationCapacity(driver.runtime, driver.repo, floor_bytes=10000,
        workspace_bytes=1 << 20, floor_inodes=10, workspace_inodes=100,
        statvfs=lambda _: status)
    driver.preparation_capacity = gate
    original = workspace._copy_tree
    def stop_after_copy(source, destination, **kwargs):
        original(source, destination, **kwargs)
        if destination == driver.root / '1/repo/.lake/build':
            raise RuntimeError('power loss after copy')
    monkeypatch.setattr(workspace, '_copy_tree', stop_after_copy)
    with pytest.raises(RuntimeError, match='power loss'):
        driver._hydrate_worker_cache(dict(id=1))
    status.f_bavail = 10001  # Floor + 1, far below the original 1 MiB lease.
    monkeypatch.setattr(workspace, '_copy_tree', original)
    driver._hydrate_worker_cache(dict(id=1))
    assert (driver.root / '1/build-cache-ready.json').exists()


def test_interrupted_authority_seed_is_completed_before_ready(project, monkeypatch):
    from trellis.atomic_actions import observations as obs
    repo, _, _ = project
    artifact = repo / '.trellis/supervisor/repo/.lake/build/lib/lean/Tablet/A.olean'
    artifact.parent.mkdir(parents=True)
    artifact.write_bytes(b'unit-fixture-compiled-cache')
    class Snapshot:
        def __init__(self, *_): pass
        def materialization_order(self, nodes): return nodes
        def is_current(self): return True
    monkeypatch.setattr(obs, 'SourceClosureSnapshot', Snapshot)
    monkeypatch.setattr(obs, 'olean_is_content_current', lambda *a, **kw: True)
    monkeypatch.setattr(obs, 'olean_has_current_kernel_replay', lambda *a, **kw: True)
    monkeypatch.setattr(obs, '_read_olean_provenance', lambda *a, **kw: {})
    driver, path, workers = make_batch(project)
    driver._materialize_batch(workers, path=path, committed=False)
    destination = driver.root / '1/repo/.trellis/supervisor/repo/.lake/build/lib/lean/Tablet/A.olean'
    original = workspace._copy_build_file
    def interrupt(source, target, **kwargs):
        if Path(target) == destination:
            Path(target).write_bytes(b'partial')
            raise RuntimeError('interrupted supervisor seed')
        return original(source, target, **kwargs)
    monkeypatch.setattr(workspace, '_copy_build_file', interrupt)
    with pytest.raises(RuntimeError, match='interrupted supervisor'):
        driver._hydrate_worker_cache(dict(id=1))
    assert destination.exists() and not (driver.root / '1/build-cache-ready.json').exists()
    monkeypatch.setattr(workspace, '_copy_build_file', original)
    driver._hydrate_worker_cache(dict(id=1))
    assert destination.read_bytes() == artifact.read_bytes()
    assert json.loads((driver.root / '1/build-cache-ready.json').read_text())['seeded_oleans'] == 1


@pytest.mark.parametrize('fault', ['json', 'shape', 'symlink', 'worker_pin'])
def test_collector_retains_bad_or_pinned_optional_capture_without_stopping_snapshot(project, fault):
    from trellis.dispatch.storage import terminal_directories
    driver, path, workers = make_batch(project)
    driver._materialize_batch(workers, path=path, committed=False)
    root = driver.root / 'batch-caches/7' / ('a' * 64)
    marker = root / 'capture.json'
    if fault == 'json': marker.write_text('{')
    elif fault == 'shape': marker.write_text('{}')
    elif fault == 'symlink':
        marker.unlink(); marker.symlink_to('/missing-optional-cache')
    else: (driver.root / '1/retain-builds.json').write_text('{}')
    state = dict(dispatch=dict(tasks={id:dict(review_id=7,status={'status':'resolved'}) for id in workers},
                               receipts={'7':{}},verifiers={'jobs':{}}))
    paths = list(terminal_directories(driver.root, state))
    assert root not in paths
    assert driver.root / 'review-7' in paths


def test_final_prepare_crash_resumes_auxiliary_carriers_at_real_batch_entry(project):
    driver, path, workers = make_batch(project)
    state = dict(dispatch=dict(tasks={}, receipts={}, config={'profiles': {'test':{}}}))
    (driver.runtime / 'protocol_state.json').write_bytes(candidate.canonical_json(state))
    response = {'assignments': [dict(id='closure-' + id, profile='test', compiler='managed',
        task_kind='lean_closure', review={}) for id in workers]}
    saved = json.loads(path.read_text())
    saved.update(base_sha256=candidate.digest(candidate.canonical_json(state)), build_cache_mode=1)
    saved['prepared']['receipt']['response_sha256'] = candidate.digest(candidate.canonical_json(response))
    saved['prepared']['event'] = {'kind': 'batch_prepared'}
    path.write_bytes(candidate.canonical_json(saved))
    driver.native.observe_sources = lambda *_: dict(graph={}, input_hashes={}, manifest={})
    calls = []
    driver.native._call = lambda payload: calls.append(payload) or {}
    original = driver._prepare
    def interrupt(destination, *args, **kwargs):
        result = original(destination, *args, **kwargs)
        if destination.name == '2':
            raise RuntimeError('crash after final prepare before assignment and handoff')
        return result
    driver._prepare = interrupt
    with pytest.raises(RuntimeError, match='crash after final prepare'):
        driver.native.dispatch_current_batch(7, response, driver.repo)
    assert not calls
    assert (driver.root / '2/prepared.json').exists()
    assert not (driver.root / '2/assignment.json').exists()
    assert not (driver.root / '2/worker-handoffs.json').exists()
    assert not (driver.root / '2/carrier-ready.json').exists()
    first = {name: (driver.root / '1' / name).read_bytes()
             for name in (*batch_storage.CAPTURE_RECORDS, 'carrier-ready.json')}
    driver._prepare = original
    driver.native.dispatch_current_batch(7, response, driver.repo)
    assert len(calls) == 1 and calls[0]['action'] == 'dispatch_control'
    for id in workers:
        batch_storage.adopt_capture(driver.root / id, workers[id])
    assert first == {name: (driver.root / '1' / name).read_bytes() for name in first}
    assert driver.preparation_capacity.calls[-1][0] < driver.preparation_capacity.calls[0][0]


def test_same_cache_path_changed_identity_requires_replacement_allocation(project):
    driver, path, workers = make_batch(project)
    driver._materialize_batch(workers, path=path, committed=False)
    before = (driver.root / '1/prepared.json').read_bytes()
    # Canonical config can differ even though this saved native batch path stays
    # the same. The prior source/config capture cannot count as current allocation.
    config = json.loads(driver.config_path.read_text())
    config['test_new_config_field'] = True
    driver.config_path.write_bytes(candidate.canonical_json(config))
    driver.preparation_capacity.blocked = True
    with pytest.raises(PreparationDeferred):
        driver._materialize_batch(workers, path=path, committed=False)
    assert (driver.root / '1/prepared.json').read_bytes() == before
    assert not (driver.root / 'unused-preparations').exists()
    assert driver.preparation_capacity.calls[-1][0] >= driver.preparation_capacity.workspace_bytes


def test_missing_preparation_does_not_hydrate_before_capacity_admission(project, monkeypatch):
    driver, path, workers = make_batch(project)
    driver.preparation_capacity.blocked = True
    monkeypatch.setattr(batch_storage, 'read_worker',
        lambda *args: pytest.fail('absent preparation must not hydrate a worker'))
    with pytest.raises(PreparationDeferred):
        driver._materialize_batch(workers, path=path, committed=False)
    assert not any((driver.root / id / 'prepared.json').exists() for id in workers)


def test_existing_preparation_still_hydrates_and_checks_identity(project, monkeypatch):
    driver, path, workers = make_batch(project)
    marker = driver.root / '1/prepared.json'
    marker.parent.mkdir()
    marker.write_text(json.dumps({'identity': 'changed'}))
    original = marker.read_bytes()
    calls = []
    read_worker = batch_storage.read_worker
    def observed(root, reference, id):
        calls.append(id)
        return read_worker(root, reference, id)
    monkeypatch.setattr(batch_storage, 'read_worker', observed)
    driver.preparation_capacity.blocked = True
    with pytest.raises(PreparationDeferred):
        driver._materialize_batch(workers, path=path, committed=False)
    assert calls == ['1']
    assert marker.read_bytes() == original
    assert not (driver.root / 'unused-preparations').exists()


def test_captured_handoff_changes_refuse_before_native_adoption(project):
    driver, path, workers = make_batch(project)
    driver._materialize_batch(workers, path=path, committed=False)
    (driver.root / '1/worker-handoffs.json').write_text('{}')
    with pytest.raises(RuntimeError, match='carrier records changed'):
        driver._materialize_batch(workers, path=path, committed=True)


def test_restart_after_assignment_before_seal_does_not_reallocate_assignment(project, monkeypatch):
    driver, path, workers = make_batch(project)
    driver._materialize_batch(workers, path=path, committed=False)
    (driver.root / '2/carrier-ready.json').unlink()
    original = (driver.root / '2/assignment.json').stat()
    from trellis.dispatch import driver as driver_module
    write = driver_module.atomic
    def guarded_write(path, data):
        assert Path(path).name != 'assignment.json', 'already captured assignment must not allocate again'
        return write(path, data)
    monkeypatch.setattr(driver_module, 'atomic', guarded_write)
    driver._materialize_batch(workers, path=path, committed=False)
    assert (driver.root / '2/assignment.json').stat() == original
    assert driver.preparation_capacity.calls[-1] == (4096, 1)
    batch_storage.adopt_capture(driver.root / '2', workers['2'])
