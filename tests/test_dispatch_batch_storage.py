import hashlib
import json
from pathlib import Path
import weakref
import zipfile

import pytest

from trellis.dispatch import batch_storage as storage
from trellis.dispatch.candidate import canonical_json
from trellis.dispatch.native import NativeLedger


def worker(task_id):
    context = dict(task_id=task_id, profile='test')
    request = dict(task_execution=context, reviewer_comments='exact λ \n evidence')
    return dict(assignment=dict(request=request, scope=dict(nodes=[f'A{task_id}'])),
                private_state=dict(in_flight_request=request, worker_execution=context,
                                   dispatch=None, retained='captured, not current'))


def publish(root, value, *, extra=False):
    root.mkdir(parents=True, exist_ok=True)
    raw = json.dumps(value, separators=(',', ':'), ensure_ascii=False).encode()
    reference = dict(format=storage.WORKER_FORMAT, sha256=hashlib.sha256(raw).hexdigest(), bytes=len(raw))
    with zipfile.ZipFile(root / (reference['sha256'] + '.zip'), 'w', zipfile.ZIP_DEFLATED, compresslevel=1) as archive:
        archive.writestr('worker.json', raw)
        if extra:
            archive.writestr('unexpected', 'x')
    return reference


def test_stored_worker_preserves_canonical_identity_and_refuses_wrong_execution(tmp_path):
    original = worker(8)
    reference = publish(tmp_path, original)
    actual = storage.read_worker(tmp_path, reference, '8')
    assert canonical_json(actual) == canonical_json(original)
    with pytest.raises(RuntimeError, match='wrong task'):
        storage.read_worker(tmp_path, reference, '9')
    changed = worker(8)
    changed['private_state']['in_flight_request'] = {'task_execution': {'task_id': 9}}
    with pytest.raises(RuntimeError, match='wrong task'):
        storage.read_worker(tmp_path, publish(tmp_path, changed), '8')


@pytest.mark.parametrize('fault', ['missing', 'digest', 'size', 'kind', 'extra', 'symlink', 'parent_alias', 'bound'])
def test_stored_worker_objects_fail_closed(tmp_path, monkeypatch, fault):
    root = tmp_path / 'workers'
    reference = publish(root, worker(1), extra=fault == 'extra')
    path = root / (reference['sha256'] + '.zip')
    if fault == 'missing':
        path.unlink()
    elif fault == 'digest':
        with zipfile.ZipFile(path, 'w') as archive:
            raw = json.dumps(worker(1), separators=(',', ':'), ensure_ascii=False).encode().replace(b'captured', b'CAPTURED')
            archive.writestr('worker.json', raw)
    elif fault == 'size':
        reference['bytes'] += 1
    elif fault == 'kind':
        reference['format'] = 'trellis-dispatch-request/1'
    elif fault == 'symlink':
        target = tmp_path / 'outside.zip'
        path.rename(target)
        path.symlink_to(target)
    elif fault == 'parent_alias':
        alias = tmp_path / 'alias'
        alias.symlink_to(root, target_is_directory=True)
        root = alias
    elif fault == 'bound':
        monkeypatch.setattr(storage, 'MAX_WORKER_BYTES', reference['bytes'] - 1)
    with pytest.raises((RuntimeError, OSError)):
        storage.read_worker(root, reference, '1')


@pytest.mark.parametrize('consumer', ['mapping', 'driver'])
def test_lazy_workers_do_not_decode_unvisited_peers_or_retain_previous(tmp_path, monkeypatch, consumer):
    references = {str(i): publish(tmp_path, worker(i)) for i in range(1, 9)}
    actual_read = storage.read_worker
    live, visited = [], []
    class Observed(dict):
        pass
    def measured(*args):
        assert all(ref() is None for ref in live), 'previous worker retained during next hydration'
        value = Observed(actual_read(*args))
        live.append(weakref.ref(value))
        visited.append(args[-1])
        return value
    monkeypatch.setattr(storage, 'read_worker', measured)
    mapping = storage.workers(dict(format=storage.FORMAT, workers=references), tmp_path)
    assert len(mapping) == 8 and visited == []
    if consumer == 'driver':
        from trellis.dispatch.driver import ProductionDriver
        driver = ProductionDriver.__new__(ProductionDriver)
        driver.root = tmp_path / 'dispatch'; driver.root.mkdir()
        reads = []
        def state():
            reads.append(True)
            return dict(dispatch=dict(tasks={}))
        driver.state = state
        driver._prepare = lambda directory, *a, **kw: directory.mkdir()
        driver._materialize(mapping)
        assert reads == [True], 'one canonical state for the whole batch'
    else:
        for id, value in mapping.items():
            assert value == worker(int(id))
            del value
    assert visited == list(references)
    assert all(ref() is None for ref in live)


def test_stored_batch_recovers_captured_workers_before_single_reservation(tmp_path):
    runtime, repo = tmp_path / 'runtime', tmp_path / 'repo'
    runtime.mkdir(); repo.mkdir()
    state = dict(version=1, dispatch=dict(receipts={}))
    state_path = runtime / 'protocol_state.json'
    state_path.write_bytes(canonical_json(state))
    calls, materialized = [], {}
    interrupted = [True]
    receipt = {}
    def kernel(payload):
        calls.append(payload['action'])
        if payload['action'] == 'dispatch_prepare_batch':
            object_root = Path(payload['storage']['object_root'])
            receipt.update(response_sha256=payload['response_sha256'], decisions=[{'task_id': 1}, {'task_id': 2}])
            return dict(status='dispatch_ok', output=dict(format=storage.FORMAT, receipt=receipt,
                event={'kind': 'batch_prepared', 'retained_pre_binding': 'exact'},
                workers={str(i): publish(object_root, worker(i)) for i in (1, 2)}))
        assert set(materialized) == {'1', '2'}, 'all original source preparations precede reserve'
        state['dispatch']['receipts']['7'] = receipt.copy()
        state_path.write_bytes(canonical_json(state))
        return dict(status='dispatch_ok', output={})
    def materialize(workers, *, committed=False):
        for id, prepared in workers.items():
            encoded = canonical_json(prepared)
            assert materialized.setdefault(id, encoded) == encoded
            del prepared
            if interrupted[0]:
                interrupted[0] = False
                raise RuntimeError('crash during first materialization')
    ledger = NativeLedger(runtime, kernel=kernel, materialize=materialize)
    def dispatch():
        with ledger.locked():
            return ledger._dispatch_batch(7, {'assignments': []}, [], {}, repo=repo)
    with pytest.raises(RuntimeError, match='crash during'):
        dispatch()
    saved = json.loads((runtime / 'dispatch/reviews/7/batch.json').read_text())
    assert saved['prepared']['format'] == storage.FORMAT
    assert 'private_state' not in canonical_json(saved).decode()
    assert calls == ['dispatch_prepare_batch']
    result = dispatch()
    assert calls == ['dispatch_prepare_batch', 'dispatch_control']
    state['version'] = 2  # Peer published after reservation.
    state_path.write_bytes(canonical_json(state))
    assert dispatch() == result
    assert calls == ['dispatch_prepare_batch', 'dispatch_control']
    assert materialized == {str(i): canonical_json(worker(i)) for i in (1, 2)}
    # The committed receipt cannot make a missing source object optional.
    object_root = runtime / 'dispatch/reviews/7/workers'
    (object_root / (result['workers']['2']['sha256'] + '.zip')).unlink()
    with pytest.raises(FileNotFoundError):
        dispatch()
    assert calls == ['dispatch_prepare_batch', 'dispatch_control']


def test_old_inline_batches_and_unknown_versions(tmp_path):
    inline = {'workers': {'1': worker(1)}}
    assert storage.workers(inline, tmp_path) is inline['workers']
    with pytest.raises(RuntimeError, match='unsupported'):
        storage.workers(dict(format='future', workers={}), tmp_path)


@pytest.mark.parametrize('fault', ['missing', 'corrupt'])
def test_bad_worker_object_prevents_any_reservation(tmp_path, fault):
    state = dict(dispatch=dict(receipts={}))
    (tmp_path / 'protocol_state.json').write_bytes(canonical_json(state))
    calls = []
    def kernel(payload):
        calls.append(payload['action'])
        assert payload['action'] == 'dispatch_prepare_batch', 'must not commit an incomplete preparation'
        root = Path(payload['storage']['object_root'])
        reference = publish(root, worker(1))
        path = root / (reference['sha256'] + '.zip')
        if fault == 'missing':
            path.unlink()
        else:
            path.write_bytes(b'not a ZIP object')
        return dict(status='dispatch_ok', output=dict(format=storage.FORMAT,
            receipt=dict(response_sha256=payload['response_sha256']), event={}, workers={'1': reference}))
    ledger = NativeLedger(tmp_path, kernel=kernel, materialize=lambda workers: list(workers.items()))
    with pytest.raises((OSError, zipfile.BadZipFile)):
        ledger._dispatch_batch(1, {}, [], {}, repo=tmp_path)
    assert calls == ['dispatch_prepare_batch']


def test_committed_materialization_adopts_original_sources_despite_current_config_and_worker_edits(tmp_path, monkeypatch):
    from trellis.dispatch import candidate
    from trellis.dispatch.driver import ProductionDriver
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = tmp_path / 'dispatch'; driver.root.mkdir()
    driver.repo = tmp_path / 'canonical'; driver.repo.mkdir()
    (driver.repo / 'Tablet').mkdir()
    (driver.repo / 'Tablet/A.lean').write_text('new peer publication')
    driver.config_path = driver.repo / 'trellis.config.json'
    driver.config_path.write_text('{"new_deployment":true}')
    driver.state = lambda: pytest.fail('committed adoption must not reload canonical state')
    driver._prepare = lambda *a, **kw: pytest.fail('committed adoption must not prepare again')
    value = worker(1)
    value['private_state']['in_flight_request']['id'] = 12
    destination = driver.root / '1'; destination.mkdir()
    manifest = {'Tablet/A.lean': candidate.digest(b'original source')}
    value['assignment']['base_snapshot'] = candidate.digest(canonical_json(manifest))
    (destination / 'repo/Tablet').mkdir(parents=True)
    (destination / 'repo/Tablet/A.lean').write_text('worker edits are allowed')
    (destination / 'runtime').mkdir()
    (destination / 'runtime/protocol_state.json').write_text('{"advanced_by_worker":true}')
    old_config = b'{"original_config":true}'
    (destination / 'repo/trellis.config.json').write_bytes(old_config)
    (destination / 'base-manifest.json').write_bytes(canonical_json(manifest))
    (destination / 'assignment.json').write_bytes(canonical_json(value['assignment']))
    marker = dict(request_id=12, prepared_config_sha256=candidate.digest(old_config))
    (destination / 'prepared.json').write_bytes(canonical_json(marker))
    before = {p: p.read_bytes() for p in destination.rglob('*') if p.is_file()}
    monkeypatch.setattr(candidate, 'source_manifest', lambda *_: pytest.fail('recaptured canonical sources'))
    driver._materialize({'1': value}, committed=True)
    assert before == {p: p.read_bytes() for p in before}
    for relative in ('base-manifest.json', 'assignment.json', 'prepared.json', 'repo/trellis.config.json'):
        path = destination / relative
        raw = path.read_bytes()
        path.write_text('{}')
        with pytest.raises((RuntimeError, KeyError)):
            driver._materialize({'1': value}, committed=True)
        path.unlink()
        with pytest.raises(FileNotFoundError):
            driver._materialize({'1': value}, committed=True)
        path.write_bytes(raw)
    assert not (driver.root / 'unused-preparations').exists()


def test_stale_uncommitted_compact_batch_reprepares_before_reserve(tmp_path):
    runtime = tmp_path / 'runtime'; runtime.mkdir()
    state_path = runtime / 'protocol_state.json'
    state_path.write_bytes(canonical_json(dict(version=1, dispatch=dict(receipts={}))))
    captured, prepares = [], []
    fail = [True]
    def kernel(payload):
        if payload['action'] == 'dispatch_prepare_batch':
            version = payload['state']['version']; prepares.append(version)
            value = worker(1); value['private_state']['retained'] = str(version)
            return dict(status='dispatch_ok', output=dict(format=storage.FORMAT,
                receipt=dict(response_sha256=payload['response_sha256']), event=dict(version=version),
                workers={'1': publish(Path(payload['storage']['object_root']), value)}))
        if fail[0]:
            raise RuntimeError('before reservation')
        assert payload['event']['version'] == 2
        return dict(status='dispatch_ok', output={})
    def materialize(workers):
        captured.append(workers['1']['private_state']['retained'])
    ledger = NativeLedger(runtime, kernel=kernel, materialize=materialize)
    with pytest.raises(RuntimeError, match='before reservation'):
        ledger._dispatch_batch(7, {}, [], {}, repo=tmp_path)
    state_path.write_bytes(canonical_json(dict(version=2, dispatch=dict(receipts={}))))
    fail[0] = False
    ledger._dispatch_batch(7, {}, [], {}, repo=tmp_path)
    assert prepares == [1, 2] and captured == ['1', '2']
