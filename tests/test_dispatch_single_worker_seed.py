"""Direct single-worker capture keeps the existing reservation/replay boundary."""
import json

import pytest

from test_dispatch_batch_cache import make_batch
from test_dispatch_workspace import project
from trellis.dispatch import batch_cache, batch_storage, candidate
from trellis.dispatch.preparation_capacity import PreparationDeferred


def single(project):
    driver, path, workers = make_batch(project)
    saved = json.loads(path.read_bytes())
    saved['prepared']['workers'] = {'1': saved['prepared']['workers']['1']}
    saved['build_cache_mode'] = 1
    path.write_bytes(candidate.canonical_json(saved))
    # Existing fixture capacity records measured allocation; direct preparation
    # additionally uses the ordinary workspace-count admission interface.
    capacity = driver.preparation_capacity
    capacity.require = lambda count: capacity.require_allocation(
        count * capacity.workspace_bytes, count * capacity.workspace_inodes) if count else None
    return driver, path, batch_storage.workers(saved['prepared'], path.parent / 'workers')


def donor(driver, path):
    saved = json.loads(path.read_bytes())
    return driver.root / 'batch-caches' / path.parent.name / saved['base_sha256']


def capture_old_shared(driver, path, workers):
    saved = json.loads(path.read_bytes())
    root = donor(driver, path)
    binding = dict(base_sha256=saved['base_sha256'], review_id=int(path.parent.name),
                   receipt=saved['prepared']['receipt'], workers=workers.references)
    receipt = batch_cache.capture(root, driver.repo, binding)
    return root, batch_cache.reference(root, driver.root, receipt)


def native_entry(driver, path, workers):
    state = dict(dispatch=dict(tasks={}, receipts={}, config={'profiles': {'test': {}}}))
    (driver.runtime / 'protocol_state.json').write_bytes(candidate.canonical_json(state))
    response = {'assignments': [dict(id='one', profile='test', compiler='managed',
        task_kind='lean_closure', review={})]}
    saved = json.loads(path.read_bytes())
    saved.update(base_sha256=candidate.digest(candidate.canonical_json(state)), build_cache_mode=1)
    saved['prepared']['receipt']['response_sha256'] = candidate.digest(candidate.canonical_json(response))
    saved['prepared']['event'] = {'kind': 'batch_prepared'}
    path.write_bytes(candidate.canonical_json(saved))
    driver.native.observe_sources = lambda *_: dict(graph={}, input_hashes={}, manifest={})
    calls = []

    def call(payload):
        assert payload['action'] == 'dispatch_control'
        batch_storage.adopt_capture(driver.root / '1', workers['1'])
        calls.append(payload)
        return {}

    driver.native._call = call
    return state, response, saved, calls


def test_fresh_single_keeps_measured_admission_and_original_capture(project, monkeypatch):
    driver, path, workers = single(project)
    expected = batch_cache.allocation(driver.repo, workers, ['1'], new_capture=True,
        workspace_bytes=driver.preparation_capacity.workspace_bytes,
        workspace_inodes=driver.preparation_capacity.workspace_inodes)
    monkeypatch.setattr(batch_cache, 'capture', lambda *_: pytest.fail('single must not create a donor'))
    driver._materialize_batch(workers, path=path, committed=False)
    directory = driver.root / '1'
    assert driver.preparation_capacity.calls[0] == (expected['bytes'], expected['inodes'])
    assert not donor(driver, path).exists()
    assert 'deferred_build_cache' not in json.loads((directory / 'prepared.json').read_bytes())
    assert (directory / 'repo/.lake/build/project.olean').read_bytes() == b'build cache'
    batch_storage.adopt_capture(directory, workers['1'])
    assert not (directory / 'carrier-ready.json').exists()
    assert not (directory / 'build-cache-ready.json').exists()


def test_single_storage_refusal_precedes_hydration_or_workspace(project, monkeypatch):
    driver, path, workers = single(project)
    driver.preparation_capacity.blocked = True
    monkeypatch.setattr(batch_storage, 'read_worker', lambda *_: pytest.fail('no early private-state hydration'))
    with pytest.raises(PreparationDeferred):
        driver._materialize_batch(workers, path=path, committed=False)
    assert not (driver.root / '1').exists()
    assert not donor(driver, path).exists()


@pytest.mark.parametrize('crash', ['after_prepare', 'after_assignment', 'before_native_commit'])
def test_single_crash_replay_completes_original_records_before_reservation(project, monkeypatch, crash):
    driver, path, workers = single(project)
    state, response, saved, calls = native_entry(driver, path, workers)
    original_prepare = driver._prepare
    original_materialize = driver._materialize
    original_call = driver.native._call

    def after_prepare(*args, **kwargs):
        original_prepare(*args, **kwargs)
        raise RuntimeError('injected interruption')

    def after_assignment(*args, **kwargs):
        original_materialize(*args, **kwargs)
        raise RuntimeError('injected interruption')

    def before_commit(payload):
        batch_storage.adopt_capture(driver.root / '1', workers['1'])
        raise RuntimeError('injected interruption')

    if crash == 'after_prepare': driver._prepare = after_prepare
    elif crash == 'after_assignment': driver._materialize = after_assignment
    else: driver.native._call = before_commit
    with pytest.raises(RuntimeError, match='injected interruption'):
        driver.native.dispatch_current_batch(7, response, driver.repo)
    assert calls == []
    assert not donor(driver, path).exists()
    directory = driver.root / '1'
    frozen = {name: (directory/name).read_bytes() for name in
              ['prepared.json', 'base-manifest.json', 'runtime/protocol_state.json']}
    driver._prepare, driver._materialize, driver.native._call = original_prepare, original_materialize, original_call
    driver.native.dispatch_current_batch(7, response, driver.repo)
    assert len(calls) == 1
    assert frozen == {name: (directory/name).read_bytes() for name in frozen}
    assert not donor(driver, path).exists()
    # Actual committed native entry skips source observation/materialization.
    state['dispatch']['receipts']['7'] = saved['prepared']['receipt']
    (driver.runtime / 'protocol_state.json').write_bytes(candidate.canonical_json(state))
    (driver.repo / 'Tablet/A.lean').write_text('new canonical publication')
    (directory / 'repo/Tablet/A.lean').write_text('legitimate worker edit')
    monkeypatch.setattr(driver.native, 'observe_sources', lambda *_: pytest.fail('committed replay observes current sources'))
    driver.native.dispatch_current_batch(7, response, driver.repo)
    assert len(calls) == 1
    assert (directory / 'repo/Tablet/A.lean').read_text() == 'legitimate worker edit'


def test_single_adoption_failure_cannot_reserve(project, monkeypatch):
    driver, path, workers = single(project)
    _, response, _, calls = native_entry(driver, path, workers)
    original = driver._materialize

    def corrupt(*args, **kwargs):
        original(*args, **kwargs)
        (driver.root / '1/assignment.json').write_text('{}')

    monkeypatch.setattr(driver, '_materialize', corrupt)
    with pytest.raises(RuntimeError, match='original preparation binding'):
        driver.native.dispatch_current_batch(7, response, driver.repo)
    assert calls == []


@pytest.mark.parametrize('stage', ['donor_only', 'partial_carrier', 'complete_carrier'])
def test_existing_single_shared_capture_stays_on_original_recovery_path(project, stage):
    driver, path, workers = single(project)
    root, ref = capture_old_shared(driver, path, workers)
    receipt = (root / 'capture.json').read_bytes()
    if stage == 'partial_carrier':
        driver._prepare(driver.root / '1', workers['1']['private_state'], deferred_build_cache=ref)
    elif stage == 'complete_carrier':
        driver._materialize(workers, deferred_build_cache=ref)
    driver._materialize_batch(workers, path=path, committed=False)
    directory = driver.root / '1'
    assert json.loads((directory / 'prepared.json').read_bytes())['deferred_build_cache'] == ref
    assert (root / 'capture.json').read_bytes() == receipt
    batch_storage.validate_capture_seal(directory)
    assert not (directory / 'repo/.lake/build').exists()
    driver._hydrate_worker_cache(dict(id=1))
    assert (directory / 'repo/.lake/build/project.olean').read_bytes() == b'build cache'


@pytest.mark.parametrize('fault', ['missing_referenced_donor', 'unfinished_directory', 'broken_alias'])
def test_damaged_single_shared_history_is_not_replaced_by_direct_seed(project, fault):
    driver, path, workers = single(project)
    root = donor(driver, path)
    if fault == 'missing_referenced_donor':
        import shutil
        root, ref = capture_old_shared(driver, path, workers)
        driver._prepare(driver.root / '1', workers['1']['private_state'], deferred_build_cache=ref)
        shutil.rmtree(root)
    else:
        root.parent.mkdir(parents=True)
        if fault == 'unfinished_directory': root.mkdir()
        else: root.symlink_to(root.with_name('missing'))
    with pytest.raises((OSError, RuntimeError, candidate.CandidateError)):
        driver._materialize_batch(workers, path=path, committed=False)
    assert not (driver.root / '1/build-cache-ready.json').exists()


def test_complete_direct_single_replays_below_new_allocation_floor(project):
    driver, path, workers = single(project)
    driver._materialize_batch(workers, path=path, committed=False)
    before = list(driver.preparation_capacity.calls)
    driver.preparation_capacity.blocked = True
    driver._materialize_batch(workers, path=path, committed=False)
    assert driver.preparation_capacity.calls == before
    batch_storage.adopt_capture(driver.root / '1', workers['1'])
