"""Retire exact Git-recoverable paper/process-memory copies of terminal owners.

This is a separate tracked-checkout policy, not an exception to _untracked.
The prepared base manifest does NOT cover these files. Canonical Git blobs at
prepared.source_head prove recoverability directly; differing/extra bytes stay.
"""
import hashlib
from pathlib import Path
import stat
import subprocess

from . import retained_files as retained, terminal_retention as terminal
from .source_retention import inventory
from .candidate import canonical_json, _sync_dir
from .publication import atomic

FORMAT = 'trellis-terminal-auxiliary-checkout-retirement/1'
RECEIPT = 'auxiliary-checkouts-retired.json'


def roots(directory):
    from .storage import _attempt_directories
    for attempt in _attempt_directories(directory):
        for repo in (attempt / 'repo', attempt / 'repo/.trellis/supervisor/repo'):
            yield attempt, repo, 'paper/paper.tex'
            yield attempt, repo, 'process-memory'


def _parents(path, directory):
    """Bind directory identities without timestamps changed by our own unlinks."""
    relative = path.relative_to(directory)
    if not relative.parts:
        raise ValueError('auxiliary file must be below its owner directory')
    result = {}
    parent = path.parent
    # Keep every ancestor lstat, but derive relative components only once.
    # This fence runs for every file; repeated relative_to work is substantial.
    for length in range(len(relative.parts) - 1, 0, -1):
        info = parent.lstat()
        if not stat.S_ISDIR(info.st_mode):
            raise ValueError('auxiliary checkout has a directory alias or special file')
        result['/'.join(relative.parts[:length])] = [info.st_dev, info.st_ino, info.st_mode]
        parent = parent.parent
    return result


def _binding(attempt, collector, witness, owner):
    path = retained.regular_path(attempt / 'prepared.json')
    before = list(retained._signature(path.stat()))
    prepared = terminal._json(path)
    if Path(prepared['source_repo']).resolve() != collector.repo:
        raise ValueError('auxiliary preparation belongs to another project')
    if owner['kind'] == 'review' and prepared['request_id'] != owner['id']:
        raise ValueError('auxiliary preparation belongs to another reviewer')
    head = prepared['source_head']
    if not isinstance(head, str) or len(head) not in (40, 64) or any(c not in '0123456789abcdef' for c in head):
        raise ValueError('invalid auxiliary recovery commit')
    terminal._git(collector.repo, 'merge-base', '--is-ancestor', head, witness['commit'])
    if list(retained._signature(path.stat())) != before:
        raise ValueError('auxiliary preparation changed during recovery proof')
    return dict(source_head=head, prepared_signature=before)


def _git_files(collector, head, surfaces):
    expected = {}
    raw = terminal._git(collector.repo, 'ls-tree', '-r', '-z', head, '--', *surfaces)
    for row in raw.split(b'\0'):
        if not row:
            continue
        metadata, encoded = row.split(b'\t', 1)
        mode, kind, oid = metadata.decode().split()
        name = encoded.decode()
        if not any(name == surface or name.startswith(surface + '/') for surface in surfaces):
            raise ValueError('auxiliary recovery tree escapes its scope')
        if kind == 'blob' and mode in ('100644', '100755'):
            expected[name] = dict(mode=mode, oid=oid)
    if expected:
        oids = sorted({entry['oid'] for entry in expected.values()})
        rows = subprocess.check_output(['git', '-C', str(collector.repo), 'cat-file',
            '--batch-check=%(objectname) %(objecttype) %(objectsize)'],
            input=('\n'.join(oids) + '\n').encode()).decode().splitlines()
        sizes = {}
        for oid, row in zip(oids, rows, strict=True):
            fields = row.split()
            if len(fields) != 3 or fields[:2] != [oid, 'blob'] or not fields[2].isdigit():
                raise ValueError('canonical auxiliary recovery object is unavailable')
            sizes[oid] = int(fields[2])
        for entry in expected.values():
            entry['bytes'] = sizes[entry['oid']]
    return expected


def retire(collector, directory, observed, *, apply=False):
    return _retire(collector, directory, observed, apply=apply)


def _retire(collector, directory, observed, *, apply=False, roots_fn=roots,
            format=FORMAT, receipt_name=RECEIPT,
            git_surfaces=('paper/paper.tex', 'process-memory'),
            file_surfaces=('paper/paper.tex',), admission=None, prepare_recovery=None):
    """Shared exact-Git proof engine; callers supply explicit private scopes."""
    result = dict(status='absent', removed=0, removed_bytes=0, removed_allocated_bytes=0, retained_files=0)
    if collector.cancelled():
        return dict(result, status='cancelled')
    root, directory = retained.owner_directory(collector.root, directory)
    owner = retained.owner_identity(directory)
    if directory.name not in observed or collector._owner_pending(owner):
        return dict(result, status='not_terminal_or_pending')
    allowed = {str((repo / surface).relative_to(directory)): (attempt, repo, surface)
               for attempt, repo, surface in roots_fn(directory)}
    receipt = directory / receipt_name
    if (not receipt.exists() and not receipt.is_symlink()
            and not any((directory / name).exists() or (directory / name).is_symlink() for name in allowed)):
        return result
    guard = retained.terminal_owner_guard(root, directory, observed)
    identity = observed[directory.name]
    if isinstance(identity, terminal._DeferredVerifierIdentity):
        identity = identity.resolve()
    witness = collector.witness()
    if witness.get('owners', witness['tasks']).get(directory.name) != identity:
        return dict(result, status='not_committed')
    # Optional scope-specific proof is prepared off-lock; its returned fence
    # performs only stable identity checks at each destructive lease.
    admission_record, admission_fence = admission(witness) if admission else (None, None)
    expected, bindings = {}, {}
    def binding(attempt):
        key = str(attempt.relative_to(directory))
        if key not in bindings:
            bindings[key] = _binding(attempt, collector, witness, owner)
        value = bindings[key]
        head = value['source_head']
        if head not in expected:
            expected[head] = _git_files(collector, head, git_surfaces)
        return key, value, expected[head]
    if receipt.exists() or receipt.is_symlink():
        record = terminal._json(receipt)
        if (record.get('format') != format or record.get('owner') != owner
                or record.get('identity') != identity or record.get('repo') != str(collector.repo)):
            raise ValueError('auxiliary retirement receipt changed identity')
        if record.get('completed') is True:
            if (record.get('files') != [] or type(record.get('retired_files')) is not int
                    or record['retired_files'] < 0 or type(record.get('retained_files')) is not int
                    or record['retained_files'] < 0):
                raise ValueError('invalid completed auxiliary retirement receipt')
            # Skip-only: future files, including a recreated selected pathname,
            # never acquire deletion authority from a completed inventory.
            guard()
            return dict(result, status='unchanged', retained_files=record['retained_files'])
        if record.get('admission') != admission_record:
            raise ValueError('auxiliary retirement admission changed')
        if not retained._untracked(receipt):
            raise ValueError('auxiliary retirement receipt is Git tracked')
        terminal._git(collector.repo, 'merge-base', '--is-ancestor', record['checkpoint']['commit'], witness['commit'])
    else:
        if not any((directory / name).exists() or (directory / name).is_symlink() for name in allowed):
            return result
        if not retained._untracked(receipt):
            raise ValueError('auxiliary retirement receipt is Git tracked')
        entries = []
        for name, (attempt, repo, surface) in allowed.items():
            path = directory / name
            if not path.exists() and not path.is_symlink():
                continue
            key, bound, git_files = binding(attempt)
            if surface not in file_surfaces:
                content = inventory(path)
                paths = [path / relative for relative, item in content.items() if 'directory' not in item]
            else:
                paths = [path]
            for path in sorted(paths):
                info = retained.regular_path(path).stat()
                relative = str(path.relative_to(repo))
                proof = git_files.get(relative)
                if (proof is None or info.st_size != proof['bytes']
                        or bool(info.st_mode & 0o111) != (proof['mode'] == '100755')):
                    result['retained_files'] += 1
                    continue
                signature = list(retained._signature(info))
                obj = hashlib.new('sha1' if len(proof['oid']) == 40 else 'sha256')
                obj.update(b'blob ' + str(info.st_size).encode() + b'\0')
                with path.open('rb') as stream:
                    for chunk in iter(lambda: stream.read(1 << 20), b''):
                        obj.update(chunk)
                if list(retained._signature(retained.regular_path(path).stat())) != signature:
                    raise ValueError('auxiliary checkout changed during recovery proof')
                if obj.hexdigest() != proof['oid']:
                    result['retained_files'] += 1
                    continue
                entries.append(dict(path=str(path.relative_to(directory)), root=name, attempt=key,
                    git_path=relative, proof=proof, signature=signature, bytes=info.st_size,
                    allocated_bytes=info.st_blocks * 512, parents=_parents(path, directory)))
        record = dict(format=format, owner=owner, identity=identity, repo=str(collector.repo),
            checkpoint={key: witness[key] for key in ('commit', 'path', 'blob_sha256')},
            bindings=bindings, files=entries, retained_files=result['retained_files'])
        if admission_record is not None:
            record['admission'] = admission_record
    ready = []
    for entry in record['files']:
        name = entry['root']
        if name not in allowed:
            raise ValueError('auxiliary receipt root escapes its allowlist')
        attempt, repo, surface = allowed[name]
        relative = Path(entry['git_path'])
        if (relative.is_absolute() or '..' in relative.parts
                or str(relative) != entry['git_path']
                or (str(relative) != surface if surface in file_surfaces else not str(relative).startswith(surface + '/'))):
            raise ValueError('auxiliary receipt file escapes its allowlist')
        path = repo / relative
        if str(path.relative_to(directory)) != entry['path']:
            raise ValueError('auxiliary receipt path changed')
        key, bound, git_files = binding(attempt)
        if (entry['attempt'] != key or record['bindings'].get(key) != bound
                or git_files.get(str(relative)) != entry['proof']):
            raise ValueError('auxiliary recovery binding changed')
        if path.exists() or path.is_symlink():
            ready.append((path, entry, attempt, bound))
    result['retained_files'] = record['retained_files']
    if not apply:
        return dict(result, status='eligible', files=len(ready))
    if collector.cancelled():
        return dict(result, status='cancelled')
    if prepare_recovery is not None:
        # A reader-backed scope must durably publish its exact recovery index
        # before the first unlink. Preparation runs off-lock; the returned cheap
        # fence participates in every existing destructive/completion lease.
        recovery_fence = prepare_recovery(record)
        prior_fence = admission_fence
        def admission_fence():
            if prior_fence is not None:
                prior_fence()
            recovery_fence()
    if not receipt.exists():
        atomic(receipt, canonical_json(record))
    receipt_signature = retained._signature(retained.regular_path(receipt).stat())
    # A checkout tree is the natural lease unit (typically ~210 memory files),
    # not a per-pass work cap. The broker can yield between roots while every
    # eligible root still drains. Recheck canonical controls for each lease.
    groups = {}
    for item in ready:
        groups.setdefault(item[1]['root'], []).append(item)
    # Pins can be added by an operator without the coordinator lock. Keep their
    # per-file checks, while process/preparation admission is once per root: a
    # committed terminal owner cannot legitimately start work under this lease.
    from .storage import _attempt_directories
    pins = [attempt / 'retain-builds.json' for attempt in _attempt_directories(directory)]
    if owner['kind'] == 'review':
        pins.append(root / 'reviews' / str(owner['id']) / 'retain-state.json')
    dirty = set()
    try:
        for group in groups.values():
            with collector.retirement_lock() as acquired:
                if not acquired:
                    return dict(result, status='cancelled' if collector.cancelled() else 'coordinator_busy')
                if not collector.checkpoint_current(witness):
                    return dict(result, status='checkpoint_changed')
                if collector._owner_pending(owner):
                    return dict(result, status='publication_pending')
                if not retained._untracked(receipt):
                    raise ValueError('auxiliary retirement receipt became Git tracked')
                guard()
                if admission_fence is not None:
                    admission_fence()
                # Each root belongs to one prepared attempt (validated above).
                attempt, bound = group[0][2:]
                if list(retained._signature(retained.regular_path(attempt / 'prepared.json').stat())) != bound['prepared_signature']:
                    raise ValueError('auxiliary preparation changed before retirement')
                for path, entry, attempt, bound in group:
                    if collector.cancelled():
                        return dict(result, status='cancelled')
                    if any(pin.exists() or pin.is_symlink() for pin in pins):
                        raise ValueError('private evidence is retained by an operator pin')
                    if (_parents(path, directory) != entry['parents']
                            or list(retained._signature(retained.regular_path(path).stat())) != entry['signature']):
                        raise ValueError('auxiliary checkout changed before retirement')
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
    return _complete(collector, directory, owner, guard, witness, receipt,
                     receipt_signature, record, result, admission_fence=admission_fence)


def _complete(collector, directory, owner, guard, witness, receipt, signature, record, result, *, admission_fence=None):
    """Discard restart inventory only after its unlinks are durable.

    Also handles restart after the last unlink but before fsync/compaction. The
    compact record authorizes no deletion, so later files are always preserved.
    """
    if collector.cancelled():
        return dict(result, status='cancelled')
    parents = {}
    paths = []
    for entry in record['files']:
        path = directory / entry['path']
        if path.exists() or path.is_symlink():
            return dict(result, status='inputs_reappeared')
        parents.setdefault(path.parent, entry['parents'])
        paths.append(path)
    # Repeated fsync is intentional: an interrupted prior pass may have unlinked
    # all files without reaching its finally block. Never compact on any error.
    guard()
    for parent, expected in parents.items():
        probe = parent / '.retirement-absence-probe'
        if _parents(probe, directory) != expected:
            raise ValueError('auxiliary parent changed before completion')
        _sync_dir(parent)
    with collector.retirement_lock() as acquired:
        if not acquired or collector.cancelled():
            return dict(result, status='cancelled' if collector.cancelled() else 'coordinator_busy')
        if not collector.checkpoint_current(witness):
            return dict(result, status='checkpoint_changed')
        if collector._owner_pending(owner):
            return dict(result, status='publication_pending')
        guard()
        if admission_fence is not None:
            admission_fence()
        if (retained._signature(retained.regular_path(receipt).stat()) != signature
                or not retained._untracked(receipt)):
            raise ValueError('auxiliary retirement receipt changed before completion')
        for key, binding in record['bindings'].items():
            if list(retained._signature(retained.regular_path(directory / key / 'prepared.json').stat())) != binding['prepared_signature']:
                raise ValueError('auxiliary preparation changed before completion')
        if any(path.exists() or path.is_symlink() for path in paths):
            return dict(result, status='inputs_reappeared')
        if collector.cancelled():
            return dict(result, status='cancelled')
        completed = {key: value for key, value in record.items() if key != 'files'}
        completed.update(completed=True, files=[], retired_files=len(record['files']),
                         retired_bytes=sum(entry['bytes'] for entry in record['files']),
                         retired_allocated_bytes=sum(entry['allocated_bytes'] for entry in record['files']))
        atomic(receipt, canonical_json(completed))
    return dict(result, status='retired' if result['removed'] else 'unchanged')
