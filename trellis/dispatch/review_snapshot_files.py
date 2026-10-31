"""Lossless files for immutable, private reviewer inputs.

Canonical and Git state remain plain JSON. New large reviewer captures are
compressed directly into their durable destination; existing plain captures
remain readable. Callers serialize capture with the coordinator lock.
"""
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import tempfile

from . import retained_files as retained
from .candidate import _sync_dir

COMPRESS_AT = 65536


def location(path):
    path = Path(path)
    if path.name != 'state.json':
        raise ValueError('review snapshot requires a state.json logical path')
    found = [p for p in (path, path.with_name('state.json.gz'))
             if p.exists() or p.is_symlink()]
    if len(found) > 1:
        raise ValueError('ambiguous plain and compressed review snapshots')
    return retained.regular_path(found[0]) if found else None


def read_bytes(path):
    actual = location(path)
    if actual is None:
        raise FileNotFoundError(path)
    before = retained._signature(actual.stat())
    with (gzip.open(actual, 'rb') if actual.suffix == '.gz' else actual.open('rb')) as stream:
        raw = stream.read()
    if location(path) != actual or retained._signature(actual.stat()) != before:
        raise ValueError('review snapshot changed during read')
    return raw


def read(path):
    return json.loads(read_bytes(path))


def capture(path, source):
    """Publish one exact input from canonical bytes or a locked canonical file.

    Streaming gzip uses bounded temporary memory. A present logical snapshot
    must already have identical bytes; this never replaces historical inputs.
    """
    path = Path(path)
    if not path.is_absolute() or path.name != 'state.json':
        raise ValueError('review snapshot destination must be an absolute state.json path')
    if any(p.is_symlink() for p in (path.parent, *path.parent.parents)):
        raise ValueError('review snapshot parent traverses a symlink')
    path.parent.mkdir(parents=True, exist_ok=True)
    if any(p.is_symlink() for p in (path.parent, *path.parent.parents)):
        raise ValueError('review snapshot parent traverses a symlink')
    parent = path.parent.stat()
    parent_identity = parent.st_dev, parent.st_ino
    if not all(retained._untracked(p) for p in (path, path.with_name('state.json.gz'))):
        raise ValueError('review snapshots must not be Git tracked')
    source_path = None if isinstance(source, bytes) else retained.regular_path(Path(source))
    source_signature = retained._signature(source_path.stat()) if source_path else None
    size = len(source) if source_path is None else source_path.stat().st_size
    def source_stream():
        return io.BytesIO(source) if source_path is None else source_path.open('rb')
    def source_current():
        if source_path is not None and retained._signature(retained.regular_path(source_path).stat()) != source_signature:
            raise ValueError('canonical input changed during reviewer capture')
    existing = location(path)
    if existing is not None:
        with source_stream() as stream:
            expected = hashlib.file_digest(stream, 'sha256').digest()
        if hashlib.sha256(read_bytes(path)).digest() != expected:
            raise RuntimeError('review ID already has a different source snapshot')
        source_current()
        return existing
    target = path.with_name('state.json.gz') if size >= COMPRESS_AT else path
    fd, temporary_name = tempfile.mkstemp(prefix='.review-state-', suffix='.tmp', dir=path.parent)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(fd, 'wb') as output:
            if target.suffix == '.gz':
                sink = gzip.GzipFile(filename='', mode='wb', fileobj=output, compresslevel=1, mtime=0)
            else:
                sink = output
            try:
                with source_stream() as stream:
                    while chunk := stream.read(1 << 20):
                        sink.write(chunk)
            finally:
                if sink is not output:
                    sink.close()
            output.flush()
            os.fsync(output.fileno())
        source_current()
        fresh_parent = path.parent.stat()
        if ((fresh_parent.st_dev, fresh_parent.st_ino) != parent_identity
                or any(p.is_symlink() for p in (path.parent, *path.parent.parents))):
            raise ValueError('review snapshot parent changed')
        if location(path) is not None:
            raise ValueError('review snapshot appeared during capture')
        if not all(retained._untracked(p) for p in (path, target)):
            raise ValueError('review snapshot became Git tracked')
        temporary.replace(target)
        _sync_dir(path.parent)
        return target
    finally:
        temporary.unlink(missing_ok=True)
