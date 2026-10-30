"""Lossless terminal provider config/database retention and private export.

Export restores exact selected file bytes, not a complete resumable provider
home. Transcripts, credentials and custom skills remain at their original paths.
SQLite paths and other environment dependencies are not rewritten or validated.
"""
from contextlib import contextmanager
import argparse
import ctypes
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import tempfile

from . import retained_files as retained
from . import terminal_retention as terminal
from .assignment_retention import Deferred
from .candidate import canonical_json, _sync_dir

EXPORT_FORMAT = 'trellis-terminal-provider-export/1'
DATABASE = re.compile(r'(?:state_|thread_history_)[A-Za-z0-9_.-]+\.sqlite(?:-(?:wal|shm|journal))?\Z')


def _selected(name):
    return name == 'config.toml' or DATABASE.fullmatch(name) is not None


def paths(directory):
    """Exact owned burst homes only; normalize sidecars to their logical input."""
    from .storage import _attempt_directories
    for attempt in _attempt_directories(directory):
        parent = attempt / 'runtime/burst-homes'
        if any(p.is_symlink() for p in (parent, *parent.parents)):
            raise ValueError('provider home cannot traverse a symlink')
        if not parent.exists():
            continue
        for burst in sorted(parent.iterdir()):
            home = burst / '.codex'
            if burst.is_symlink() or home.is_symlink():
                raise ValueError('provider home cannot be an alias')
            if not home.is_dir():
                continue
            names = set()
            for child in home.iterdir():
                name = child.name
                for suffix in ('.retained-skip.json', '.retained.json', '.gz'):
                    if name.endswith(suffix):
                        name = name[:-len(suffix)]
                        break
                if _selected(name):
                    names.add(name)
            yield from (home / name for name in sorted(names))


def _guard(collector, directory, observed, *, allow_pin=False):
    root, directory = retained.owner_directory(collector.root, directory)
    owner = retained.owner_identity(directory)
    if directory.name not in observed or collector._owner_pending(owner):
        raise Deferred('not_terminal_or_pending')
    guard = (retained._execution_guard(directory, allow_pin=True) if allow_pin
             else retained.terminal_owner_guard(root, directory, observed))
    identity = observed[directory.name]
    if isinstance(identity, terminal._DeferredVerifierIdentity):
        identity = identity.resolve()
    witness = collector.witness()
    if witness.get('owners', witness['tasks']).get(directory.name) != identity:
        raise Deferred('not_committed')
    prepared = terminal._json(directory / 'prepared.json')
    if Path(prepared['source_repo']).resolve() != collector.repo:
        raise ValueError('provider preparation belongs to another project')
    if owner['kind'] == 'review' and prepared['request_id'] != owner['id']:
        raise ValueError('provider preparation belongs to another review')
    terminal._git(collector.repo, 'merge-base', '--is-ancestor', prepared['source_head'], witness['commit'])

    def recheck():
        if not collector.checkpoint_current(witness):
            raise ValueError('Git checkpoint changed during provider retention')
        if collector._owner_pending(owner):
            raise ValueError('owner publication still needs recovery')
        if terminal._json(directory / 'prepared.json') != prepared:
            raise ValueError('provider preparation changed')
        guard()
    return recheck, witness


def collect(collector, directory, observed, *, apply=True):
    result = dict(status='absent', archived=0, skipped=0, removed_bytes=0, removed_allocated_bytes=0)
    if collector.cancelled():
        return dict(result, status='cancelled')
    _, directory = retained.owner_directory(collector.root, directory)
    candidates = []
    for path in paths(directory):
        if not path.exists() and not path.is_symlink():
            continue
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            result['skipped'] += 1
            continue
        retained.regular_path(path)
        if not retained.MIN_BYTES <= info.st_size <= retained.MAX_BYTES:
            continue
        outputs = (path, *retained._paths(path), retained._skip_path(path))
        if any(not retained._untracked(item) for item in outputs) or retained._unchanged_no_savings(path):
            result['skipped'] += 1
            continue
        candidates.append((path, info, outputs))
    if not candidates:
        return result
    try:
        recheck, _ = _guard(collector, directory, observed)
    except Deferred as error:
        return dict(result, status=str(error))
    if not apply:
        return dict(result, status='eligible', files=len(candidates), bytes=sum(info.st_size for _, info, _ in candidates))
    for path, info, outputs in candidates:
        if collector.cancelled():
            return dict(result, status='cancelled')
        @contextmanager
        def final_unlink():
            with collector.retirement_lock() as acquired:
                if not acquired or collector.cancelled():
                    raise Deferred('cancelled' if collector.cancelled() else 'coordinator_busy')
                recheck()
                if any(not retained._untracked(item) for item in outputs):
                    raise ValueError('provider archive became Git tracked')
                if collector.cancelled():
                    raise Deferred('cancelled')
                yield
        try:
            archived = retained.archive(path, unlink_context=final_unlink)
        except Deferred as error:
            return dict(result, status=str(error))
        if archived:
            remaining = [item.stat() for item in retained._paths(path)]
            result['archived'] += 1
            result['removed_bytes'] += info.st_size - sum(item.st_size for item in remaining)
            result['removed_allocated_bytes'] += info.st_blocks * 512 - sum(item.st_blocks * 512 for item in remaining)
    return dict(result, status='archived' if result['archived'] else 'no_savings')


def _representation(path):
    if path.exists() or path.is_symlink():
        sources = [retained.regular_path(path)]
        mode = stat.S_IMODE(sources[0].stat().st_mode)
    else:
        _, record = retained._record(path)
        sources, mode = list(retained._paths(path)), record['mode']
    return mode, {str(p): retained._signature(retained.regular_path(p).stat()) for p in sources}


def _publish(temporary, destination):
    # Atomic directory publication without replacing even an empty destination.
    libc = ctypes.CDLL(None, use_errno=True)
    rename = getattr(libc, 'renameat2', None)
    if rename is None:
        raise OSError('atomic no-replace directory export is unavailable')
    if rename(-100, os.fsencode(temporary), -100, os.fsencode(destination), 1):
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), str(destination))


def export(collector, directory, observed, destination):
    """Export under the cleanup lease; never modify source files or controls."""
    with (collector.root / 'build-cleanup.lock').open('a+b') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        return _export_locked(collector, directory, observed, destination)


def _export_locked(collector, directory, observed, destination):
    """Pinned terminal owners may be read; all other owner guards remain."""
    _, directory = retained.owner_directory(collector.root, directory)
    destination = Path(destination).absolute()
    if any(p.is_symlink() for p in (destination, *destination.parents)):
        raise ValueError('provider export cannot traverse a symlink')
    destination = destination.resolve(strict=False)
    if destination.exists():
        raise ValueError('provider export destination already exists')
    for forbidden in (collector.root.parent, collector.repo):
        if destination.is_relative_to(forbidden.resolve()):
            raise ValueError('provider export must be outside the live runtime and project')
    recheck, witness = _guard(collector, directory, observed, allow_pin=True)
    inputs = {path: _representation(path) for path in paths(directory)}
    temporary = Path(tempfile.mkdtemp(prefix='.provider-export-', dir=destination.parent))
    try:
        records = []
        for path, (mode, signature) in inputs.items():
            relative = path.relative_to(directory)
            target = temporary / relative
            target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            digest, size = hashlib.sha256(), 0
            with target.open('xb') as output:
                for chunk in retained.chunks(path):
                    digest.update(chunk); size += len(chunk); output.write(chunk)
                output.flush(); os.fchmod(output.fileno(), mode); os.fsync(output.fileno())
            if _representation(path) != (mode, signature):
                raise ValueError('provider input changed during export')
            records.append(dict(path=str(relative), bytes=size, sha256=digest.hexdigest(), mode=mode))
        recheck()
        if set(paths(directory)) != set(inputs) or any(_representation(p) != value for p, value in inputs.items()):
            raise ValueError('provider family changed during export')
        record = dict(format=EXPORT_FORMAT, owner=retained.owner_identity(directory),
            checkpoint={key:witness[key] for key in ('commit','path','blob_sha256')}, files=records,
            scope='Exact retained provider config/database bytes only; not a complete resumable home. Credentials and transcripts remain at original paths.')
        with (temporary / 'export-manifest.json').open('xb') as output:
            output.write(canonical_json(record)); output.flush(); os.fsync(output.fileno())
        for parent in sorted((p for p in temporary.rglob('*') if p.is_dir()), key=lambda p:len(p.parts), reverse=True):
            _sync_dir(parent)
        _sync_dir(temporary)
        _publish(temporary, destination)
        _sync_dir(destination.parent)
        return dict(owner=directory.name, destination=str(destination), files=len(records), bytes=sum(r['bytes'] for r in records))
    finally:
        if temporary.exists():
            shutil.rmtree(temporary)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--runtime', type=Path, required=True)
    parser.add_argument('--repo', type=Path, required=True)
    parser.add_argument('--owner', required=True)
    parser.add_argument('--export', type=Path, required=True, help='new private destination outside runtime/project')
    args = parser.parse_args()
    root = args.runtime.absolute() / 'dispatch'
    collector = terminal.Collector(root, args.repo)
    observed = terminal.terminal_owners(json.loads(retained.regular_path(root.parent / 'protocol_state.json').read_bytes()))
    print(json.dumps(export(collector, root / args.owner, observed, args.export)))


if __name__ == '__main__':
    main()
