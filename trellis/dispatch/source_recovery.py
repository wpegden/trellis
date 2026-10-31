"""Logical source reads for explicitly retired terminal-worker Git copies.

The index lists only files that actually existed when retired. It is not a Git
checkout: originally absent files stay absent, and surviving unique files win.
Readers are deployed before the separately opt-in cleanup scope is enabled.
"""
from contextlib import contextmanager
import gzip
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess

from . import retained_files as retained
from .candidate import canonical_json, safe_relative, CandidateError
from .publication import atomic

FORMAT = 'trellis-worker-git-source-recovery/1'
RETIREMENT_FORMAT = 'trellis-terminal-recoverable-source-retirement/1'
INDEX = 'source-recovery.json.gz'
RECEIPT = 'recoverable-sources-retired.json'
MAX_INDEX_BYTES = 16 << 20
MAX_FILE_BYTES = 64 << 20


def _json(path):
    path = retained.regular_path(path)
    before = retained._signature(path.stat())
    with path.open('rb') as stream:
        raw = stream.read(MAX_INDEX_BYTES + 1)
    if len(raw) > MAX_INDEX_BYTES or retained._signature(retained.regular_path(path).stat()) != before:
        raise ValueError('source recovery metadata exceeds its size limit')
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise ValueError('source recovery metadata must be an object')
    return value


def _index(path):
    path = retained.regular_path(path)
    before = retained._signature(path.stat())
    with path.open('rb') as stream:
        if os.fstat(stream.fileno()).st_size > MAX_INDEX_BYTES:
            raise ValueError('compressed source recovery index exceeds its size limit')
        with gzip.GzipFile(fileobj=stream) as archive:
            raw = archive.read(MAX_INDEX_BYTES + 1)
    if len(raw) > MAX_INDEX_BYTES or retained._signature(retained.regular_path(path).stat()) != before:
        raise ValueError('source recovery index exceeds its size limit')
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise ValueError('source recovery index must be an object')
    return value, hashlib.sha256(raw).hexdigest()


def _relative(name):
    try:
        safe_relative(name)
    except CandidateError as error:
        raise ValueError('unsafe source recovery path') from error
    path = Path(name)
    if (str(path) != name or len(path.parts) < 2
            or path.parts[0] not in ('Tablet', 'reference')):
        raise ValueError('source recovery path escapes its source surfaces')
    return path


def _git(repo, *args):
    try:
        return subprocess.check_output(['git', '--no-replace-objects', '-C', str(repo), *args],
            env={k: v for k, v in os.environ.items() if not k.startswith('GIT_')}, stderr=subprocess.PIPE)
    except subprocess.CalledProcessError as error:
        raise ValueError('canonical source recovery Git operation failed') from error


class Sources:
    """One bounded, receipt-bound logical view of a previous worker's sources."""
    def __init__(self, directory, *, repo=None):
        self.directory = Path(directory)
        self.files = {}
        receipt_path = self.directory / RECEIPT
        if not receipt_path.exists() and not receipt_path.is_symlink():
            # An orphan index can precede receipt publication, but no unlink can.
            return
        receipt = _json(receipt_path)
        record, digest = _index(self.directory / INDEX)
        prepared = _json(self.directory / 'prepared.json')
        if (receipt.get('format') != RETIREMENT_FORMAT
                or receipt.get('recovery_sha256') != digest
                or record.get('format') != FORMAT
                or not self.directory.name.isdigit()
                or record.get('owner') != dict(kind='worker', id=int(self.directory.name))
                or receipt.get('owner') != record['owner']
                or record.get('repo') != prepared.get('source_repo')
                or record.get('source_head') != prepared.get('source_head')
                or receipt.get('repo') != record['repo']
                or receipt.get('bindings', {}).get('.', {}).get('source_head') != record['source_head']):
            raise ValueError('source recovery index does not match its owner/preparation')
        if not isinstance(record['repo'], str):
            raise ValueError('source recovery repository must be an absolute path')
        self.repo = Path(record['repo'])
        if self.repo.resolve() != self.repo or (repo is not None and self.repo != Path(repo).resolve()):
            raise ValueError('source recovery belongs to another canonical repository')
        self.head = record['source_head']
        if (not isinstance(self.head, str) or len(self.head) not in (40, 64)
                or any(c not in '0123456789abcdef' for c in self.head)):
            raise ValueError('invalid source recovery commit')
        from .git_retention import _canonical_objects, PIN_PREFIX
        _canonical_objects(self.repo)
        pin = PIN_PREFIX + self.head
        if (_git(self.repo, 'rev-parse', '--verify', pin).decode().strip() != self.head
                or _git(self.repo, 'rev-parse', '--symbolic-full-name', pin).decode().strip() != pin):
            raise ValueError('source recovery requires its immutable canonical pin')
        files = record.get('files')
        if not isinstance(files, dict):
            raise ValueError('source recovery inventory must be a mapping')
        tree = {}
        for row in _git(self.repo, 'ls-tree', '-r', '-z', self.head, '--', 'Tablet', 'reference').split(b'\0'):
            if row:
                fields, name = row.split(b'\t', 1)
                tree[name.decode()] = fields.decode().split()
        for name, entry in files.items():
            _relative(name)
            if (not isinstance(entry, dict)
                    or type(entry.get('bytes')) is not int or not 0 <= entry['bytes'] <= MAX_FILE_BYTES
                    or type(entry.get('mode')) is not int or not 0 <= entry['mode'] <= 0o777
                    or type(entry.get('mtime_ns')) is not int or entry['mtime_ns'] < 0
                    or type(entry.get('atime_ns')) is not int or entry['atime_ns'] < 0
                    or tree.get(name) != ['100755' if entry['mode'] & 0o111 else '100644', 'blob', entry.get('oid')]):
                raise ValueError('source recovery entry is not an exact supported Git file')
        self.files = files

    @contextmanager
    def blobs(self):
        process = subprocess.Popen(['git', '--no-replace-objects', '-C', str(self.repo), 'cat-file', '--batch'],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            env={k: v for k, v in os.environ.items() if not k.startswith('GIT_')})
        try:
            yield process
        finally:
            process.stdin.close()
            process.stdout.close()
            if process.poll() is None:
                process.terminate()
            process.wait()

    def _blob(self, process, entry):
        process.stdin.write((entry['oid'] + '\n').encode()); process.stdin.flush()
        header = process.stdout.readline(256).decode().strip().split()
        if header != [entry['oid'], 'blob', str(entry['bytes'])]:
            raise ValueError('source recovery blob is missing or changed size')
        raw = process.stdout.read(entry['bytes'])
        if len(raw) != entry['bytes'] or process.stdout.read(1) != b'\n':
            raise ValueError('source recovery blob was truncated')
        algorithm = 'sha1' if len(entry['oid']) == 40 else 'sha256'
        if hashlib.new(algorithm, b'blob ' + str(len(raw)).encode() + b'\0' + raw).hexdigest() != entry['oid']:
            raise ValueError('source recovery blob failed its Git digest')
        return raw

    def read(self, relative, limit, *, truncate=False):
        name = str(_relative(relative))
        if name not in self.files:
            raise FileNotFoundError(self.directory / 'repo' / name)
        entry = self.files[name]
        if entry['bytes'] > limit and not truncate:
            raise ValueError('worker findings exceed the file size limit')
        with self.blobs() as process:
            raw = self._blob(process, entry)
        return raw[:limit], len(raw) > limit

    def materialize(self, destination):
        """Fill retired holes in a newly copied retry tree, without overwrites."""
        if not self.files:
            return
        destination = Path(destination)
        with self.blobs() as process:
            for name, entry in sorted(self.files.items()):
                target = destination / _relative(name)
                for parent in (target, *target.parents):
                    if parent.is_symlink():
                        raise ValueError('retry source destination contains an alias')
                    if parent == destination:
                        break
                if target.exists():
                    if not target.is_file():
                        raise ValueError('retry source path is not a regular file')
                    continue  # Remaining raw unique/recreated bytes are authoritative.
                raw = self._blob(process, entry)
                target.parent.mkdir(parents=True, exist_ok=True)
                with target.open('xb') as stream:
                    stream.write(raw)
                target.chmod(entry['mode'])
                os.utime(target, ns=(entry['atime_ns'], entry['mtime_ns']))


def prepare_index(collector, directory, record):
    """Publish recovery before deletion, returning an inexpensive final fence."""
    from .git_retention import _pin, _canonical_objects
    from .auxiliary_retention import _parents
    index = directory / INDEX
    binding = record['bindings'].get('.')
    if binding is None:
        raise ValueError('source recovery has no outer preparation binding')
    _canonical_objects(collector.repo)
    if index.exists() or index.is_symlink():
        manifest, digest = _index(index)
        if record.get('recovery_sha256', digest) != digest:
            raise ValueError('source recovery index changed')
    else:
        if record.get('recovery_sha256') is not None:
            raise ValueError('source recovery index disappeared')
        files = {}
        for entry in record['files']:
            path = retained.regular_path(directory / entry['path'])
            info = path.stat()
            if list(retained._signature(info)) != entry['signature']:
                raise ValueError('source changed before recovery publication')
            if (info.st_size > MAX_FILE_BYTES or info.st_mode & 0o7000
                    or os.listxattr(path, follow_symlinks=False)):
                raise ValueError('source metadata is unsupported for exact recovery')
            files[entry['git_path']] = dict(oid=entry['proof']['oid'], bytes=entry['bytes'],
                mode=stat.S_IMODE(info.st_mode), mtime_ns=info.st_mtime_ns, atime_ns=info.st_atime_ns)
        manifest = dict(format=FORMAT, owner=record['owner'], repo=str(collector.repo),
            source_head=binding['source_head'], files=files)
        raw = canonical_json(manifest)
        if len(raw) > MAX_INDEX_BYTES:
            raise ValueError('source recovery inventory exceeds its size limit')
        digest = hashlib.sha256(raw).hexdigest()
        if not retained._untracked(index):
            raise ValueError('source recovery index is Git tracked')
        atomic(index, gzip.compress(raw, mtime=0))
    expected = {e['git_path']: (e['proof']['oid'], e['bytes']) for e in record['files']}
    if (manifest.get('format') != FORMAT or manifest.get('owner') != record['owner']
            or manifest.get('repo') != str(collector.repo)
            or manifest.get('source_head') != binding['source_head']
            or {k: (v['oid'], v['bytes']) for k, v in manifest['files'].items()} != expected):
        raise ValueError('source recovery inventory differs from retirement inventory')
    for entry in record['files']:
        saved = manifest['files'][entry['git_path']]
        path = directory / entry['path']
        if (type(saved.get('mode')) is not int or not 0 <= saved['mode'] <= 0o777
                or type(saved.get('bytes')) is not int or not 0 <= saved['bytes'] <= MAX_FILE_BYTES
                or type(saved.get('mtime_ns')) is not int or saved['mtime_ns'] < 0
                or saved['mtime_ns'] != entry['signature'][3]
                or type(saved.get('atime_ns')) is not int or saved['atime_ns'] < 0):
            raise ValueError('source recovery copy metadata is invalid')
        if path.exists() or path.is_symlink():
            info = retained.regular_path(path).stat()
            if (stat.S_IMODE(info.st_mode) != saved['mode']
                    or list(retained._signature(info)) != entry['signature']
                    or os.listxattr(path, follow_symlinks=False)):
                raise ValueError('source recovery copy metadata changed')
        elif record.get('recovery_sha256') != digest:
            raise ValueError('missing source requires a receipt-bound recovery index')
    pin = _pin(collector.repo, binding['source_head'])
    record['recovery_sha256'] = digest
    signature = retained._signature(retained.regular_path(index).stat())
    parents = _parents(index, directory)
    def fence():
        _canonical_objects(collector.repo)
        if (retained._signature(retained.regular_path(index).stat()) != signature
                or _parents(index, directory) != parents or not retained._untracked(index)):
            raise ValueError('source recovery index changed before retirement')
        if (_git(collector.repo, 'rev-parse', '--verify', pin).decode().strip() != binding['source_head']
                or _git(collector.repo, 'rev-parse', '--symbolic-full-name', pin).decode().strip() != pin):
            raise ValueError('canonical source recovery pin changed before retirement')
    return fence
