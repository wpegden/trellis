"""Discard generated terminal framework source/skills, preserving diagnostics."""
from __future__ import annotations

import argparse
import fcntl
import json
from pathlib import Path
import shutil
import stat

from . import retained_files as retained
from . import terminal_retention as terminal
from .candidate import canonical_json, _sync_dir
from .publication import atomic

FORMAT = 'trellis-terminal-framework-retirement/1'
OWNER_FORMAT = 'trellis-terminal-framework-retirement/2'
RECEIPT = 'framework-inputs-retired.json'
MAX_ENTRIES = 10000
MAX_BYTES = 256 * 1024 * 1024


def roots(directory):
    from .storage import _attempt_directories
    for attempt in _attempt_directories(directory):
        for repo in (attempt / 'repo', attempt / 'repo/.trellis/supervisor/repo'):
            for name in ('src', 'skills'):
                yield repo / '.trellis/runtime' / name


def tombstone(path):
    return path.with_name('.' + path.name + '.terminal-retiring')


def inventory(path):
    """Bounded metadata-only traversal; reject aliases and nested repositories."""
    for parent in (path, *path.parents):
        if parent.is_symlink():
            raise ValueError('framework tree cannot traverse a symlink')
    info = path.stat()
    if not stat.S_ISDIR(info.st_mode):
        raise ValueError('framework root is not a directory')
    result, pending, size = {}, [path], 0
    while pending:
        current = pending.pop()
        for child in current.iterdir():
            info = child.lstat()
            if child.name == '.git' or stat.S_ISLNK(info.st_mode):
                raise ValueError('framework tree contains a repository or symlink')
            relative = str(child.relative_to(path))
            if stat.S_ISDIR(info.st_mode):
                pending.append(child)
                result[relative] = dict(directory=True)
            elif stat.S_ISREG(info.st_mode) and info.st_nlink == 1:
                size += info.st_size
                result[relative] = dict(signature=list(retained._signature(info)),
                    bytes=info.st_size, allocated_bytes=info.st_blocks * 512)
            else:
                raise ValueError('framework tree contains an alias or special file')
            if len(result) > MAX_ENTRIES or size > MAX_BYTES:
                raise ValueError('framework tree exceeds the bounded cleanup limit')
    return result


def retire(collector, directory, observed, *, apply=False, budget=None, max_trees=None):
    if collector.cancelled():
        return dict(status='cancelled', removed=0)
    root, directory = retained.owner_directory(collector.root, directory)
    task_id = directory.name
    owner = retained.owner_identity(directory)
    worker = owner['kind'] == 'worker'
    binding = dict(task_id=owner['id']) if worker else dict(owner=owner)
    format = FORMAT if worker else OWNER_FORMAT
    identity_field = 'task' if worker else 'identity'
    result = dict(**binding, status='eligible', removed=0,
        removed_bytes=0, removed_allocated_bytes=0, pending=False, files=0, bytes=0, allocated_bytes=0)
    if task_id not in observed or collector._owner_pending(owner):
        return dict(result, status='not_terminal_or_pending')
    if not any(path.exists() or path.is_symlink() or tombstone(path).exists()
               or tombstone(path).is_symlink() for path in roots(directory)):
        return dict(result, status='absent')
    guard = retained.terminal_owner_guard(root, directory, observed)
    identity = observed[task_id]
    if isinstance(identity, terminal._DeferredVerifierIdentity):
        identity = identity.resolve()
    witness = collector.witness()
    if witness.get('owners', witness['tasks']).get(task_id) != identity:
        return dict(result, status='not_committed')
    prepared = terminal._json(directory / 'prepared.json')
    if Path(prepared['source_repo']).resolve() != collector.repo:
        raise ValueError('task preparation belongs to another project')
    if not worker:
        if owner['kind'] == 'review' and prepared['request_id'] != owner['id']:
            raise ValueError('private preparation belongs to another owner request')
        terminal._git(collector.repo, 'merge-base', '--is-ancestor', prepared['source_head'], witness['commit'])
    receipt_path = directory / RECEIPT
    if not retained._untracked(receipt_path):
        raise ValueError('framework retirement receipt is Git tracked')
    allowed = {str(path.relative_to(directory)) for path in roots(directory)}
    if receipt_path.exists() or receipt_path.is_symlink():
        record = terminal._json(receipt_path)
        if (not isinstance(record, dict) or record.get('format') != format
                or any(record.get(key) != value for key, value in binding.items())
                or record.get('repo') != str(collector.repo)
                or record.get(identity_field) != identity):
            raise ValueError('framework retirement receipt changed identity')
        terminal._git(collector.repo, 'merge-base', '--is-ancestor', record['checkpoint']['commit'], witness['commit'])
    else:
        trees = {}
        for relative in sorted(allowed):
            path = directory / relative
            if tombstone(path).exists() or tombstone(path).is_symlink():
                raise ValueError('framework tombstone has no retirement receipt')
            if not path.exists() and not path.is_symlink():
                continue
            if not retained._untracked(path):
                continue
            content = inventory(path)
            info = path.stat()
            trees[relative] = dict(identity=[info.st_dev, info.st_ino], files=content)
        record = dict(format=format, **binding, repo=str(collector.repo),
            **{identity_field: identity}, checkpoint={key: witness[key] for key in ('commit', 'path', 'blob_sha256')},
            trees=trees)
    for relative, entry in record['trees'].items():
        if relative not in allowed:
            raise ValueError('framework receipt escapes its allowlist')
        for item in entry['files'].values():
            if not item.get('directory'):
                result['files'] += 1
                result['bytes'] += item['bytes']
                result['allocated_bytes'] += item['allocated_bytes']
    if not apply or not record['trees']:
        return result
    if collector.cancelled():
        return dict(result, status='cancelled')
    if not receipt_path.exists():
        atomic(receipt_path, canonical_json(record))
    # Drain all trees unless an explicit caller asks for a partial operation.
    for relative, entry in record['trees'].items():
        if collector.cancelled():
            return dict(result, status='cancelled')
        if budget is not None and budget['files'] <= 0:
            result['pending'] = True
            break
        path = directory / relative
        trash = tombstone(path)
        if not path.exists() and not path.is_symlink() and not trash.exists() and not trash.is_symlink():
            continue
        if path.exists() and (trash.exists() or trash.is_symlink()):
            raise ValueError('both framework root and retirement tombstone exist')
        selected = trash if trash.exists() or trash.is_symlink() else path
        content = inventory(selected)
        info = selected.stat()
        if ([info.st_dev, info.st_ino] != entry['identity']
                or any(entry['files'].get(name) != item for name, item in content.items())):
            raise ValueError('framework tree changed after retirement inventory')
        if not retained._untracked(path) or not retained._untracked(trash):
            continue
        guard()
        with collector.retirement_lock() as acquired:
            if not acquired:
                return dict(result, status='cancelled' if collector.cancelled() else 'coordinator_busy')
            if not collector.checkpoint_current(witness):
                return dict(result, status='checkpoint_changed')
            if collector._owner_pending(owner):
                return dict(result, status='publication_pending')
            guard()
            if not retained._untracked(path) or not retained._untracked(trash):
                continue
            if retained._signature(selected.stat()) != retained._signature(info):
                raise ValueError('framework root changed before retirement')
            if collector.cancelled():
                return dict(result, status='cancelled')
            if selected == path:
                path.rename(trash)
        _sync_dir(path.parent)
        guard()
        remaining = inventory(trash)
        if (any(entry['files'].get(name) != item for name, item in remaining.items())
                or not retained._untracked(path) or not retained._untracked(trash)):
            raise ValueError('framework tree changed before deletion')
        if collector.cancelled():
            return dict(result, status='cancelled')
        shutil.rmtree(trash)
        _sync_dir(path.parent)
        result['removed'] += 1
        result['removed_bytes'] += sum(item.get('bytes', 0) for item in content.values())
        result['removed_allocated_bytes'] += sum(item.get('allocated_bytes', 0) for item in content.values())
        if budget is not None:
            budget['files'] -= 1
        if max_trees is not None and result['removed'] >= max_trees:
            result['pending'] = True
            break
    result['status'] = 'retired' if result['removed'] else 'unchanged'
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--runtime', required=True, type=Path)
    parser.add_argument('--repo', required=True, type=Path)
    parser.add_argument('--task', action='append', type=int)
    parser.add_argument('--owner', action='append', help='worker ID, review-N, or verifier-N')
    parser.add_argument('--apply', action='store_true')
    args = parser.parse_args()
    root = args.runtime / 'dispatch'
    observed = terminal.terminal_owners(terminal._json(args.runtime / 'protocol_state.json'))
    collector = terminal.Collector(root, args.repo)
    ids = [str(value) for value in (args.task or [])] + (args.owner or [])
    if not ids:
        ids = sorted(observed)
    with (root / 'build-cleanup.lock').open('a+b') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        for task_id in ids:
            try:
                value = retire(collector, root / str(task_id), observed, apply=args.apply)
            except Exception as error:
                value = dict(task_id=task_id, status='retained', reason=str(error))
            print(json.dumps(value), flush=True)


if __name__ == '__main__':
    main()
