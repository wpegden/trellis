"""Discard terminal worker audit copies only while exact originals survive.

Reviewer/advisor workspaces can be future handoff sources and are excluded.
Canonical audit evidence is never changed, whether or not Git tracks it.
"""
import hashlib
import errno
import re
import stat
from pathlib import Path

from . import auxiliary_retention as auxiliary, retained_files as retained
from . import terminal_retention as terminal
from .candidate import canonical_json, _sync_dir
from .publication import atomic
from .source_retention import inventory

FORMAT = 'trellis-terminal-audit-copy-retirement/1'
RECEIPT = 'audit-copies-retired.json'
SURFACE = '.trellis/stuck-math-audit'


def retire_empty_directories(collector, directory, observed, *, apply=False):
    """Remove only empty generated descendants, retaining the audit root.

    This is independent of completed file receipts: old retirement deliberately
    left directories behind. No receipt grants authority to remove new files.
    """
    result = dict(status='absent', removed_directories=0, removed_bytes=0,
                  removed_allocated_bytes=0)
    if collector.cancelled():
        return dict(result, status='cancelled')
    root, directory = retained.owner_directory(collector.root, Path(directory))
    owner = retained.owner_identity(directory)
    if owner['kind'] != 'worker':
        return dict(result, status='nonworker_preserved')
    if directory.name not in observed or collector._owner_pending(owner):
        return dict(result, status='not_terminal_or_pending')
    guard = witness = None
    from .storage import _attempt_directories
    pins = [attempt / 'retain-builds.json' for attempt in _attempt_directories(directory)]
    for attempt in roots(directory):
        surface = attempt / 'repo' / SURFACE
        if not surface.exists() and not surface.is_symlink():
            continue
        # Bind root and ancestors too; the synthetic leaf is never opened.
        parents = auxiliary._parents(surface / '.directory-fence', directory)
        content = inventory(surface)  # Reject aliases, nested Git, special files.
        selected = [(surface / name, item['directory']) for name, item in content.items()
                    if 'directory' in item and re.fullmatch(
                        r'cycle-[0-9]+-request-[0-9]+', Path(name).parts[0])]
        if not selected:
            continue
        if guard is None:
            guard = retained.terminal_owner_guard(root, directory, observed)
            witness = collector.witness()
            if witness.get('owners', witness['tasks']).get(directory.name) != observed[directory.name]:
                return dict(result, status='not_committed')
        binding = auxiliary._binding(attempt, collector, witness, owner)
        selected.sort(key=lambda item: len(item[0].parts), reverse=True)
        expected_parents = {path: auxiliary._parents(path, directory) for path, _ in selected}
        directory_fences = {surface: parents, **{
            path: expected_parents[path] | {str(path.relative_to(directory)): identity}
            for path, identity in selected}}
        if not apply:
            result['status'] = 'eligible'
            result['candidate_directories'] = result.get('candidate_directories', 0) + len(selected)
            continue
        dirty = set()
        try:
            with collector.retirement_lock() as acquired:
                if not acquired or collector.cancelled():
                    return dict(result, status='cancelled' if collector.cancelled() else 'coordinator_busy')
                if not collector.checkpoint_current(witness):
                    return dict(result, status='checkpoint_changed')
                if collector._owner_pending(owner):
                    return dict(result, status='publication_pending')
                guard()
                if (list(retained._signature(retained.regular_path(attempt / 'prepared.json').stat()))
                        != binding['prepared_signature']):
                    raise ValueError('audit preparation changed before directory retirement')
                if auxiliary._parents(surface / '.directory-fence', directory) != parents:
                    raise ValueError('audit root changed before directory retirement')
                if not retained._untracked(surface):
                    continue
                for path, identity in selected:
                    if collector.cancelled():
                        return dict(result, status='cancelled')
                    if any(pin.exists() or pin.is_symlink() for pin in pins):
                        raise ValueError('private evidence is retained by an operator pin')
                    try:
                        info = path.lstat()
                    except FileNotFoundError:
                        continue
                    if (not stat.S_ISDIR(info.st_mode)
                            or [info.st_dev, info.st_ino, info.st_mode] != identity
                            or auxiliary._parents(path, directory) != expected_parents[path]):
                        raise ValueError('audit directory identity changed before retirement')
                    try:
                        path.rmdir()  # Atomic emptiness test; never unlinks evidence.
                    except OSError as error:
                        if error.errno in (errno.ENOTEMPTY, errno.EEXIST):
                            continue
                        raise
                    dirty.add(path.parent)
                    result['removed_directories'] += 1
                    result['removed_allocated_bytes'] += info.st_blocks * 512
                    result['status'] = 'retired'
        finally:
            # Parents removed later need no separate sync; their surviving
            # ancestor records the removal. There is no permanent done marker.
            for parent in sorted(dirty):
                if parent.exists() or parent.is_symlink():
                    if auxiliary._parents(parent / '.directory-fence', directory) != directory_fences[parent]:
                        raise ValueError('audit parent changed before directory sync')
                    _sync_dir(parent)
    return result


def roots(directory):
    from .storage import _numbered_directories
    yield directory
    for attempt in _numbered_directories(directory / 'acceptance'):
        yield attempt
        yield from _numbered_directories(attempt / 'finalization')


def _hash(path):
    path = retained.regular_path(path)
    before = retained._signature(path.stat())
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b''):
            value.update(chunk)
    if retained._signature(retained.regular_path(path).stat()) != before:
        raise ValueError('audit evidence changed during comparison')
    return value.hexdigest(), list(before)


def _original(collector, relative, digest):
    path = collector.repo / relative
    try:
        parents = auxiliary._parents(path, collector.repo.parent)
        actual, signature = _hash(path)
    except FileNotFoundError:
        return None
    if actual != digest:
        return None
    return dict(path=path, signature=signature, parents=parents)


def retire(collector, directory, observed, *, apply=False):
    result = dict(status='absent', removed=0, removed_bytes=0,
                  removed_allocated_bytes=0, retained_files=0)
    if collector.cancelled():
        return dict(result, status='cancelled')
    root, directory = retained.owner_directory(collector.root, directory)
    owner = retained.owner_identity(directory)
    if owner['kind'] != 'worker':
        return dict(result, status='nonworker_preserved')
    if directory.name not in observed or collector._owner_pending(owner):
        return dict(result, status='not_terminal_or_pending')
    allowed = {str((attempt / 'repo' / SURFACE).relative_to(directory)): attempt
               for attempt in roots(directory)}
    receipt = directory / RECEIPT
    if not receipt.exists() and not any((directory / name).exists() or (directory / name).is_symlink()
                                        for name in allowed):
        return result
    guard = retained.terminal_owner_guard(root, directory, observed)
    identity = observed[directory.name]
    witness = collector.witness()
    if witness.get('owners', witness['tasks']).get(directory.name) != identity:
        return dict(result, status='not_committed')
    if not retained._untracked(receipt):
        raise ValueError('audit retirement receipt is Git tracked')
    if receipt.exists() or receipt.is_symlink():
        record = terminal._json(receipt)
        if (record.get('format') != FORMAT or record.get('owner') != owner
                or record.get('identity') != identity or record.get('repo') != str(collector.repo)):
            raise ValueError('audit retirement receipt changed identity')
        terminal._git(collector.repo, 'merge-base', '--is-ancestor', record['checkpoint']['commit'], witness['commit'])
        if record.get('completed') is True:
            # Skip-only: recreated/previously unique files gain no authority.
            if record.get('files') != []:
                raise ValueError('invalid completed audit retirement receipt')
            guard()
            return dict(result, status='unchanged', retained_files=record['retained_files'])
    else:
        entries, bindings = [], {}
        for name, attempt in allowed.items():
            path = directory / name
            if not path.exists() and not path.is_symlink():
                continue
            content = inventory(path)  # Reject aliases, nested Git and hardlinks.
            if not retained._untracked(path):
                result['retained_files'] += sum('signature' in item for item in content.values())
                continue
            key = str(attempt.relative_to(directory))
            bindings[key] = auxiliary._binding(attempt, collector, witness, owner)
            for relative, item in content.items():
                if 'signature' not in item:
                    continue
                selected = path / relative
                digest, signature = _hash(selected)
                if signature != item['signature']:
                    raise ValueError('private audit copy changed during inventory')
                original_relative = str(Path(SURFACE) / relative)
                if _original(collector, original_relative, digest) is None:
                    result['retained_files'] += 1
                    continue
                entries.append(dict(path=str(selected.relative_to(directory)), root=name, attempt=key,
                    original=original_relative, sha256=digest, signature=signature,
                    bytes=item['bytes'], allocated_bytes=item['allocated_bytes'],
                    parents=auxiliary._parents(selected, directory)))
        record = dict(format=FORMAT, owner=owner, identity=identity, repo=str(collector.repo),
            checkpoint={key: witness[key] for key in ('commit', 'path', 'blob_sha256')},
            files=entries, bindings=bindings, retained_files=result['retained_files'])
    result['retained_files'] = 0
    ready = {}
    for entry in record['files']:
        relative = Path(entry['path'])
        name = entry['root']
        if name not in allowed or relative.is_absolute() or '..' in relative.parts:
            raise ValueError('audit retirement receipt escapes its allowlist')
        path = directory / relative
        if not path.is_relative_to(directory / name) or path == directory / name:
            raise ValueError('audit retirement path escapes its root')
        expected = str(Path(SURFACE) / path.relative_to(directory / name))
        attempt = allowed[name]
        key = str(attempt.relative_to(directory))
        if entry['original'] != expected or entry['attempt'] != key:
            raise ValueError('audit original binding changed')
        if not path.exists() and not path.is_symlink():
            continue
        digest, signature = _hash(path)
        if signature != entry['signature'] or digest != entry['sha256']:
            raise ValueError('private audit copy changed')
        original = _original(collector, expected, entry['sha256'])
        if original is None:
            result['retained_files'] += 1
            continue
        ready.setdefault(name, []).append((path, entry, original))
    result['retained_files'] += record['retained_files']
    if not apply:
        return dict(result, status='eligible', files=sum(map(len, ready.values())))
    if collector.cancelled():
        return dict(result, status='cancelled')
    if not receipt.exists():
        atomic(receipt, canonical_json(record))
    receipt_signature = retained._signature(retained.regular_path(receipt).stat())
    from .storage import _attempt_directories
    pins = [attempt / 'retain-builds.json' for attempt in _attempt_directories(directory)]
    dirty = set()
    try:
        for name, entries in ready.items():
            attempt = allowed[name]
            key = str(attempt.relative_to(directory))
            bound = auxiliary._binding(attempt, collector, witness, owner)
            if bound != record['bindings'][key]:
                raise ValueError('audit preparation changed')
            with collector.retirement_lock() as acquired:
                if not acquired or collector.cancelled():
                    return dict(result, status='cancelled' if collector.cancelled() else 'coordinator_busy')
                if not collector.checkpoint_current(witness):
                    return dict(result, status='checkpoint_changed')
                if collector._owner_pending(owner):
                    return dict(result, status='publication_pending')
                guard()
                if (list(retained._signature(retained.regular_path(attempt / 'prepared.json').stat()))
                        != bound['prepared_signature']):
                    raise ValueError('audit preparation changed before retirement')
                inventory(directory / name)
                if not retained._untracked(directory / name):
                    continue
                for path, entry, original in entries:
                    if collector.cancelled():
                        return dict(result, status='cancelled')
                    if any(pin.exists() or pin.is_symlink() for pin in pins):
                        raise ValueError('private evidence is retained by an operator pin')
                    if (auxiliary._parents(path, directory) != entry['parents']
                            or list(retained._signature(retained.regular_path(path).stat())) != entry['signature']):
                        raise ValueError('private audit copy changed before retirement')
                    try:
                        unchanged = (auxiliary._parents(original['path'], collector.repo.parent) == original['parents']
                            and list(retained._signature(retained.regular_path(original['path']).stat())) == original['signature'])
                    except FileNotFoundError:
                        unchanged = False
                    if not unchanged:
                        result['retained_files'] += 1
                        continue
                    path.unlink()
                    dirty.add(path.parent)
                    result['removed'] += 1
                    result['removed_bytes'] += entry['bytes']
                    result['removed_allocated_bytes'] += entry['allocated_bytes']
    finally:
        error = None
        for parent in sorted(dirty):
            try:
                _sync_dir(parent)
            except OSError as failure:
                error = error or failure
        if error is not None:
            raise error
    # Existing durable completion compacts only after EVERY selected input is
    # absent. Missing/changed originals leave their private copies retryable.
    return auxiliary._complete(collector, directory, owner, guard, witness,
        receipt, receipt_signature, record, result)
