"""Lossless storage for terminal, private evidence; never a new trust source.

The original bytes, length and SHA256 are checked before removing a raw file
and on every archived read. Receipt validation still checks its original hashes.
Native/offline tools expecting ordinary paths can use ``--restore`` first.

Archival remains an explicit compatibility operation. Default background cleanup
uses terminal_retention to discard redundant committed private inputs. Pins stop
both operations; Git-indexed files are never compressed or removed here.

Operator usage::

    python -m trellis.dispatch.retained_files --runtime RUN-runtime --task 123
    python -m trellis.dispatch.retained_files --runtime RUN-runtime --task 123 --apply
    python -m trellis.dispatch.retained_files --runtime RUN-runtime --task 123 --restore

Terminal reviewer/verifier sessions can be restored with ``--owner review-123``
or ``--owner verifier-123`` instead of ``--task``. This restores only sessions.

Restore pins the task, preserves an existing operator pin, and restores every
selected representation without overwriting differing files. Once the offline
audit is complete, removing that pin allows normal terminal cleanup to resume. Build
caches previously discarded by ordinary GC are not reconstructed by restore.
"""
from __future__ import annotations

import argparse
from contextlib import nullcontext
import fcntl
import gzip
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import zlib

from .candidate import canonical_json, _sync_dir
from .publication import atomic

FORMAT = 'trellis-private-evidence-gzip/1'
MAX_BYTES = 2 << 30
CHUNK = 1 << 20
MIN_BYTES = 1 << 20


def regular_path(path):
    path = Path(path).absolute()
    for parent in reversed((path, *path.parents)):
        if parent.is_symlink():
            raise ValueError(f'retained evidence traverses a symlink: {path}')
    info = path.stat()
    if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
        raise ValueError(f'retained evidence must be an independent regular file: {path}')
    return path


def _paths(path):
    path = Path(path)
    return path.with_name(path.name + '.gz'), path.with_name(path.name + '.retained.json')


def _record(path):
    archive, metadata = _paths(path)
    with regular_path(metadata).open('rb') as stream:
        raw = stream.read(4097)
    if len(raw) > 4096:
        raise ValueError('oversized retained evidence record')
    record = json.loads(raw)
    if (not isinstance(record, dict) or record.get('format') != FORMAT or record.get('name') != Path(path).name
            or type(record.get('bytes')) is not int or not 0 <= record['bytes'] <= MAX_BYTES
            or not isinstance(record.get('sha256'), str) or len(record['sha256']) != 64
            or any(c not in '0123456789abcdef' for c in record['sha256'])
            or type(record.get('mode')) is not int or record['mode'] & ~0o777
            or ('mtime_ns' in record and (type(record['mtime_ns']) is not int or record['mtime_ns'] < 0))):
        raise ValueError('invalid retained evidence record')
    regular_path(archive)
    return archive, record


def _decode_chunks(path):
    archive, record = _record(path)
    digest, size = hashlib.sha256(), 0
    with gzip.open(archive, 'rb') as stream:
        while True:
            # Read at most one byte beyond the recorded size, even for a bomb.
            chunk = stream.read(min(CHUNK, record['bytes'] - size + 1))
            if not chunk:
                break
            size += len(chunk)
            if size > record['bytes']:
                raise ValueError('retained evidence exceeds its original length')
            digest.update(chunk)
            yield chunk
    if size != record['bytes'] or digest.hexdigest() != record['sha256']:
        raise ValueError('retained evidence bytes changed')


def _archived_chunks(path):
    try:
        yield from _decode_chunks(path)
    except (EOFError, zlib.error, gzip.BadGzipFile) as error:
        raise ValueError('retained evidence compressed stream is corrupt') from error


def chunks(path):
    """Read ordinary evidence or verify its lossless retained representation."""
    try:
        stream = regular_path(path).open('rb')
    except FileNotFoundError:
        yield from _archived_chunks(path)
        return
    with stream:
        yield from iter(lambda: stream.read(CHUNK), b'')


def read_bytes(path):
    return b''.join(chunks(path))


def present(path):
    """Presence hint only; any partial archive must reach the verifying reader."""
    return any(item.exists() or item.is_symlink() for item in (Path(path), *_paths(path)))


def logical_size(path):
    try:
        return regular_path(path).stat().st_size
    except FileNotFoundError:
        return _record(path)[1]['bytes']


def _signature(info):
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)


def _skip_path(path):
    return Path(path).with_name(Path(path).name + '.retained-skip.json')


def _unchanged_no_savings(path):
    """A performance hint only: the complete original evidence stays in place."""
    marker = _skip_path(path)
    if not marker.exists() and not marker.is_symlink():
        return False
    with regular_path(marker).open('rb') as stream:
        raw = stream.read(4097)
    if len(raw) > 4096:
        raise ValueError('oversized retained evidence skip record')
    record = json.loads(raw)
    if not isinstance(record, dict) or record.get('format') != FORMAT or record.get('reason') != 'no_savings':
        raise ValueError('invalid retained evidence skip record')
    return record.get('signature') == list(_signature(regular_path(path).stat()))


def _same_bytes(left, right):
    with regular_path(left).open('rb') as first, regular_path(right).open('rb') as second:
        while True:
            a, b = first.read(CHUNK), second.read(CHUNK)
            if a != b:
                return False
            if not a:
                return True


def _digest_file(path):
    digest = hashlib.sha256()
    with regular_path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(CHUNK), b''):
            digest.update(chunk)
    return digest.hexdigest()


def _untracked(path):
    """Ask the actual enclosing Git repository, including nested repositories."""
    result = subprocess.run(['git', '-C', str(Path(path).parent), 'rev-parse', '--show-toplevel'],
                            capture_output=True, text=True)
    if result.returncode:
        # A non-repository is the usual external runtime case. Other Git errors
        # (permissions, corrupt index, dubious ownership) must not allow pruning.
        if 'not a git repository' in result.stderr:
            return True
        raise ValueError('cannot determine Git ownership of private evidence')
    repo = Path(result.stdout.strip()).resolve()
    relative = Path(path).absolute().relative_to(repo)
    result = subprocess.run(['git', '-C', str(repo), 'ls-files', '-z', '--', str(relative)],
                            capture_output=True)
    if result.returncode:
        raise ValueError('cannot inspect Git index before evidence archival')
    return not result.stdout


def _untracked_cache(path, runtime_root):
    """Fresh negative Git discovery for the two fixed external cache scopes.

    No negative result is cached: each pre-unlink call checks all ancestors.
    Nested repositories, worktrees, bare layouts, aliases and Git overrides
    retain the original Git query/error behavior. The callers' path, process
    and coordinator fences still apply; this only avoids spawning Git when
    there is no possible enclosing repository to discover.
    """
    path, root = Path(path), Path(runtime_root)
    try:
        parts = path.relative_to(root).parts
    except ValueError:
        return _untracked(path)
    def sha(value):
        return len(value) == 64 and all(c in '0123456789abcdef' for c in value)
    scoped = ((len(parts) >= 3 and parts[0] == 'publication' and sha(parts[1]))
              or (len(parts) >= 4 and parts[0] == 'batch-caches'
                  and parts[1].isascii() and parts[1].isdigit() and sha(parts[2])))
    if (not path.is_absolute() or not root.is_absolute() or '..' in path.parts
            or not scoped or any(key.startswith('GIT_') for key in os.environ)):
        return _untracked(path)
    try:
        try:
            if stat.S_ISLNK(path.lstat().st_mode):
                return _untracked(path)
        except FileNotFoundError:
            pass  # A not-yet-created retirement marker is also queried.
        for parent in path.parents:
            # Directory-relative probes avoid resolving the same long runtime
            # prefix four times. O_NOFOLLOW retains the ancestor alias check.
            fd = os.open(parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
            try:
                for marker in ('.git', 'HEAD', 'objects', 'refs'):
                    try:
                        os.stat(marker, dir_fd=fd, follow_symlinks=False)
                    except FileNotFoundError:
                        continue
                    return _untracked(path)
            finally:
                os.close(fd)
    except OSError:
        return _untracked(path)
    return True


def archive(path, *, recheck=lambda: None, unlink_context=nullcontext, preserve_mtime=False):
    """Called only after a native terminal witness and all process cleanups.

    ``recheck`` repeats the caller's process/pin guard immediately before unlink.
    An interrupted archive is verified and completed, never trusted by presence.
    Optional ``unlink_context`` fences only final checks/unlink, keeping encoding,
    validation and directory fsync outside the caller's publication lock.
    """
    path = regular_path(path)
    info = path.stat()
    if not MIN_BYTES <= info.st_size <= MAX_BYTES or not _untracked(path):
        return False
    archive_path, metadata = _paths(path)
    digest = hashlib.sha256()
    fd, name = tempfile.mkstemp(prefix='.retained-', dir=path.parent)
    temporary = Path(name)
    try:
        with os.fdopen(fd, 'wb') as output, path.open('rb') as source:
            read = 0
            with gzip.GzipFile(filename='', mode='wb', fileobj=output, compresslevel=1, mtime=0) as encoded:
                for chunk in iter(lambda: source.read(CHUNK), b''):
                    read += len(chunk)
                    if read > info.st_size:
                        raise ValueError('private evidence grew during archival')
                    digest.update(chunk)
                    encoded.write(chunk)
            output.flush()
            os.fsync(output.fileno())
        record = dict(format=FORMAT, name=path.name, bytes=info.st_size,
                      sha256=digest.hexdigest(), mode=stat.S_IMODE(info.st_mode))
        if preserve_mtime:
            if info.st_mtime_ns < 0:
                raise ValueError('retained original mtime must be nonnegative')
            record['mtime_ns'] = info.st_mtime_ns
        if _signature(regular_path(path).stat()) != _signature(info):
            raise ValueError('private evidence changed during archival')
        if temporary.stat().st_size + len(canonical_json(record)) >= info.st_size:
            # Do not repeatedly spend a bounded pass on the same incompressible
            # prefix, starving later evidence. Stat identity invalidates this
            # advisory skip when any raw inode/bytes/metadata change.
            atomic(_skip_path(path), canonical_json(dict(format=FORMAT,
                reason='no_savings', signature=list(_signature(info)))))
            return False
        if archive_path.exists() or archive_path.is_symlink():
            # Recover archive-before-manifest interruption by comparing the
            # complete independent compressed bytes we just produced.
            if not _same_bytes(archive_path, temporary):
                raise ValueError('existing retained archive differs')
        else:
            os.replace(temporary, archive_path)
            _sync_dir(path.parent)
        if metadata.exists() or metadata.is_symlink():
            if _record(path)[1] != record:
                raise ValueError('existing retained evidence record differs')
        else:
            atomic(metadata, canonical_json(record))
        # Exercise the same decoder as future audits before removing any bytes.
        for _ in _archived_chunks(path):
            pass
        with unlink_context():
            recheck()
            if (_signature(regular_path(path).stat()) != _signature(info)
                    or not _untracked(path)):
                raise ValueError('private evidence changed before archival unlink')
            path.unlink()
        _sync_dir(path.parent)
        return True
    finally:
        temporary.unlink(missing_ok=True)


def restore(path):
    """Restore an independent ordinary file, preserving exact bytes and mode."""
    path = Path(path)
    _, record = _record(path)
    if path.exists() or path.is_symlink():
        if regular_path(path).stat().st_size != record['bytes'] or _digest_file(path) != record['sha256']:
            raise ValueError('refusing to overwrite different restored evidence')
        # A raw file does not make a corrupt retained representation acceptable.
        for _ in _archived_chunks(path):
            pass
        return False
    fd, name = tempfile.mkstemp(prefix='.restore-', dir=path.parent)
    temporary = Path(name)
    try:
        with os.fdopen(fd, 'wb') as output:
            for chunk in _archived_chunks(path):
                output.write(chunk)
            output.flush()
            os.fchmod(output.fileno(), record['mode'])
            if 'mtime_ns' in record:
                os.utime(output.fileno(), ns=(record['mtime_ns'], record['mtime_ns']))
            os.fsync(output.fileno())
        # Do not overwrite a file recreated while the decompressor was working.
        os.link(temporary, path)
        temporary.unlink()
        _sync_dir(path.parent)
        return True
    finally:
        temporary.unlink(missing_ok=True)


def eligible_paths(directory):
    from .storage import _attempt_directories
    for attempt in _attempt_directories(directory):
        # Staged context is read only through authenticated regular() after
        # completion; staged-result is a diagnostic copy of execution/response.
        # Native checking receives the context inline while the task is active.
        for name in ('protocol_state.json', 'staged-context.json', 'staged-result.json'):
            path = attempt / 'runtime' / name
            if any(part.exists() or part.is_symlink() for part in (path, *_paths(path), _skip_path(path))):
                yield path
        for repo in (attempt / 'repo', attempt / 'repo/.trellis/supervisor/repo'):
            parent = repo / '.trellis/kernel-replay-environments'
            if parent.is_symlink():
                raise ValueError('private replay environments cannot traverse a symlink')
            names = {path.name.removesuffix('.retained-skip.json').removesuffix('.retained.json').removesuffix('.gz')
                     for pattern in ('*.json', '*.json.gz') for path in parent.glob(pattern)}
            for name in sorted(names):
                stem = name.removesuffix('.json')
                if len(stem) == 64 and all(c in '0123456789abcdef' for c in stem):
                    yield parent / name


def _task_directory(root, directory):
    root, directory = Path(root).absolute(), Path(directory).absolute()
    if directory.parent != root or not directory.name.isdigit():
        raise ValueError('evidence archival requires a numbered task directory')
    if any(path.is_symlink() for path in (directory, *directory.parents)):
        raise ValueError('private task cannot traverse a symlink')
    return root, directory


def owner_identity(directory):
    """Namespaced private owners; exclusive lanes need a separate lifecycle proof."""
    name = Path(directory).name
    if name.isascii() and name.isdigit():
        return dict(kind='worker', id=int(name))
    for kind in ('review', 'verifier'):
        prefix = kind + '-'
        suffix = name.removeprefix(prefix)
        if name.startswith(prefix) and suffix.isascii() and suffix.isdigit() and str(int(suffix)) == suffix:
            return dict(kind=kind, id=int(suffix))
    raise ValueError('unsupported private retirement owner')


def owner_directory(root, directory):
    root, directory = Path(root).absolute(), Path(directory).absolute()
    owner_identity(directory)
    if directory.parent != root or any(path.is_symlink() for path in (directory, *directory.parents)):
        raise ValueError('private owner must be a direct unaliased dispatch directory')
    return root, directory


def terminal_owner_guard(root, directory, observed):
    root, directory = owner_directory(root, directory)
    if directory.name not in observed:
        raise ValueError('private evidence has no native terminal owner witness')
    owner = owner_identity(directory)
    pins = [root / 'reviews' / str(owner['id']) / 'retain-state.json'] if owner['kind'] == 'review' else []
    return _execution_guard(directory, pins=pins)


def terminal_guard(root, directory, *, allow_pin=False, terminal_tasks=None):
    """Verify the canonical terminal witness and every owned execution guard."""
    root, directory = _task_directory(root, directory)
    # The background caller already observed these IDs in native routing's
    # authoritative result. Terminal IDs never reopen; retain only their names,
    # avoiding another huge state parse/GIL pause during every dispatch snapshot.
    # Standalone operator use obtains its own witness from canonical state.
    if terminal_tasks is None:
        state = json.loads(regular_path(root.parent / 'protocol_state.json').read_bytes())
        terminal_tasks = {id for id, task in state['dispatch']['tasks'].items()
                          if task['status']['status'] == 'resolved'}
        del state
    if directory.name not in terminal_tasks:
        raise ValueError('private evidence has no native terminal task witness')
    return _execution_guard(directory, allow_pin=allow_pin)


def _execution_guard(directory, *, allow_pin=False, pins=()):
    from .storage import _attempt_directories, _pinned
    from .process import alive
    attempts = _attempt_directories(directory)

    def guard():
        if not allow_pin and (_pinned(attempts) or any(p.exists() or p.is_symlink() for p in pins)):
            raise ValueError('private evidence is retained by an operator pin')
        for attempt in attempts:
            execution = attempt / 'execution'
            if execution.is_symlink():
                raise ValueError('private execution cannot traverse a symlink')
            process = execution / 'process.json'
            if process.exists() or process.is_symlink():
                identity = json.loads(regular_path(process).read_bytes())
                cleanup = execution / 'cleanup.json'
                if alive(identity) or json.loads(regular_path(cleanup).read_bytes()) != identity:
                    raise ValueError('private execution lacks acknowledged process cleanup')
    guard()
    return guard


def archive_terminal(root, directory, *, max_files=2, terminal_tasks=None, budget=None):
    """Bound compression per housekeeping pass; all heavy work is off-lock."""
    root, directory = _task_directory(root, directory)
    budget = {'files': max_files} if budget is None else budget
    marker = Path(directory) / 'private-evidence-archived.json'
    if marker.exists() or marker.is_symlink():
        if json.loads(regular_path(marker).read_bytes()).get('format') == FORMAT:
            return 0
        raise ValueError('unknown private evidence archival marker')
    paths = list(eligible_paths(directory))
    # Avoid repeatedly loading a large canonical ledger for small empty attempts.
    if not any(path.exists() and path.stat().st_size >= MIN_BYTES for path in paths):
        return 0
    guard = terminal_guard(root, directory, terminal_tasks=terminal_tasks)
    removed = 0
    for path in paths:
        if not path.exists() and not path.is_symlink():
            continue
        info = regular_path(path).stat()
        if not MIN_BYTES <= info.st_size <= MAX_BYTES or not _untracked(path):
            continue
        if _unchanged_no_savings(path):
            continue
        if budget['files'] <= 0:
            return removed
        # Bound work attempted, including incompressible/corrupt files, rather
        # than only successful unlinks. The background pass shares this budget
        # across tasks, so unchanged incompressible evidence cannot monopolize GC.
        budget['files'] -= 1
        removed += bool(archive(path, recheck=guard))
    atomic(marker, canonical_json(dict(format=FORMAT)))
    return removed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--runtime', type=Path, required=True)
    target = parser.add_mutually_exclusive_group(required=True)
    target.add_argument('--task', type=int)
    target.add_argument('--owner', help='review-ID or verifier-ID; session restore only')
    parser.add_argument('--restore', action='store_true', help='restore exact files and pin against immediate archival')
    parser.add_argument('--apply', action='store_true', help='archive a resolved task; default lists eligible private paths')
    args = parser.parse_args()
    if (args.task is not None and args.task < 0) or args.apply and args.restore:
        parser.error('choose a nonnegative task and at most one of --apply/--restore')
    if args.owner is not None and not args.restore:
        parser.error('--owner requires --restore')
    root = args.runtime / 'dispatch'
    directory = root / (args.owner if args.owner is not None else str(args.task))
    if args.owner is not None:
        _, directory = owner_directory(root, directory)
        if owner_identity(directory)['kind'] not in ('review', 'verifier'):
            parser.error('--owner requires review-ID or verifier-ID')
    with (root / 'build-cleanup.lock').open('a+b') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if args.owner is None:
            terminal_guard(root, directory, allow_pin=args.restore)
        else:
            from .terminal_retention import terminal_owners
            state = json.loads(regular_path(root.parent / 'protocol_state.json').read_bytes())
            if directory.name not in terminal_owners(state):
                raise ValueError('session restore requires a native terminal owner')
            del state
            _execution_guard(directory, allow_pin=True)
        if args.restore:
            # Full offline replay/native readers need ordinary paths. A pin
            # prevents the background collector from immediately compacting them.
            pin = directory / 'retain-builds.json'
            if pin.exists() or pin.is_symlink():
                regular_path(pin)
            else:
                atomic(pin, canonical_json(dict(reason='restored private evidence for offline audit')))
            from .host_response_retention import paths as host_response_paths
            from .provider_session_retention import paths as session_paths
            from itertools import chain
            restored = []
            paths = (session_paths(directory) if args.owner is not None else
                     chain(eligible_paths(directory), host_response_paths(directory), session_paths(directory)))
            for path in paths:
                if not any(part.exists() or part.is_symlink() for part in _paths(path)):
                    continue
                if restore(path):
                    restored.append(str(path.relative_to(directory)))
            marker = directory / 'private-evidence-archived.json'
            if args.owner is None and (marker.exists() or marker.is_symlink()):
                regular_path(marker).unlink()
                _sync_dir(directory)
            print(json.dumps(dict(task=args.task, restored=restored) if args.owner is None else
                             dict(owner=args.owner, restored=restored)))
        elif args.apply:
            print(json.dumps(dict(task=args.task, archived=archive_terminal(root, directory, max_files=100000))))
        else:
            files = []
            for path in eligible_paths(directory):
                if not path.exists():
                    continue
                info = regular_path(path).stat()
                if MIN_BYTES <= info.st_size <= MAX_BYTES and _untracked(path):
                    files.append(dict(path=str(path.relative_to(directory)), bytes=info.st_size,
                                      allocated_bytes=info.st_blocks * 512))
            print(json.dumps(dict(task=args.task, files=files,
                bytes=sum(item['bytes'] for item in files),
                allocated_bytes=sum(item['allocated_bytes'] for item in files))))


if __name__ == '__main__':
    main()
