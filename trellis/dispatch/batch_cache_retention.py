"""Discard unchanged terminal batch donors after durable no-materialization ack.

The existing compact batch is the recovery record. Capture inventories stay
until the last removal is durable, then disappear; no new historical corpus is
created. Unknown or changed donor files remain available for inspection.
"""
import errno
import hashlib
import os
from pathlib import Path, PurePosixPath
import re
import stat

from . import batch_cache, retained_files as retained, terminal_batches as batches
from . import terminal_retention as terminal
from .auxiliary_retention import _binding, _parents
from .candidate import canonical_json, digest, _sync_dir
from .publication_retention import _json, _read

MAX_CAPTURE = 64 << 20
# Scheduling granularity only: every qualifying member is drained.
FILES_PER_LEASE = 128


def _sha(value):
    return isinstance(value, str) and re.fullmatch('[0-9a-f]{64}', value) is not None


def _files(value):
    if (not isinstance(value, dict)
            or set(value) != {'format', 'binding', 'sources', 'files', 'allocated_copy_bytes', 'inodes', 'seeded_oleans'}
            or value['format'] != batch_cache.FORMAT or not isinstance(value['files'], dict)):
        raise ValueError('unsupported terminal cache inventory')
    result = {}
    for name, entry in value['files'].items():
        path = PurePosixPath(name)
        if (not isinstance(name, str) or not name or str(path) != name or path.is_absolute()
                or '\\' in name or '\0' in name or any(p in ('', '.', '..', '.git') for p in name.split('/'))
                or any(path.parts[i:i+2] == ('.lake', 'packages') for i in range(len(path.parts)))
                or not isinstance(entry, dict) or set(entry) != {'bytes', 'sha256'}
                or type(entry['bytes']) is not int or entry['bytes'] < 0 or not _sha(entry['sha256'])):
            raise ValueError('invalid terminal cache member')
        result[name] = entry
    return result


def retire(collector, review_id, current, *, apply=False, runtime_admission=None):
    result = dict(status='ineligible', caches=0, candidate_files=0, removed_files=0, retained_files=0,
                  removed_directories=0, removed_bytes=0, removed_allocated_bytes=0)
    if type(review_id) is not int or review_id < 0:
        raise ValueError('invalid terminal cache review ID')
    if collector.cancelled() or current() is None:
        return result
    parent = collector.root / 'batch-caches' / str(review_id)
    if not parent.exists() and not parent.is_symlink():
        return dict(result, status='absent')
    _parents(parent/'.scope', collector.root)
    for cache in sorted(parent.iterdir()):
        if not _sha(cache.name):
            continue  # Unknown staging/operator directories are not captures.
        value = _retire_cache(collector, review_id, cache, current, apply, runtime_admission)
        for key in result:
            if key != 'status':
                result[key] += value.get(key, 0)
        result['status'] = value['status']
    return result


def _retire_cache(c, review_id, cache, current, apply, admission):
    from .storage import _attempt_directories
    result = dict(status='ineligible', caches=0, candidate_files=0, removed_files=0, retained_files=0,
                  removed_directories=0, removed_bytes=0, removed_allocated_bytes=0)
    observed = current()
    if observed is None or c.cancelled():
        return result
    if admission is not None:
        admission()
    capture = cache/'capture.json'
    if not capture.exists() and not capture.is_symlink():
        return dict(result, status='absent')
    capture_signature = retained._signature(retained.regular_path(capture).stat())
    capture_parents = _parents(capture, c.root)
    ack = c.root/'reviews'/str(review_id)/'batch.json'
    if not ack.exists() and not ack.is_symlink():
        return dict(result, status='no_terminal_ack')
    ack_info = retained.regular_path(ack).stat()
    if ack_info.st_size > batches.MAX_METADATA_BYTES:
        return dict(result, status='no_terminal_ack')
    ack_signature = retained._signature(ack_info)
    ack_parents = _parents(ack, c.root)
    record = _json(_read(ack, batches.MAX_METADATA_BYTES))
    if record.get('format') != batches.FORMAT:
        return dict(result, status='no_terminal_ack')
    if canonical_json(batches._compact(record, review_id)) != canonical_json(observed):
        raise ValueError('terminal batch acknowledgement differs')
    witness = c.witness()
    if witness.get('terminal_batches', {}).get(str(review_id)) != observed:
        return dict(result, status='not_committed')
    terminal._git(c.repo, 'merge-base', '--is-ancestor', record['checkpoint'], witness['commit'])
    value = _json(_read(capture, MAX_CAPTURE))
    files = _files(value)
    binding = value['binding']
    if (not isinstance(binding, dict) or set(binding) != {'base_sha256', 'review_id', 'receipt', 'workers'}
            or binding['base_sha256'] != cache.name or type(binding['review_id']) is not int
            or binding['review_id'] != review_id or not observed['task_ids']
            or canonical_json(binding['receipt']) != canonical_json(observed['receipt'])
            or not record['objects'] or canonical_json(binding['workers']) != canonical_json(record['objects'])):
        raise ValueError('terminal donor belongs to another batch capture')
    review = c.root/f'review-{review_id}'
    reviewer = dict(sha256=digest(canonical_json(observed['receipt'])))
    owners = {**observed['tasks'], review.name:reviewer}
    if witness['owners'].get(review.name) != reviewer:
        return dict(result, status='not_committed')
    guard = retained.terminal_owner_guard(c.root, review, owners)
    workers = [c.root/key for key in observed['task_ids']]
    worker_guards = [retained.terminal_owner_guard(c.root, owner, owners) for owner in workers]
    prepared = _binding(review, c, witness, dict(kind='review', id=review_id))
    pins = [attempt/'retain-builds.json' for owner in (review, *workers)
            for attempt in _attempt_directories(owner)]
    pins.extend((cache/'retain-builds.json', ack.parent/'retain-state.json'))
    candidates, directories = {}, {}
    donor = cache/'repo'
    for name, entry in files.items():
        path = donor/name
        parent = path.parent
        while parent.is_relative_to(donor):
            try:
                directories[parent] = _parents(parent/'.directory-fence', c.root)
            except FileNotFoundError:
                pass
            parent = parent.parent
        # Missing files include already-pruned builds and interrupted unlinks.
        # Only currently present, unchanged regular files acquire delete authority.
        try:
            parents = _parents(path, c.root)
            info = path.lstat()
        except FileNotFoundError:
            continue
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            result['retained_files'] += 1
            continue
        signature = retained._signature(info)
        if info.st_size != entry['bytes']:
            result['retained_files'] += 1
            continue
        sha = hashlib.sha256()
        with retained.regular_path(path).open('rb') as stream:
            while True:
                if c.cancelled():return dict(result, status='cancelled')
                block = stream.read(1 << 20)
                if not block:break
                sha.update(block)
        if retained._signature(retained.regular_path(path).stat()) != signature:
            raise ValueError('terminal donor changed while hashing')
        if sha.hexdigest() != entry['sha256'] or not retained._untracked_cache(path, c.root):
            result['retained_files'] += 1
            continue
        candidates[path] = (signature, parents)
    if not retained._untracked_cache(capture, c.root):
        return dict(result, status='tracked')
    if not apply:
        return dict(result, status='eligible', candidate_files=len(candidates))

    def fence():
        if c.cancelled():raise InterruptedError('terminal donor retirement cancelled')
        if admission is not None:admission()
        if current() != observed or not c.checkpoint_current(witness):
            raise ValueError('terminal donor state or Git checkpoint changed')
        if any(c._same_task_pending(int(key)) for key in observed['task_ids']):
            raise ValueError('terminal donor still has pending publication')
        guard()
        for check in worker_guards:check()
        if any(pin.exists() or pin.is_symlink() for pin in pins):
            raise ValueError('terminal donor or owner is pinned')
        if (retained._signature(retained.regular_path(capture).stat()) != capture_signature
                or _parents(capture,c.root) != capture_parents
                or retained._signature(retained.regular_path(ack).stat()) != ack_signature
                or _parents(ack,c.root) != ack_parents
                or list(retained._signature(retained.regular_path(review/'prepared.json').stat())) != prepared['prepared_signature']):
            raise ValueError('terminal donor receipt, acknowledgement or preparation changed')

    # Re-establish the ack's durability before first unlink, including recovery
    # after an earlier atomic acknowledgement replacement.
    with retained.regular_path(ack).open('rb') as stream:os.fsync(stream.fileno())
    _sync_dir(ack.parent)
    # Include parents of already-absent members: a prior unlink can precede
    # its directory fsync when the process crashes.
    dirty = dict(directories)
    grouped = {}
    for path, identity in candidates.items():
        grouped.setdefault(path.parent, []).append((path, identity))
    try:
        for members in grouped.values():
            for offset in range(0, len(members), FILES_PER_LEASE):
                with c.retirement_lock() as acquired:
                    if not acquired:return dict(result,status='coordinator_busy')
                    fence()
                    for path,(signature,parents) in members[offset:offset+FILES_PER_LEASE]:
                        if c.cancelled():return dict(result,status='cancelled')
                        if any(pin.exists() or pin.is_symlink() for pin in pins):
                            raise ValueError('terminal donor or owner is pinned')
                        if (_parents(path,c.root) != parents
                                or retained._signature(retained.regular_path(path).stat()) != signature
                                or not retained._untracked_cache(path, c.root)):
                            raise ValueError('terminal donor changed before unlink')
                        info=path.stat();path.unlink();dirty[path.parent]=parents
                        result['removed_files']+=1;result['removed_bytes']+=info.st_size
                        result['removed_allocated_bytes']+=info.st_blocks*512
    finally:
        for parent,expected in dirty.items():
            if _parents(parent/'.sync-fence',c.root) != expected:
                raise ValueError('terminal donor parent changed before sync')
            _sync_dir(parent)
    ordered = sorted(directories.items(), key=lambda item:len(item[0].parts), reverse=True)
    for offset in range(0, len(ordered), FILES_PER_LEASE):
        with c.retirement_lock() as acquired:
            if not acquired:return dict(result,status='coordinator_busy')
            fence()
            for path,parents in ordered[offset:offset+FILES_PER_LEASE]:
                if c.cancelled():return dict(result,status='cancelled')
                if any(pin.exists() or pin.is_symlink() for pin in pins):
                    raise ValueError('terminal donor or owner is pinned')
                if _parents(path/'.directory-fence',c.root) != parents:
                    raise ValueError('terminal donor directory changed before rmdir')
                info=path.lstat()
                try:path.rmdir()
                except OSError as error:
                    if error.errno in (errno.ENOTEMPTY,errno.EEXIST):continue
                    raise
                _sync_dir(path.parent)
                result['removed_directories']+=1
                result['removed_allocated_bytes']+=info.st_blocks*512
    with c.retirement_lock() as acquired:
        if not acquired:return dict(result,status='coordinator_busy')
        # Compact batch acknowledgement remains; the original capture inventory
        # is obsolete once every selected unlink and directory entry is durable.
        fence()
        if not retained._untracked_cache(capture, c.root):raise ValueError('terminal capture became tracked')
        info=capture.stat();capture.unlink();_sync_dir(cache)
        result['removed_files']+=1;result['removed_bytes']+=info.st_size
        result['removed_allocated_bytes']+=info.st_blocks*512
        result['caches']=1
    return dict(result,status='retired')
