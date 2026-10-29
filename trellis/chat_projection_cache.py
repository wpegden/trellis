"""Discardable, content-integrity-checked acceleration for derived chat trees.

The cache never supplies filesystem paths. Current event filenames and current
live artifact names drive all operations. Any invalid cache falls back to the
ordinary parser/copy, and changed source/destination fingerprints invalidate it.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import tempfile

FORMAT = 'trellis-chat-projection-cache/1'


def _json(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


def load(path):
    try:
        if path.is_symlink():
            return {}
        record = json.loads(path.read_bytes())
        data = record['data']
        if (record['format'] != FORMAT or not isinstance(data, dict)
                or not isinstance(data.get('events'), dict)
                or not isinstance(data.get('artifacts'), dict)
                or record['sha256'] != hashlib.sha256(_json(data)).hexdigest()):
            return {}
        return data
    except (OSError, ValueError, KeyError, TypeError):
        return {}


def save(path, data):
    """Best-effort atomic cache publication; never affect checkpoint success."""
    temporary = None
    try:
        record = dict(format=FORMAT, data=data, sha256=hashlib.sha256(_json(data)).hexdigest())
        fd, name = tempfile.mkstemp(prefix='.chat-projection-cache-', dir=path.parent)
        temporary = Path(name)
        with os.fdopen(fd, 'wb') as stream:
            stream.write(_json(record))
        os.replace(temporary, path)
    except OSError:
        pass
    finally:
        if temporary is not None:
            try:
                temporary.unlink(missing_ok=True)
            except OSError:
                pass


def signature(path):
    """No aliases: inode plus nanosecond timestamps cover replacement/rewind."""
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode):
        return None
    return [info.st_dev, info.st_ino, info.st_mode, info.st_nlink,
            info.st_size, info.st_mtime_ns, info.st_ctime_ns]


def request_cycles(files, cached, parse):
    mapping, fresh = {}, {}
    for path in files:
        try:
            before = signature(path)
        except OSError:
            before = None
        previous = cached.get(path.name)
        requests = None
        if (before is not None and isinstance(previous, dict)
                and previous.get('signature') == before
                and isinstance(previous.get('requests'), dict)):
            candidate = previous['requests']
            try:
                if all(str(int(k)) == k and type(v) is int for k, v in candidate.items()):
                    requests = {int(k): v for k, v in candidate.items()}
            except (ValueError, TypeError):
                pass
        readable = True
        if requests is None:
            requests, readable = parse(path)
        mapping.update(requests)  # Same sorted-file/last-command-wins order.
        try:
            after = signature(path)
        except OSError:
            after = None
        if readable and before is not None and before == after:
            fresh[path.name] = dict(signature=after,
                                    requests={str(k): v for k, v in requests.items()})
    return mapping, fresh


def tree_signature(root):
    """Full membership and strong signatures for source AND destination trees.

    Alias/special-file trees use the legacy full-copy path and are never cached.
    """
    result = {}
    try:
        def visit(path):
            info = path.lstat()
            if not (stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode)):
                raise ValueError('uncacheable tree entry')
            result[str(path.relative_to(root))] = [info.st_dev, info.st_ino,
                info.st_mode, info.st_nlink, info.st_size, info.st_mtime_ns, info.st_ctime_ns]
            if stat.S_ISDIR(info.st_mode):
                for child in sorted(path.iterdir()):
                    visit(child)
        visit(root)
    except (OSError, ValueError):
        return None
    return result


def copy_artifact(source, destination, previous):
    before = tree_signature(source)
    current = tree_signature(destination)
    if (before is not None and current is not None and isinstance(previous, dict)
            and previous.get('source') == before and previous.get('destination') == current):
        return previous
    # Match the old rebuild's contents, including removal of extra target files.
    # A symlink cycle/artifact destination is refused rather than followed.
    if destination.is_symlink():
        raise ValueError('chat projection destination must not be a symlink')
    if destination.is_dir():
        shutil.rmtree(destination)
    elif destination.exists():
        destination.unlink()
    shutil.copytree(source, destination)
    after = tree_signature(source)
    copied = tree_signature(destination)
    if before is None or before != after or copied is None:
        return None  # Concurrent source changes must be recopied next time.
    return dict(source=after, destination=copied)
