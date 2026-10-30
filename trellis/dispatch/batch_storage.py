"""Verify and hydrate one immutable prepared worker at a time."""
from collections.abc import Mapping
import hashlib
import json
from pathlib import Path
import re
import stat
import zipfile

FORMAT = 'trellis-prepared-batch/1'
WORKER_FORMAT = 'trellis-prepared-worker/1'
MAX_WORKER_BYTES = 2 * 1024 * 1024 * 1024
CAPTURE_RECORDS = ('prepared.json', 'base-manifest.json', 'assignment.json', 'worker-handoffs.json')


def seal_capture(directory):
    """Publish readiness only after every original carrier record is durable."""
    from .acceptance_receipts import regular_digest
    from .candidate import canonical_json
    from .publication import atomic
    directory = Path(directory)
    binding = {name: regular_digest(directory / name) for name in CAPTURE_RECORDS}
    atomic(directory / 'carrier-ready.json', canonical_json(binding))


def validate_capture_seal(directory):
    from .acceptance_receipts import regular, regular_digest
    directory = Path(directory)
    binding = json.loads(regular(directory / 'carrier-ready.json'))
    if binding != {name: regular_digest(directory / name) for name in CAPTURE_RECORDS}:
        raise RuntimeError('batch carrier records changed after capture')


def read_worker(root, reference, task_id):
    if (not isinstance(reference, dict) or set(reference) != {'format', 'sha256', 'bytes'}
            or reference['format'] != WORKER_FORMAT
            or not isinstance(reference['sha256'], str)
            or re.fullmatch('[0-9a-f]{64}', reference['sha256']) is None
            or type(reference['bytes']) is not int
            or not 0 < reference['bytes'] <= MAX_WORKER_BYTES):
        raise RuntimeError('invalid prepared worker reference')
    path = Path(root) / (reference['sha256'] + '.zip')
    for parent in path.parents:
        if not stat.S_ISDIR(parent.lstat().st_mode):
            raise RuntimeError('prepared worker object parent is not a regular directory')
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode):
        raise RuntimeError('prepared worker object is not a regular file')
    if info.st_size > MAX_WORKER_BYTES + (1 << 20):
        raise RuntimeError('prepared worker archive exceeds storage bound')
    with zipfile.ZipFile(path) as archive:
        entries = archive.infolist()
        if (len(entries) != 1 or entries[0].filename != 'worker.json'
                or entries[0].is_dir() or entries[0].file_size != reference['bytes']):
            raise RuntimeError('prepared worker archive has wrong entry or length')
        with archive.open(entries[0]) as stream:
            raw = stream.read(reference['bytes'] + 1)
    if len(raw) != reference['bytes'] or hashlib.sha256(raw).hexdigest() != reference['sha256']:
        raise RuntimeError('prepared worker object digest or length changed')
    worker = json.loads(raw)
    del raw
    if not isinstance(worker, dict) or set(worker) != {'assignment', 'private_state'}:
        raise RuntimeError('invalid prepared worker object')
    request = worker['assignment']['request']
    private = worker['private_state']
    context = request.get('task_execution')
    if (not isinstance(context, dict) or context.get('task_id') != int(task_id)
            or private.get('worker_execution') != context
            or private.get('in_flight_request') != request
            or private.get('dispatch') is not None):
        raise RuntimeError('prepared worker object has wrong task or private request')
    return worker


def adopt_capture(directory, worker):
    """A committed batch already captured sources before reservation.

    Validate those host records, never recreate them from today's canonical
    sources/config or compare a worker's legitimately edited source/runtime to
    its starting snapshot. This also reads historical inline batches.
    """
    from .acceptance_receipts import regular, regular_digest
    from .candidate import canonical_json, digest
    directory = Path(directory)
    assignment = worker['assignment']
    prepared = json.loads(regular(directory / 'prepared.json'))
    if prepared.get('deferred_build_cache'):
        validate_capture_seal(directory)
    manifest = json.loads(regular(directory / 'base-manifest.json'))
    if (regular_digest(directory / 'assignment.json') != digest(canonical_json(assignment))
            or digest(canonical_json(manifest)) != assignment['base_snapshot']
            or prepared['request_id'] != worker['private_state']['in_flight_request']['id']
            or regular_digest(directory / 'repo/trellis.config.json') != prepared['prepared_config_sha256']):
        raise RuntimeError('committed batch original preparation binding changed; retain its evidence')


class StoredWorkers(Mapping):
    def __init__(self, root, references):
        self.root, self.references = Path(root), references
        if not isinstance(references, dict) or any(
                not isinstance(key, str) or not key.isascii() or not key.isdigit()
                or str(int(key)) != key for key in references):
            raise RuntimeError('invalid prepared worker identities')

    def __len__(self):
        return len(self.references)

    def __iter__(self):
        return iter(self.references)

    def __getitem__(self, key):
        return read_worker(self.root, self.references[key], key)


def workers(prepared, root):
    if 'format' not in prepared:
        return prepared['workers']  # Existing saved batches remain exact.
    if prepared['format'] != FORMAT:
        raise RuntimeError('unsupported prepared batch format')
    return StoredWorkers(root, prepared['workers'])
