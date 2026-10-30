"""Release terminal clones' obsolete loose-object links using native Git.

An immutable canonical source-commit ref protects every local object before an
alternate is published. Git's prune-packed then removes only packed duplicates;
private metadata, refs, reflogs, index and worktree remain intact. Obsolete
private packs are released with native local repacking after the same proof.
A crash after publishing the alternate is harmless and pruning is idempotent.
"""
from functools import lru_cache
import json
import os
from pathlib import Path
import re
import stat
import subprocess

from . import retained_files as retained, terminal_retention as terminal
from .candidate import canonical_json, _sync_dir
from .publication import atomic

FORMAT = 'trellis-terminal-git-alternate/1'
RECEIPT = 'git-objects-retired.json'
PIN_PREFIX = 'refs/trellis/dispatch-bases/'
OID = re.compile(r'(?:[0-9a-f]{40}|[0-9a-f]{64})\Z')


class NotRecoverable(ValueError):
    pass


def _env():
    # Local housekeeping must not inherit another Git worktree/object-store
    # override from the service launcher. No network operations are performed.
    return {**{key:value for key,value in os.environ.items() if not key.startswith('GIT_')}, 'LC_ALL':'C'}


def _git(repo, *args, data=None):
    return subprocess.check_output(['git', '--no-replace-objects', '-c',
        'core.hooksPath=/dev/null', '-C', str(repo), *args], input=data,
        stderr=subprocess.PIPE, env=_env())


def roots(directory):
    from .storage import _attempt_directories
    for attempt in _attempt_directories(directory):
        for repo in (attempt / 'repo', attempt / 'repo/.trellis/supervisor/repo'):
            if (repo / '.git').exists() or (repo / '.git').is_symlink():
                yield attempt, repo


def _directories(path):
    for parent in (path, *path.parents):
        if not stat.S_ISDIR(parent.lstat().st_mode):
            raise ValueError('Git retention cannot traverse aliases or non-directories')


def _inventory(gitdir):
    """Exact regular Git files; content inode links are allowed, aliases are not."""
    _directories(gitdir)
    files, directories, stack = {}, {}, [gitdir]
    while stack:
        parent = stack.pop(); info = parent.lstat()
        directories[str(parent.relative_to(gitdir))] = [info.st_dev, info.st_ino, info.st_mode]
        for path in parent.iterdir():
            info = path.lstat(); name = str(path.relative_to(gitdir))
            if stat.S_ISDIR(info.st_mode):
                stack.append(path)
            elif stat.S_ISREG(info.st_mode):
                if path.name.endswith('.lock') or name == 'gc.pid':
                    raise NotRecoverable('Git writer lock is present')
                # Link-count changes in sibling clones change ctime without
                # changing immutable object contents. Metadata remains strict.
                content = (name.startswith('objects/pack/') or
                    (len(Path(name).parts) == 3 and Path(name).parts[0] == 'objects'
                     and OID.fullmatch(''.join(Path(name).parts[1:]))))
                files[name] = ([info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_mode]
                    if content else list(retained._signature(info)) + [info.st_mode])
            else:
                raise ValueError('Git retention encountered an alias or special file')
    return dict(files=files, directories=directories)


def _local_objects(gitdir, inventory):
    objects = set()
    for name in inventory['files']:
        parts = Path(name).parts
        if len(parts) == 3 and parts[0] == 'objects' and len(parts[1]) == 2:
            oid = parts[1] + parts[2]
            if OID.fullmatch(oid): objects.add(oid)
            else: raise NotRecoverable('unknown loose-object entry')
        elif name.startswith('objects/pack/') and name.endswith('.idx'):
            with (gitdir / name).open('rb') as stream:
                output = subprocess.check_output(['git', 'show-index'], stdin=stream, stderr=subprocess.PIPE, env=_env())
            for line in output.decode('ascii').splitlines():
                oid = line.split()[1]
                if not OID.fullmatch(oid): raise ValueError('invalid packed object identity')
                objects.add(oid)
        elif name.startswith('objects/pack/') and name.endswith('.pack'):
            if str(Path(name).with_suffix('.idx')) not in inventory['files']:
                raise NotRecoverable('Git pack has no index')
    return objects


@lru_cache(maxsize=8)
def _closure(repo, head):
    return frozenset(_git(Path(repo), 'rev-list', '--objects', '--no-object-names', head).decode().splitlines())


def _metadata_oids(repo):
    values = set(_git(repo, 'for-each-ref', '--format=%(objectname)').decode().splitlines())
    values.add(_git(repo, 'rev-parse', '--verify', 'HEAD').decode().strip())
    for row in _git(repo, 'ls-files', '--stage', '-z').split(b'\0'):
        if row: values.add(row.split(b'\t', 1)[0].split()[1].decode())
    # Include BOTH sides of each reflog update, including deleted refs' logs.
    logs = repo / '.git/logs'
    if logs.exists():
        for path in logs.rglob('*'):
            if path.is_file():
                with path.open('rb') as stream:
                    for line in stream:
                        fields = line.split(b' ', 2)
                        if len(fields) < 3: raise NotRecoverable('malformed Git reflog')
                        values.update(x.decode() for x in fields[:2] if set(x) != {48})
    if any(not OID.fullmatch(value) for value in values):
        raise NotRecoverable('invalid Git metadata object')
    return values


def _layout(gitdir):
    for name in ('shallow', 'commondir', 'info/grafts'):
        path = gitdir / name
        if path.exists() or path.is_symlink():
            raise NotRecoverable('shallow, grafted or indirect Git stores are unsupported')


def _canonical_objects(repo):
    gitdir = repo / '.git'; _directories(gitdir); _directories(gitdir / 'objects')
    _layout(gitdir)
    for name in ('pack', 'info'):
        path = gitdir / 'objects' / name
        if path.exists() or path.is_symlink(): _directories(path)
    if _git(repo, 'rev-parse', '--absolute-git-dir').decode().strip() != str(gitdir):
        raise NotRecoverable('canonical Git indirection is unsupported')
    alternate = gitdir / 'objects/info/alternates'
    if alternate.exists() or alternate.is_symlink():
        raise NotRecoverable('canonical alternate chain is unsupported')
    return gitdir / 'objects'


def _pin(repo, head):
    name = PIN_PREFIX + head
    symbolic = subprocess.run(['git', '-C', str(repo), 'symbolic-ref', '-q', name], capture_output=True, env=_env())
    if symbolic.returncode == 0:
        raise ValueError('immutable Git recovery pin is symbolic')
    result = subprocess.run(['git', '-C', str(repo), 'rev-parse', '--verify', '--quiet', name], capture_output=True, env=_env())
    if result.returncode == 0:
        if result.stdout.decode().strip() != head: raise ValueError('immutable Git recovery pin conflicts')
    elif result.returncode == 1:
        _git(repo, 'update-ref', '--no-deref', name, head, '0' * len(head))
    else:
        raise ValueError('cannot inspect Git recovery pin')
    # update-ref uses native atomic locking; make the new ref durable before
    # introducing a dependency on it. Refs may already have been packed.
    path = repo / '.git' / name
    if path.exists():
        with retained.regular_path(path).open('rb') as stream:
            import os
            os.fsync(stream.fileno())
        for parent in path.parents:
            _sync_dir(parent)
            if parent == repo / '.git': break
    else:
        with retained.regular_path(repo / '.git/packed-refs').open('rb') as stream:
            import os
            os.fsync(stream.fileno())
        _sync_dir(repo / '.git')
    return name


def _pack_generation(canonical):
    result = []
    for pack in sorted((canonical / 'pack').glob('*.pack')):
        pair = []
        for path in (pack, pack.with_suffix('.idx')):
            try: info = path.lstat()
            except FileNotFoundError: return None  # A native pack publication is in flight.
            if not stat.S_ISREG(info.st_mode):
                raise ValueError('canonical Git pack is an alias')
            pair.append([path.name, info.st_dev, info.st_ino, info.st_size])
        result.append(pair)
    return result


def _has_loose(repo):
    objects = repo / '.git/objects'
    _directories(objects)
    for prefix in objects.iterdir():
        if len(prefix.name) == 2 and all(c in '0123456789abcdef' for c in prefix.name):
            _directories(prefix)
            if next(prefix.iterdir(), None) is not None:
                return True
    return False


def _finished_generation(receipt, generation, head, canonical, owner, identity):
    if not receipt.exists() and not receipt.is_symlink(): return False
    with retained.regular_path(receipt).open('rb') as stream:
        raw = stream.read(65537)
    if len(raw) > 65536: raise ValueError('oversized Git retirement receipt')
    record = json.loads(raw)
    if (not isinstance(record, dict) or record.get('format') != FORMAT
            or record.get('owner') != owner or record.get('identity') != identity
            or record.get('source_head') != head or record.get('alternate') != str(canonical)):
        raise ValueError('Git retirement receipt changed its binding')
    return record.get('finished_pack_generation') == generation


def _prune(repo):
    _git(repo, 'prune-packed')


def _pack_files(objects):
    directory = objects / 'pack'
    if not directory.exists() and not directory.is_symlink(): return {}
    _directories(directory)
    result = {}
    for path in directory.iterdir():
        if path.suffix in ('.pack', '.idx', '.rev', '.bitmap', '.keep', '.promisor'):
            info = path.lstat()
            if not stat.S_ISREG(info.st_mode):
                raise ValueError('Git pack retirement encountered an alias')
            result[path.name] = info
    return result


def _obsolete_packs(repo, canonical_inodes):
    return {name for name, info in _pack_files(repo / '.git/objects').items()
            if name.endswith('.pack') and (info.st_dev, info.st_ino) not in canonical_inodes}


def _repack_local(repo):
    # These private repositories are not dumb-HTTP servers. Avoid updating
    # hardlinked info/packs and perturbing sibling clones' metadata snapshots.
    _git(repo, '-c', 'pack.threads=1', '-c', 'pack.windowMemory=256m',
         '-c', 'repack.writeBitmaps=false', '-c', 'pack.writeReverseIndex=false',
         'repack', '-a', '-d', '--local', '-n')


def _verify_objects(repo, objects):
    ordered = sorted(objects)
    rows = _git(repo, 'cat-file', '--batch-check=%(objectname) %(objecttype)',
        data=('\n'.join(ordered) + '\n').encode()).decode().splitlines()
    if len(rows) != len(ordered) or any(row.split()[0] != oid or row.endswith(' missing')
            for oid, row in zip(ordered, rows)):
        raise ValueError('protected Git object is unavailable after private repack')


def collect(collector, directory, observed, *, apply=True):
    result = dict(status='absent', stores=0, retained_stores=0, removed_links=0,
                  removed_bytes=0, removed_allocated_bytes=0, unlinked_allocated_bytes=0,
                  pack_stores=0, removed_pack_files=0, new_pack_allocated_bytes=0)
    if collector.cancelled(): return dict(result, status='cancelled')
    _, directory = retained.owner_directory(collector.root, directory)
    owner = retained.owner_identity(directory)
    if directory.name not in observed or collector._owner_pending(owner):
        return dict(result, status='not_terminal_or_pending')
    selected = [(attempt, repo) for attempt, repo in roots(directory)
                if _has_loose(repo) or _pack_files(repo / '.git/objects')]
    if not selected: return result
    canonical = _canonical_objects(collector.repo)
    canonical_identity = (canonical.stat().st_dev, canonical.stat().st_ino)
    generation = _pack_generation(canonical)
    if generation is None: return dict(result, status='canonical_pack_pending')
    canonical_inodes = {(info.st_dev, info.st_ino) for name, info in _pack_files(canonical).items() if name.endswith('.pack')}
    selected = [(attempt, repo, _obsolete_packs(repo, canonical_inodes)) for attempt, repo in selected]
    selected = [(attempt, repo, obsolete) for attempt, repo, obsolete in selected if obsolete or _has_loose(repo)]
    if not selected: return result
    guard = retained.terminal_owner_guard(collector.root, directory, observed)
    identity = observed[directory.name]
    if isinstance(identity, terminal._DeferredVerifierIdentity): identity = identity.resolve()
    witness = collector.witness()
    if witness.get('owners', witness['tasks']).get(directory.name) != identity:
        return dict(result, status='not_committed')
    for attempt, repo, obsolete in selected:
        if collector.cancelled(): return dict(result, status='cancelled')
        try:
            prepared_path = retained.regular_path(attempt / 'prepared.json')
            prepared_signature = retained._signature(prepared_path.stat())
            prepared = terminal._json(prepared_path); head = prepared.get('source_head')
            if Path(prepared['source_repo']).resolve() != collector.repo or not isinstance(head, str) or not OID.fullmatch(head):
                raise ValueError('Git preparation does not identify the canonical source')
            if owner['kind'] == 'review' and prepared['request_id'] != owner['id']:
                raise ValueError('Git preparation has the wrong reviewer')
            _git(collector.repo, 'merge-base', '--is-ancestor', head, witness['commit'])
            gitdir = repo / '.git'; _layout(gitdir); receipt = gitdir / RECEIPT
            finished = _finished_generation(receipt, generation, head, canonical, owner, identity)
            inventory = _inventory(gitdir)
            if _git(repo, 'rev-parse', '--absolute-git-dir').decode().strip() != str(gitdir):
                raise NotRecoverable('private Git indirection is unsupported')
            alternate = gitdir / 'objects/info/alternates'; wanted = (str(canonical) + '\n').encode()
            if alternate.exists() and retained.regular_path(alternate).read_bytes() != wanted:
                raise NotRecoverable('private Git has a foreign alternate')
            if finished and alternate.exists() and not obsolete:
                current_pin = subprocess.run(['git', '-C', str(collector.repo), 'rev-parse',
                    '--verify', '--quiet', PIN_PREFIX + head], capture_output=True, env=_env())
                symbolic_pin = subprocess.run(['git', '-C', str(collector.repo), 'symbolic-ref',
                    '-q', PIN_PREFIX + head], capture_output=True, env=_env())
                if current_pin.returncode == 0 and current_pin.stdout.decode().strip() == head and symbolic_pin.returncode != 0:
                    continue
            closure = _closure(str(collector.repo), head)
            local_objects = _local_objects(gitdir, inventory) | _metadata_oids(repo)
            if not local_objects <= closure:
                raise NotRecoverable('private Git objects are outside protected source history')
            loose = {}
            for name in inventory['files']:
                parts = Path(name).parts
                if len(parts) == 3 and parts[0] == 'objects' and OID.fullmatch(parts[1] + parts[2]):
                    info = (gitdir / name).stat()
                    loose[name] = dict(bytes=info.st_size, allocated=info.st_blocks * 512)
            packs = {name: info for name, info in _pack_files(gitdir / 'objects').items()} if obsolete else {}
            if any(str(Path(name).with_suffix('.keep')) in packs for name in obsolete):
                raise NotRecoverable('obsolete private Git pack is explicitly kept')
            if not loose and not obsolete: continue
            if not apply:
                result['stores'] += 1; continue
            # Git excludes its administrative directory from the worktree
            # index. Check from the worktree, not from inside .git.
            if _git(repo, 'ls-files', '-z', '--', '.git/' + RECEIPT):
                raise ValueError('Git retirement receipt is tracked')
            with collector.retirement_lock() as acquired:
                if not acquired: return dict(result, status='cancelled' if collector.cancelled() else 'coordinator_busy')
                if collector.cancelled(): return dict(result, status='cancelled')
                if not collector.checkpoint_current(witness): return dict(result, status='checkpoint_changed')
                if collector._owner_pending(owner): return dict(result, status='publication_pending')
                guard()
                if retained._signature(retained.regular_path(prepared_path).stat()) != prepared_signature:
                    raise ValueError('Git preparation changed before retirement')
                if _inventory(gitdir) != inventory:
                    raise ValueError('private Git store changed before retirement')
                _canonical_objects(collector.repo)
                _layout(gitdir)
                if (canonical.stat().st_dev, canonical.stat().st_ino) != canonical_identity:
                    raise ValueError('canonical Git object store changed identity')
                pin = _pin(collector.repo, head)
                if not alternate.exists():
                    alternate.parent.mkdir(exist_ok=True)
                    atomic(alternate, wanted)
                    # Persist a newly created info/ directory before any
                    # object becomes dependent on the alternate it contains.
                    _sync_dir(alternate.parent.parent)
                # Record before prune; an interrupted operation safely retries.
                record = dict(format=FORMAT, owner=owner, identity=identity,
                    canonical_repo=str(collector.repo), source_head=head, recovery_pin=pin,
                    checkpoint_commit=witness['commit'], alternate=str(canonical))
                atomic(receipt, canonical_json(record))
                names = set(loose) | {'objects/pack/' + name for name in packs}
                before = {name:(gitdir / name).stat() for name in names}
                if collector.cancelled(): return dict(result, status='cancelled')
                _prune(repo)
                if obsolete:
                    _repack_local(repo)
                    _verify_objects(repo, local_objects)
                    result['pack_stores'] += 1
                    record['pack_retirement'] = 1
                    # Normally all objects come from the protected alternate,
                    # so no new pack is written. Account conservatively if Git
                    # does create a representation under another configuration.
                    # Keep allocation separate from gross reclaimed counters,
                    # whose monotonicity is required by service telemetry.
                    for name, info in _pack_files(gitdir / 'objects').items():
                        old = packs.get(name)
                        if old is None or (old.st_dev, old.st_ino) != (info.st_dev, info.st_ino):
                            result['new_pack_allocated_bytes'] += info.st_blocks * 512
                changed_parents = set()
                for name, info in before.items():
                    if not (gitdir / name).exists():
                        changed_parents.add((gitdir / name).parent)
                        if name.startswith('objects/pack/'):
                            result['removed_pack_files'] += 1
                        else:
                            result['removed_links'] += 1
                        result['unlinked_allocated_bytes'] += info.st_blocks * 512
                        # A last-link sample is a conservative physical estimate;
                        # shared links are NOT counted as reclaimed disk bytes.
                        if info.st_nlink == 1:
                            result['removed_bytes'] += info.st_size
                            result['removed_allocated_bytes'] += info.st_blocks * 512
                for parent in sorted(changed_parents):
                    if parent.exists():
                        _directories(parent)
                        _sync_dir(parent)
                _sync_dir(gitdir / 'objects')
                # Revisit when canonical later packs its currently loose data.
                # A completion receipt never permanently hides remaining links.
                record['finished_pack_generation'] = generation
                atomic(receipt, canonical_json(record))
            result['stores'] += 1
        except NotRecoverable:
            result['retained_stores'] += 1
    return dict(result, status='eligible' if not apply else 'processed')
