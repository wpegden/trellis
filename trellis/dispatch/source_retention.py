"""Retire only Git-recoverable terminal reviewer/verifier source checkouts.

This scope intentionally removes private tracked checkout bytes. It never
changes Git objects/indexes or the generic untracked-only retention policy.
Workers, advisory scratch, chats, configuration and result evidence stay intact.
"""
import hashlib
from pathlib import Path
import shutil
import stat
import subprocess

from . import retained_files as retained, terminal_retention as terminal
from .candidate import canonical_json, _sync_dir
from .publication import atomic

FORMAT = 'trellis-terminal-source-checkout-retirement/1'
RECEIPT = 'source-checkouts-retired.json'


class NotRecoverable(ValueError):
    """Ordinary unique private bytes stay available without failing cleanup."""



def roots(directory):
    from .storage import _attempt_directories
    for attempt in _attempt_directories(directory):
        for repo in (attempt / 'repo', attempt / 'repo/.trellis/supervisor/repo'):
            for name in ('Tablet', 'reference'):
                yield attempt, repo / name


def tombstone(path):
    return path.with_name('.' + path.name + '.source-retiring')


def inventory(path):
    for parent in (path, *path.parents):
        if parent.is_symlink():
            raise ValueError('source checkout cannot traverse symlinks')
    info = path.stat()
    if not stat.S_ISDIR(info.st_mode):
        raise ValueError('source checkout root is not a directory')
    content, pending = {}, [path]
    while pending:
        current = pending.pop()
        for child in current.iterdir():
            info = child.lstat()
            name = str(child.relative_to(path))
            if stat.S_ISDIR(info.st_mode) and child.name != '.git':
                content[name] = dict(directory=[info.st_dev, info.st_ino, info.st_mode])
                pending.append(child)
            elif stat.S_ISREG(info.st_mode) and info.st_nlink == 1:
                content[name] = dict(signature=list(retained._signature(info)), bytes=info.st_size,
                    allocated_bytes=info.st_blocks * 512)
            else:
                raise ValueError('source checkout contains an alias, repository or special file')
    return content


def _binding(attempt, collector, witness, owner):
    inputs = {name: list(retained._signature(retained.regular_path(attempt / name).stat()))
              for name in ('prepared.json', 'base-manifest.json')}
    prepared = terminal._json(attempt / 'prepared.json')
    if Path(prepared['source_repo']).resolve() != collector.repo:
        raise ValueError('source preparation belongs to another project')
    if owner['kind'] == 'review' and prepared['request_id'] != owner['id']:
        raise ValueError('source preparation belongs to another reviewer')
    head = prepared['source_head']
    if not isinstance(head, str) or len(head) not in (40, 64) or any(c not in '0123456789abcdef' for c in head):
        raise ValueError('invalid source recovery commit')
    terminal._git(collector.repo, 'merge-base', '--is-ancestor', head, witness['commit'])
    manifest_bytes = retained.regular_path(attempt / 'base-manifest.json').read_bytes()
    import json
    manifest = json.loads(manifest_bytes)
    if not isinstance(manifest, dict):
        raise ValueError('invalid prepared source manifest')
    if any(list(retained._signature(retained.regular_path(attempt / name).stat())) != value
           for name, value in inputs.items()):
        raise ValueError('source preparation changed during recovery binding')
    return dict(source_head=head, base_manifest_sha256=hashlib.sha256(manifest_bytes).hexdigest(), inputs=inputs), manifest


def _prove(path, collector, binding, manifest, content):
    """Read/hash private files off-lock and prove exact canonical Git recovery."""
    raw = terminal._git(collector.repo, 'ls-tree', '-r', '-z', binding['source_head'], '--', path.name)
    expected = {}
    for row in raw.split(b'\0'):
        if not row:
            continue
        metadata, encoded = row.split(b'\t', 1)
        mode, kind, oid = metadata.decode().split()
        name = encoded.decode()
        if not name.startswith(path.name + '/') or kind != 'blob' or mode not in ('100644', '100755'):
            raise ValueError('source recovery tree contains an unsupported entry')
        expected[name[len(path.name) + 1:]] = (mode, oid)
    files = {name: item for name, item in content.items() if 'directory' not in item}
    if set(files) != set(expected):
        raise NotRecoverable('source checkout has extra or missing Git files')
    # Ensure referenced immutable objects actually remain available in the
    # canonical repository, not merely in this disposable private checkout.
    if expected:
        oids = [expected[name][1] for name in sorted(expected)]
        checked = subprocess.check_output(['git', '-C', str(collector.repo), 'cat-file',
            '--batch-check=%(objectname) %(objecttype) %(objectsize)'],
            input=('\n'.join(oids) + '\n').encode()).decode().splitlines()
        for name, oid, row in zip(sorted(expected), oids, checked, strict=True):
            fields = row.split()
            if len(fields) != 3 or fields[:2] != [oid, 'blob'] or not fields[2].isdigit():
                raise ValueError('canonical source recovery object is unavailable')
            if int(fields[2]) != files[name]['bytes']:
                raise NotRecoverable('source checkout differs from recoverable Git object size')
    for name, item in files.items():
        source = path / name
        mode, oid = expected[name]
        obj = hashlib.new('sha1' if len(oid) == 40 else 'sha256')
        obj.update(b'blob ' + str(item['bytes']).encode() + b'\0')
        value = hashlib.sha256()
        with source.open('rb') as stream:
            for chunk in iter(lambda: stream.read(1 << 20), b''):
                obj.update(chunk); value.update(chunk)
        info = source.stat()
        if list(retained._signature(info)) != item['signature']:
            raise ValueError('source checkout changed during recovery proof')
        if (bool(info.st_mode & 0o111) != (mode == '100755')
                or obj.hexdigest() != oid or value.hexdigest() != manifest.get(path.name + '/' + name)):
            raise NotRecoverable('source checkout differs from recoverable prepared Git bytes')
    if inventory(path) != content:
        raise ValueError('source checkout changed during recovery proof')


def retire(collector, directory, observed, *, apply=False):
    result = dict(status='absent', removed=0, removed_bytes=0, removed_allocated_bytes=0)
    if collector.cancelled():
        return dict(result, status='cancelled')
    root, directory = retained.owner_directory(collector.root, directory)
    owner = retained.owner_identity(directory)
    if owner['kind'] not in ('review', 'verifier'):
        return dict(result, status='worker_preserved')
    if directory.name not in observed or collector._owner_pending(owner):
        return dict(result, status='not_terminal_or_pending')
    allowed = {str(path.relative_to(directory)): attempt for attempt, path in roots(directory)}
    if not any((directory / name).exists() or (directory / name).is_symlink()
               or tombstone(directory / name).exists() or tombstone(directory / name).is_symlink() for name in allowed):
        return result
    guard = retained.terminal_owner_guard(root, directory, observed)
    identity = observed[directory.name]
    if isinstance(identity, terminal._DeferredVerifierIdentity):
        identity = identity.resolve()
    witness = collector.witness()
    if witness.get('owners', witness['tasks']).get(directory.name) != identity:
        return dict(result, status='not_committed')
    receipt = directory / RECEIPT
    if not retained._untracked(receipt):
        raise ValueError('source retirement receipt is Git tracked')
    if receipt.exists() or receipt.is_symlink():
        record = terminal._json(receipt)
        if (record.get('format') != FORMAT or record.get('owner') != owner
                or record.get('identity') != identity or record.get('repo') != str(collector.repo)):
            raise ValueError('source retirement receipt changed identity')
        terminal._git(collector.repo, 'merge-base', '--is-ancestor', record['checkpoint']['commit'], witness['commit'])
    else:
        trees = {}
        for name, attempt in allowed.items():
            path = directory / name
            if tombstone(path).exists() or tombstone(path).is_symlink():
                raise ValueError('source tombstone has no recovery receipt')
            if not path.exists() and not path.is_symlink():
                continue
            binding, manifest = _binding(attempt, collector, witness, owner)
            content = inventory(path)
            try:
                _prove(path, collector, binding, manifest, content)
            except NotRecoverable as error:
                return dict(result, status='not_recoverable', retained_sources=1, reason=str(error))
            info = path.stat()
            trees[name] = dict(identity=[info.st_dev, info.st_ino], files=content, binding=binding)
        record = dict(format=FORMAT, owner=owner, identity=identity, repo=str(collector.repo),
            checkpoint={key: witness[key] for key in ('commit', 'path', 'blob_sha256')}, trees=trees)
    if any(name not in allowed for name in record['trees']):
        raise ValueError('source receipt escapes its allowlist')
    if not apply:
        return dict(result, status='eligible', trees=len(record['trees']))
    if collector.cancelled():
        return dict(result, status='cancelled')
    if not receipt.exists():
        atomic(receipt, canonical_json(record))
    for name, entry in record['trees'].items():
        path = directory / name; trash = tombstone(path)
        if not path.exists() and not path.is_symlink() and not trash.exists() and not trash.is_symlink():
            continue
        if (path.exists() or path.is_symlink()) and (trash.exists() or trash.is_symlink()):
            raise ValueError('both source checkout and tombstone exist')
        selected = trash if trash.exists() or trash.is_symlink() else path
        binding, _ = _binding(allowed[name], collector, witness, owner)
        if binding != entry['binding']:
            raise ValueError('source recovery binding changed')
        content = inventory(selected); info = selected.stat()
        if ([info.st_dev, info.st_ino] != entry['identity']
                or any(entry['files'].get(n) != item for n, item in content.items())
                or (selected == path and content != entry['files'])):
            raise ValueError('source checkout changed after recovery proof')
        if not retained._untracked(trash):
            raise ValueError('source tombstone is Git tracked')
        with collector.retirement_lock() as acquired:
            if not acquired:
                return dict(result, status='cancelled' if collector.cancelled() else 'coordinator_busy')
            if collector.cancelled():
                return dict(result, status='cancelled')
            if not collector.checkpoint_current(witness):
                return dict(result, status='checkpoint_changed')
            if collector._owner_pending(owner):
                return dict(result, status='publication_pending')
            guard()
            if any(list(retained._signature(retained.regular_path(allowed[name] / key).stat())) != value
                   for key, value in binding['inputs'].items()):
                raise ValueError('source preparation changed before retirement')
            if (inventory(selected) != content
                    or retained._signature(selected.stat()) != retained._signature(info)):
                raise ValueError('source subtree changed before retirement rename')
            if not retained._untracked(trash) or not retained._untracked(receipt):
                raise ValueError('source retirement metadata became Git tracked')
            if selected == path:
                path.rename(trash)
        _sync_dir(path.parent)
        guard()
        remaining = inventory(trash)
        info = trash.stat()
        if ([info.st_dev, info.st_ino] != entry['identity']
                or any(entry['files'].get(n) != item for n, item in remaining.items())):
            raise ValueError('source tombstone changed before deletion')
        if collector.cancelled():
            return dict(result, status='cancelled')
        shutil.rmtree(trash)
        _sync_dir(path.parent)
        result['removed'] += 1
        result['removed_bytes'] += sum(item.get('bytes', 0) for item in remaining.values())
        result['removed_allocated_bytes'] += sum(item.get('allocated_bytes', 0) for item in remaining.values())
    return dict(result, status='retired' if result['removed'] else 'unchanged')
