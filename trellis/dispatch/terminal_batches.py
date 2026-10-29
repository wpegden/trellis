"""Reader-first retirement of obsolete, fully resolved batch preparation inputs.

The compact acknowledgement never reserves or materializes a worker. Active,
uncertain and uncommitted batches keep their original preparation records.
"""
import copy
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import zipfile

from .candidate import canonical_json, digest, _sync_dir
from .publication import atomic
from . import batch_projection, batch_storage, retained_files as retained
from . import terminal_retention as terminal

FORMAT = 'trellis-terminal-batch/1'
MAX_METADATA_BYTES = 8 * 1024 * 1024


def _ids(receipt, review_id):
    if (not isinstance(receipt, dict) or set(receipt) != {'review_id', 'response_sha256', 'decisions'}
            or type(receipt['review_id']) is not int or receipt['review_id'] != review_id
            or not isinstance(receipt['response_sha256'], str)
            or re.fullmatch('[0-9a-f]{64}', receipt['response_sha256']) is None
            or not isinstance(receipt['decisions'], list)):
        raise ValueError('invalid native batch receipt')
    ids, assignments = [], set()
    for decision in receipt['decisions']:
        if (not isinstance(decision, dict) or set(decision) != {'assignment_id', 'task_id', 'rejection'}
                or not isinstance(decision['assignment_id'], str)):
            raise ValueError('invalid batch decision')
        task_id = decision['task_id']
        if task_id is None:
            if not isinstance(decision['rejection'], str):
                raise ValueError('rejected decision lacks reason')
        else:
            if type(task_id) is not int or task_id < 1 or decision['rejection'] is not None:
                raise ValueError('invalid accepted batch decision')
            if str(task_id) in ids or decision['assignment_id'] in assignments:
                raise ValueError('duplicate accepted batch decision')
            ids.append(str(task_id)); assignments.add(decision['assignment_id'])
    return sorted(ids, key=int)


def observation(state, review_id):
    """Small stable current/Git projection; no payload hashes on scheduler thread."""
    ledger = state['dispatch']
    receipt = ledger.get('receipts', {}).get(str(review_id))
    if receipt is None:
        return None
    ids = _ids(receipt, review_id)
    if (ledger.get('reviewer') or {}).get('request_id') == review_id:
        return None
    tasks = {}
    for decision in receipt['decisions']:
        if decision['task_id'] is None:
            continue
        key = str(decision['task_id']); task = ledger['tasks'].get(key)
        if (task is None or task.get('id') != decision['task_id']
                or task.get('review_id') != review_id
                or task.get('assignment', {}).get('assignment_id') != decision['assignment_id']
                or task.get('status', {}).get('status') != 'resolved'):
            return None
        tasks[key] = terminal.task_identity(task)
    return dict(receipt=copy.deepcopy(receipt), task_ids=ids, tasks=copy.deepcopy(tasks))


def observations(state):
    result = {}
    for key in state['dispatch'].get('receipts', {}):
        try:
            value = observation(state, int(key))
        except (KeyError, TypeError, ValueError):
            continue  # Unsupported old records do not acquire new authority.
        if value is not None:
            result[key] = value
    return result


def _compact(record, review_id):
    if (not isinstance(record, dict) or set(record) != {'format', 'receipt', 'task_ids', 'tasks', 'checkpoint', 'objects'}
            or record['format'] != FORMAT
            or record['task_ids'] != _ids(record['receipt'], review_id)
            or not isinstance(record['tasks'], dict) or set(record['tasks']) != set(record['task_ids'])
            or not isinstance(record['checkpoint'], str)
            or re.fullmatch('[0-9a-f]{40}|[0-9a-f]{64}', record['checkpoint']) is None
            or not isinstance(record['objects'], dict)
            or (record['objects'] and set(record['objects']) != set(record['task_ids']))):
        raise ValueError('invalid compact terminal batch')
    _objects(record['objects'])
    return {key: record[key] for key in ('receipt', 'task_ids', 'tasks')}


def acknowledge(record, state, review_id, response_hash):
    expected = _compact(record, review_id)
    if record['receipt']['response_sha256'] != response_hash:
        raise RuntimeError('review ID already has a different terminal response')
    current = observation(state, review_id)
    if current is None or canonical_json(current) != canonical_json(expected):
        raise RuntimeError('terminal batch no longer matches current committed resolved tasks')
    # No fabricated prepared workers or graph: this is explicitly a no-launch ack.
    return dict(format=FORMAT, terminal=True, receipt=record['receipt'], task_ids=record['task_ids'])


def _objects(workers):
    objects = {}
    for key, ref in workers.items():
        if (not isinstance(key, str) or not re.fullmatch('[1-9][0-9]*', key)
                or not isinstance(ref, dict) or set(ref) != {'format', 'sha256', 'bytes'}
                or ref['format'] != batch_storage.WORKER_FORMAT
                or not isinstance(ref['sha256'], str) or re.fullmatch('[0-9a-f]{64}', ref['sha256']) is None
                or type(ref['bytes']) is not int or not 0 < ref['bytes'] <= batch_storage.MAX_WORKER_BYTES):
            raise ValueError('invalid stored terminal worker reference')
        objects[key] = ref
    return objects


def _projection(path, cancelled):
    with retained.regular_path(path).open('r', encoding='utf-8') as stream:
        value = batch_projection.read(stream, cancelled=cancelled)
    if len(canonical_json(value)) > MAX_METADATA_BYTES:
        raise ValueError('batch projected metadata exceeds bound')
    return value


def _verified_object(path, reference, cancelled):
    """Verify original compressed capture bytes without decoding private JSON."""
    before = retained._signature(retained.regular_path(path).stat())
    with zipfile.ZipFile(path) as archive:
        entries = archive.infolist()
        if (len(entries) != 1 or entries[0].filename != 'worker.json'
                or entries[0].is_dir() or entries[0].file_size != reference['bytes']):
            raise ValueError('terminal worker archive has wrong entry or length')
        count, hashed = 0, hashlib.sha256()
        with archive.open(entries[0]) as stream:
            while True:
                if cancelled():
                    raise InterruptedError('terminal batch verification cancelled')
                raw = stream.read(min(1 << 20, reference['bytes'] + 1 - count))
                if not raw:
                    break
                hashed.update(raw); count += len(raw)
                if count > reference['bytes']:
                    raise ValueError('terminal worker archive exceeds original length')
    if count != reference['bytes'] or hashed.hexdigest() != reference['sha256']:
        raise ValueError('terminal worker archive changed bytes')
    if retained._signature(retained.regular_path(path).stat()) != before:
        raise ValueError('terminal worker archive changed during verification')
    return before


def retire(collector, review_id, current, *, apply=False, runtime_admission=None):
    """Serialize preparation in the review lane; every mutation has a fresh fence.

    ``current`` supplies the latest small native observation for this review.
    Default product scheduling leaves this operation disabled until readers deploy.
    """
    from .auxiliary_retention import _parents, _binding
    result = dict(status='ineligible', compacted=0, removed_objects=0, removed_bytes=0,
                  removed_allocated_bytes=0, new_compact_allocated_bytes=0)
    if type(review_id) is not int or review_id < 0:
        raise ValueError('invalid terminal review ID')
    if collector.cancelled():
        return dict(result, status='cancelled')
    observed = current()
    if observed is None:
        return result
    if runtime_admission is not None:
        runtime_admission()
    folder = collector.root / 'reviews' / str(review_id)
    path = folder / 'batch.json'
    if not path.exists() and not path.is_symlink():
        return dict(result, status='absent')
    before = retained._signature(retained.regular_path(path).stat())
    parents = _parents(path, collector.root)
    witness = collector.witness()
    if witness.get('terminal_batches', {}).get(str(review_id)) != observed:
        return dict(result, status='not_committed')
    review = collector.root / f'review-{review_id}'
    reviewer = dict(sha256=digest(canonical_json(observed['receipt'])))
    owner_observations = {**observed['tasks'], review.name: reviewer}
    if witness['owners'].get(review.name) != reviewer:
        return dict(result, status='not_committed')
    guard = retained.terminal_owner_guard(collector.root, review, owner_observations)
    worker_guards = [retained.terminal_owner_guard(collector.root, collector.root / key, owner_observations)
                     for key in observed['task_ids']]
    binding = _binding(review, collector, witness, dict(kind='review', id=review_id))
    saved = _projection(path, collector.cancelled)
    compacted = saved.get('format') == FORMAT
    if compacted:
        if canonical_json(_compact(saved, review_id)) != canonical_json(observed):
            raise ValueError('compact batch identity differs')
        terminal._git(collector.repo, 'merge-base', '--is-ancestor', saved['checkpoint'], witness['commit'])
        record = saved
    else:
        prepared = saved.get('prepared', {})
        event = prepared.get('event', {})
        if (set(saved) - {'base_sha256', 'build_cache_mode', 'prepared'}
                or set(prepared) - {'event', 'receipt', 'workers', 'format'}
                or set(event) != {'kind', 'review_id', 'response_sha256', 'now_ms', 'graph', 'items'}
                or not isinstance(saved.get('base_sha256'), str)
                or re.fullmatch('[0-9a-f]{64}', saved['base_sha256']) is None
                or event.get('kind') != 'batch_prepared'
                or type(event.get('review_id')) is not int or event['review_id'] != review_id
                or event.get('response_sha256') != observed['receipt']['response_sha256']
                or type(event.get('now_ms')) is not int or event['now_ms'] < 0):
            raise ValueError('original batch event binding is invalid')
        if (canonical_json(prepared.get('receipt')) != canonical_json(observed['receipt'])
                or not isinstance(prepared.get('workers'), dict)
                or set(prepared['workers']) != set(observed['task_ids'])
                or (saved.get('build_cache_mode') is not None
                    and (type(saved['build_cache_mode']) is not int or saved['build_cache_mode'] != 1))):
            raise ValueError('original batch differs from terminal native receipt')
        format = prepared.get('format')
        if format == batch_storage.FORMAT:
            objects = _objects(prepared['workers'])
        elif format is None and all(value == {'assignment': None, 'private_state': None}
                                  for value in prepared['workers'].values()):
            objects = {}
        else:
            raise ValueError('unsupported original batch capture format')
        record = dict(format=FORMAT, **observed, checkpoint=witness['commit'], objects=objects)
    if retained._signature(retained.regular_path(path).stat()) != before:
        raise ValueError('batch changed during projection')
    if len(canonical_json(record)) > MAX_METADATA_BYTES:
        raise ValueError('terminal batch acknowledgement exceeds metadata bound')
    # Verify only exact referenced objects. Stray archives and all other evidence stay.
    candidates = {}
    for reference in record['objects'].values():
        target = folder / 'workers' / (reference['sha256'] + '.zip')
        if not target.exists() and not target.is_symlink():
            if not compacted:
                raise ValueError('original batch lost its worker object')
            continue  # Durable compact record precedes every permitted unlink.
        candidates[target] = (_verified_object(target, reference, collector.cancelled),
                              _parents(target, collector.root))
    if not retained._untracked(path) or any(not retained._untracked(p) for p in candidates):
        return dict(result, status='tracked')
    if not apply:
        return dict(result, status='eligible', objects=len(candidates), needs_compaction=not compacted)
    def fence():
        if collector.cancelled():
            raise InterruptedError('terminal batch retirement cancelled')
        if runtime_admission is not None:
            runtime_admission()
        if current() != observed or not collector.checkpoint_current(witness):
            raise ValueError('terminal batch current state or checkpoint changed')
        if any(collector._same_task_pending(int(key)) for key in observed['task_ids']):
            raise ValueError('terminal batch still has pending publication')
        guard()
        for check in worker_guards:
            check()
        if (list(retained._signature(retained.regular_path(review / 'prepared.json').stat()))
                != binding['prepared_signature'] or _parents(path, collector.root) != parents):
            raise ValueError('terminal batch preparation or parent changed')
    if not compacted:
        with collector.retirement_lock() as acquired:
            if not acquired:
                return dict(result, status='coordinator_busy')
            fence()
            if retained._signature(retained.regular_path(path).stat()) != before or not retained._untracked(path):
                raise ValueError('batch changed before compaction')
            info = path.stat()
            atomic(path, canonical_json(record))  # Durable ack before any capture object unlink.
            before = retained._signature(path.stat())
            result.update(compacted=1, removed_bytes=info.st_size,
                removed_allocated_bytes=info.st_blocks * 512,
                new_compact_allocated_bytes=path.stat().st_blocks * 512)
    if compacted:
        # An earlier atomic replacement may have stopped before its directory
        # fsync. Re-establish durable acknowledgement before resuming unlinks.
        with retained.regular_path(path).open('rb') as stream:
            os.fsync(stream.fileno())
        _sync_dir(folder)
        if retained._signature(retained.regular_path(path).stat()) != before:
            raise ValueError('compact batch changed during durability verification')
    dirty = {}
    try:
        for target, (signature, target_parents) in candidates.items():
            with collector.retirement_lock() as acquired:
                if not acquired:
                    return dict(result, status='coordinator_busy')
                fence()
                if (retained._signature(retained.regular_path(path).stat()) != before
                        or _parents(target, collector.root) != target_parents
                        or retained._signature(retained.regular_path(target).stat()) != signature
                        or not retained._untracked(target)):
                    raise ValueError('terminal worker object changed before unlink')
                info = target.stat(); target.unlink(); dirty[target.parent] = target_parents
                result['removed_objects'] += 1
                result['removed_bytes'] += info.st_size
                result['removed_allocated_bytes'] += info.st_blocks * 512
    finally:
        for parent, expected in dirty.items():
            if _parents(parent / '.sync-fence', collector.root) != expected:
                raise ValueError('terminal worker parent changed before sync')
            _sync_dir(parent)
    return dict(result, status='retired' if result['compacted'] or result['removed_objects'] else 'unchanged')
