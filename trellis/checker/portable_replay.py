"""Content-bound, optional portability for existing host replay attestations.

This is not a new proof authority. Only a completed host acceptance is a donor;
the existing source/bundle/provenance checks and ordinary probe keys still run.
Package artifacts are content-bound, including transitive imports. They must
also retain the ordinary trusted-library confinement boundary: hashing cannot
make an attacker-writable external library trustworthy.
"""
from __future__ import annotations

from collections import OrderedDict
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import time
import threading

from . import telemetry
from .admission import capture_cancellation

VERSION = 1
MAX_MODULES = 32768
MAX_MANIFEST_BYTES = 32 << 20
MAX_FILE_BINDINGS = 131072
MAX_HASH_BYTES = 32 << 30
CAPTURE_SECONDS = 180
MAX_CONTENT_READS = 3
DIRECTORY = '.trellis/kernel-replay-environments'


class Unavailable(ValueError):
    def __init__(self, reason, *, detail=None):
        super().__init__(reason)
        self.detail = detail


class CaptureUnavailable(Unavailable):
    """Capture could not establish a complete input binding."""


def _json(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()


def digest(value):
    return hashlib.sha256(_json(value)).hexdigest()


def acceptance_workspace(repo):
    """Limit initial capture overhead to isolated host acceptance workspaces."""
    repo = Path(repo).absolute()
    if len(repo.parents) <= 5 or repo.parts[-4:] != ('repo', '.trellis', 'supervisor', 'repo'):
        return False
    directory = repo.parents[3]
    if directory.parent.name == 'finalization' and directory.name.isdigit():
        directory = directory.parent.parent
    return directory.name.isdigit() and directory.parent.name == 'acceptance'


_UNOBSERVED = object()
_FILE_FIELDS = ('st_dev', 'st_ino', 'st_mode', 'st_size', 'st_mtime_ns', 'st_ctime_ns', 'st_nlink')


def _file_state(info):
    return tuple(getattr(info, name) for name in _FILE_FIELDS)


def _metadata_only(before, after):
    # Link creation/removal changes ctime, but does not establish byte identity.
    # Every permitted difference still requires a matching stable content read.
    return (before is not None and after is not None and len(before) == len(after) == 7
            and stat.S_ISREG(before[2]) and before[:5] == after[:5])


def _change_detail(path, before, after):
    if before is None or after is None:
        return dict(path=str(path)[-320:], changed_fields=['presence'])
    return dict(path=str(path)[-320:], changed_fields=[name for index, name in enumerate(
        _FILE_FIELDS[:max(len(before), len(after))])
        if index >= len(before) or index >= len(after) or before[index] != after[index]])


class _ProtectedDigestCache:
    """Bounded, process-local digests of host-confined package artifacts.

    Keys include ctime and link count: even harmless link churn is a miss.
    Stripes coalesce concurrent reads without retaining unbounded pending keys.
    No path/absence proof, mutable project input, or explicit revalidation is
    cached here; those continue through the ordinary capture guards.
    """
    def __init__(self, capacity=32768, stripes=64):
        self.capacity = capacity
        self.values = OrderedDict()
        self.lock = threading.Lock()
        self.stripes = tuple(threading.Lock() for _ in range(stripes))

    def get(self, key):
        with self.lock:
            value = self.values.get(key)
            if value is not None:
                self.values.move_to_end(key)
                return dict(value)

    def put(self, key, value):
        with self.lock:
            self.values[key] = dict(value)
            self.values.move_to_end(key)
            while len(self.values) > self.capacity:
                self.values.popitem(last=False)


_PROTECTED_DIGESTS = _ProtectedDigestCache()


class HashBudget:
    def __init__(self, *, max_bytes=MAX_HASH_BYTES, seconds=CAPTURE_SECONDS):
        self.remaining = max_bytes
        self.deadline = time.monotonic() + seconds
        self.bytes = 0
        self.files = 0
        self.memo = {}
        self.content_revalidations = 0
        self.cached_bytes = self.cached_files = 0
        self.revalidation_reads = {}  # Only files with metadata churn, for this operation.

    def file(self, path, *, expected=None, expected_state=None, initial_state=_UNOBSERVED,
             protected_package=False):
        from .admission import cooperative_stage, check_cancelled
        from .memory_retry import MemoryStopped
        def ready():
            check_cancelled()
            if time.monotonic() > self.deadline:
                raise CaptureUnavailable('portable replay hash admission deadline exhausted')
        try:
            # Acquire before any digest stripe lock. No compiler RPC occurs
            # in this stage, and the 1 MiB loop polls cooperatively below.
            with cooperative_stage('artifact_hash', cancelled=ready):
                return self._bound_file(path, expected=expected, expected_state=expected_state,
                    initial_state=initial_state, protected_package=protected_package)
        except MemoryStopped as failure:
            raise CaptureUnavailable('portable replay hash deferred: ' + failure.reason) from None

    def _bound_file(self, path, *, expected=None, expected_state=None, initial_state=_UNOBSERVED,
                    protected_package=False):
        from .admission import check_cancelled
        path = Path(path)
        # Only the caller's host-selected read-only package custody admits a
        # cross-capture lookup. Expected-byte confirmation always reads again.
        if not protected_package or expected is not None or path in self.memo:
            return self._file(path, expected=expected, expected_state=expected_state,
                              initial_state=initial_state, protected_package=protected_package)
        before = _file_state(path.lstat())
        if (not stat.S_ISREG(before[2]) or expected_state is not None
                or (initial_state is not _UNOBSERVED and before != initial_state)):
            return self._file(path, expected=expected, expected_state=expected_state,
                              initial_state=initial_state, protected_package=protected_package)
        cache = _PROTECTED_DIGESTS
        stripe = cache.stripes[hash(before) % len(cache.stripes)]
        def ready():
            check_cancelled()
            if time.monotonic() > self.deadline:
                raise CaptureUnavailable('portable replay hash deadline exhausted')
        ready()
        while not stripe.acquire(timeout=0.05):
            ready()
        try:
            ready()
            current = _file_state(path.lstat())
            if current != before:
                if not _metadata_only(before, current):
                    raise CaptureUnavailable('protected package changed while waiting for content binding')
                # Another workspace may have linked this read-only package
                # while we queued. Never reuse a digest across that stat
                # change: use the ordinary bounded content confirmation. A
                # prior cached binding, when present, must match exactly;
                # otherwise the protected cold path requires two stable reads.
                return self._file(path, expected=cache.get(before),
                    expected_state=before, initial_state=before, protected_package=True)
            value = cache.get(before)
            if value is not None:
                if before[3] > self.remaining:
                    raise CaptureUnavailable('portable replay file/byte limit')
                # A hit still binds this exact path at this observation point.
                # Capture's final path/absence checks remain independently live.
                current = _file_state(path.lstat())
                if current != before:
                    if not _metadata_only(before, current):
                        raise CaptureUnavailable('protected package changed during cached content binding')
                    return self._file(path, expected=value, expected_state=before,
                        initial_state=before, protected_package=True)
                self.remaining -= before[3]
                self.cached_bytes += before[3]
                self.cached_files += 1
                self.memo[path] = (before, dict(value), False)
                return value
            value = self._file(path, initial_state=initial_state, protected_package=True)
            if self.memo.get(path) == (before, value, False):
                cache.put(before, value)
            return value
        finally:
            stripe.release()

    def _file(self, path, *, expected=None, expected_state=None, initial_state=_UNOBSERVED,
              protected_package=False):
        from .admission import cooperative_poll as check_cancelled
        path = Path(path)
        anchor = None
        confirm_initial = False
        for attempt in range(MAX_CONTENT_READS):
            before = _file_state(path.lstat())
            check_cancelled()
            if time.monotonic() > self.deadline:
                raise CaptureUnavailable('portable replay hash deadline exhausted')
            if not stat.S_ISREG(before[2]):
                raise Unavailable('portable replay input is not a regular file')
            if initial_state is not _UNOBSERVED and before != initial_state:
                if protected_package and _metadata_only(initial_state, before):
                    confirm_initial = True
                else:
                    # Mutable project artifacts keep the strict first-read
                    # guard; link metadata cannot establish old byte identity.
                    raise CaptureUnavailable('import resolution changed before initial content binding',
                                      detail=_change_detail(path, initial_state, before))
            if expected_state is not None and not _metadata_only(expected_state, before):
                raise Unavailable('import artifact changed during content capture',
                                  detail=_change_detail(path, expected_state, before))
            if anchor is not None and not _metadata_only(anchor, before):
                raise Unavailable('import artifact changed during content capture',
                                  detail=_change_detail(path, anchor, before))
            # This memo remains operation-local, separate from the guarded
            # protected-package cache. Explicit revalidation always reads again
            # and compares the original manifest's byte binding.
            if expected is None and not confirm_initial and path in self.memo and self.memo[path][0] == before:
                return self.memo[path][1]
            if expected is not None:
                reads = self.revalidation_reads.get(path, 0)
                if reads >= MAX_CONTENT_READS - 1:
                    raise CaptureUnavailable('import metadata did not settle during content revalidation',
                                             detail=dict(path=str(path)[-320:]))
            if before[3] > self.remaining:
                raise CaptureUnavailable('portable replay file/byte limit')
            if expected is not None:
                self.revalidation_reads[path] = reads + 1
                self.content_revalidations += 1
            read_ticket = telemetry.capture_read_begin(self, revalidation=expected is not None)
            read_complete = False
            try:
                h = hashlib.sha256()
                size = 0
                with path.open('rb') as handle:
                    for chunk in iter(lambda: handle.read(1 << 20), b''):
                        check_cancelled()
                        if len(chunk) > self.remaining or time.monotonic() > self.deadline:
                            raise CaptureUnavailable('portable replay hash budget exhausted')
                        self.remaining -= len(chunk)
                        self.bytes += len(chunk)
                        size += len(chunk)
                        h.update(chunk)
                read_complete = True
            finally:
                telemetry.capture_read_end(read_ticket, self, completed=read_complete)
            after = _file_state(path.lstat())
            self.files += 1
            value = dict(sha256=h.hexdigest(), size_bytes=size)
            if size != before[3] or (expected is not None and value != expected):
                raise Unavailable('import artifact bytes changed during content capture',
                                  detail=dict(path=str(path)[-320:]))
            if before == after:
                if confirm_initial and expected is None:
                    expected, anchor = value, before
                    continue
                self.memo[path] = (after, value, expected is not None)
                return value
            if not _metadata_only(before, after) or (expected is None and not protected_package):
                raise Unavailable('import artifact changed during content capture',
                                  detail=_change_detail(path, before, after))
            if expected is None:
                # This is a fresh binding, not historical byte equality.
                # Only the host-selected, sandbox-RO package tree may use it:
                # require a second equal digest from a stable read. Trusted
                # installation can overlap hashing; observation before/after
                # equality remains mandatory at the consumer.
                expected = value
            # A hardlink operation is only a hypothesis. Confirm the same bytes
            # in a stable read; concurrent writes cannot hide behind ctime churn.
            anchor = before
        raise CaptureUnavailable('import metadata did not settle during content revalidation',
                                 detail=_change_detail(path, before, after))


def _resolved(path, repo):
    path = Path(path)
    if not path.is_absolute():
        path = repo / path
    # Native fact reuse later rechecks resolved regular paths. A mutable alias
    # could otherwise redirect lookup without changing any of those targets.
    for ancestor in (path, *path.parents):
        if ancestor.is_symlink():
            raise Unavailable('import alias is not portable')
    return path.resolve()


def _identity_path(path, repo, sysroot):
    path = _resolved(path, repo)
    if path.is_relative_to(sysroot):
        return 'sysroot/' + path.relative_to(sysroot).as_posix()
    if path.is_relative_to(repo):
        return 'repo/' + path.relative_to(repo).as_posix()
    # External path dependencies keep their exact host-controlled location.
    return 'external/' + str(path)


def _relative_parts(path, root):
    """Path.relative_to's lexical prefix on the POSIX paths used by capture.

    Avoid constructing every parent Path in Python 3.12. This does not resolve
    aliases or provide filesystem evidence; the capture's path guards do that.
    """
    parts, prefix = path.parts, root.parts
    return (parts[len(prefix):] if path.anchor == root.anchor
            and parts[:len(prefix)] == prefix else None)


class _CapturePaths:
    """One capture's alias checks, revalidated before returning its evidence.

    Large import inventories repeat the same paths and ancestors many times.
    Path evidence never survives a capture: each capture starts from fresh
    stats and mutable-input hashes. Host-protected package digests may be
    reused only at unchanged full file identity. Missing candidates are
    checked again as well.
    """
    def __init__(self, repo):
        self.repo = repo
        self.states = {}
        self.resolved = {}
        self.bindings = {}  # References to this capture's immutable file commitments.
        self.initial_states = {}

    @staticmethod
    def state(path):
        try:
            value = path.lstat()
        except FileNotFoundError:
            return None
        if stat.S_ISLNK(value.st_mode):
            raise Unavailable('import alias is not portable')
        if stat.S_ISDIR(value.st_mode):
            # Unrelated sibling creation changes directory timestamps, not
            # this path's resolution. Replacing the directory changes inode.
            return (value.st_dev, value.st_ino, value.st_mode)
        return _file_state(value)

    def resolve(self, raw):
        path = Path(raw)
        if not path.is_absolute():
            path = self.repo / path
        if path in self.resolved:
            return self.resolved[path]
        for ancestor in (path, *path.parents):
            if ancestor not in self.states:
                if len(self.states) >= MAX_FILE_BINDINGS * 2:
                    raise CaptureUnavailable('import path validation limit')
                self.states[ancestor] = self.state(ancestor)
        # Every lexical ancestor was checked for aliases above. Normalize '..'
        # without repeating filesystem traversal in Path.resolve().
        result = Path(os.path.abspath(path))
        self.resolved[path] = result
        self.initial_states.setdefault(result, self.states[path])
        return result

    def identity(self, path, sysroot):
        path = self.resolve(path)
        relative = _relative_parts(path, sysroot)
        if relative is not None:
            return 'sysroot/' + ('/'.join(relative) or '.')
        relative = _relative_parts(path, self.repo)
        if relative is not None:
            return 'repo/' + ('/'.join(relative) or '.')
        return 'external/' + str(path)

    def validate(self, budget):
        from .admission import check_cancelled
        for path, before in self.states.items():
            check_cancelled()
            if time.monotonic() > budget.deadline:
                raise CaptureUnavailable('portable replay path validation deadline exhausted')
            after = self.state(path)
            if after == before:
                continue
            resolved = self.resolved.get(path, path)
            expected = self.bindings.get(resolved)
            if _metadata_only(before, after) and expected is not None:
                verified = budget.memo.get(resolved)
                if verified != (after, expected, True):
                    budget.file(resolved, expected=expected, expected_state=before)
                continue
            raise Unavailable('import resolution changed during content capture',
                              detail=_change_detail(path, before, after))


@telemetry.capture_cost
@capture_cancellation()
def capture(repo, nodes, *, timeout_secs, bwrap_role, budget=None, telemetry_origin='other'):
    """Resolve the actual import graph once; hash each mutable part once.

    Optional failure never changes a successful mathematical check to Invalid.
    The caller records a cold-portability reason and retains ordinary replay.
    """
    from trellis.atomic_actions import observations as obs
    start = time.monotonic()
    stats = dict(status='unavailable', bytes_hashed=0, files_hashed=0)
    hash_start = None
    hash_ticket = None
    telemetry_reason = None
    before_bytes = before_files = before_revalidations = None
    before_cached_bytes = before_cached_files = 0
    try:
        repo = Path(repo).resolve()
        paths = _CapturePaths(repo)
        roots = sorted(set(nodes))
        if not roots or len(roots) > MAX_MODULES:
            raise Unavailable('empty/oversized replay roots')
        script = Path(obs.__file__).resolve().parents[2] / 'scripts/lean_module_manifest.lean'
        hash_seconds = min(timeout_secs, CAPTURE_SECONDS)
        if budget is not None:
            hash_seconds = min(hash_seconds, budget.deadline - time.monotonic())
        if hash_seconds <= 0:
            raise CaptureUnavailable('portable replay operation deadline exhausted')
        inventory_start = time.monotonic()
        inventory_ticket = telemetry.capture_phase_begin('inventory')
        inventory_complete = False
        try:
            from .admission import compiler_operation
            with compiler_operation("manifest"):
                result = obs._run_lake_command(repo,
                    ['env', 'lean', '--run', str(script), '--replay-environment', *roots],
                    timeout_secs=min(timeout_secs, CAPTURE_SECONDS), bwrap_role=bwrap_role)
            inventory_complete = True
        finally:
            telemetry.capture_phase_end(inventory_ticket, completed=inventory_complete)
            stats['inventory_wall_secs_including_queue'] = time.monotonic() - inventory_start
        # Compiler admission can legitimately outlast the active-work budget.
        # Hashing starts after the subprocess AND its lease have ended. Keep
        # independent bounded process/hash work, never a lease while hashing.
        hash_start = time.monotonic()
        hash_ticket = telemetry.capture_phase_begin('hash_phase')
        if budget is None:
            budget = HashBudget(seconds=hash_seconds)
        else:
            budget.deadline = hash_start + hash_seconds
        before_bytes, before_files = budget.bytes, budget.files
        before_revalidations = budget.content_revalidations
        before_cached_bytes, before_cached_files = budget.cached_bytes, budget.cached_files
        raw = result.get('stdout', '')
        if (result.get('returncode') != 0 or result.get('timed_out') or result.get('spawn_error')
                or len(raw.encode()) > MAX_MANIFEST_BYTES):
            raise CaptureUnavailable('import inventory unavailable or oversized', detail=dict(
                returncode=result.get('returncode'), timed_out=bool(result.get('timed_out')),
                spawn_error=str(result.get('spawn_error') or '')[:512],
                stdout_bytes=len(raw.encode()), stdout_limit_bytes=MAX_MANIFEST_BYTES,
                stderr_tail=str(result.get('stderr') or '')[-2048:]))
        rows = [json.loads(line) for line in raw.splitlines() if line.strip()]
        if not rows or len(rows) > MAX_MODULES + 1 or rows[0].get('environment_header') is not True:
            raise Unavailable('invalid import inventory header')
        sysroot = Path(rows[0]['sysroot']).resolve(strict=True)
        if sysroot.is_relative_to(repo):
            raise Unavailable('toolchain sysroot is workspace-writable')
        files = {}
        bound_labels = []
        def bind(path, *, record_label=True):
            path = paths.resolve(path)
            label = paths.identity(path, sysroot)
            if record_label:
                bound_labels.append(label)
            if label not in files:
                if len(files) >= MAX_FILE_BINDINGS:
                    raise CaptureUnavailable('import resolution binding limit')
                # Host-selected compiler confinement overlays this exact tree
                # read-only, including shared aliases. Neither arbitrary managed
                # roots nor a worker-supplied request flag establish that custody.
                files[label] = budget.file(path, initial_state=paths.initial_states[path],
                    protected_package=(bwrap_role == 'lake_compiler'
                        and path.is_relative_to(repo / '.lake/packages'))) if path.exists() else None
                paths.bindings[path] = files[label]
            return files[label]
        tools = {}
        for name in ('lean', 'lake', 'leanchecker'):
            selected = shutil.which(name, path=rows[0]['path'])
            if not selected:
                raise Unavailable('unresolved replay executable')
            path = Path(selected).resolve(strict=True)
            if not path.is_relative_to(sysroot / 'bin'):
                raise Unavailable('non-sysroot replay executable')
            tools[name] = bind(path)
        bind(script)
        common_files = sorted(set(bound_labels))
        search_path = [Path(path) if Path(path).is_absolute() else repo / path
                       for path in rows[0]['search_path']]
        if not search_path or len(search_path) > 256:
            raise CaptureUnavailable('import search-path limit')
        modules = {}
        managed = obs._lake_managed_import_roots(repo)
        for row in rows[1:]:
            bound_labels.clear()
            name = row['module']
            if not isinstance(name, str) or name in modules:
                raise Unavailable('duplicate/invalid import module')
            selected = paths.resolve(row.get('sysroot_import') or row['parts'][0]['path'])
            found = False
            for root in search_path:
                lookup = root / (name.replace('.', '/') + '.olean')
                if paths.resolve(lookup) == selected:
                    found = True
                    break
                if bind(lookup) is not None:
                    raise Unavailable('import resolver differs from observed search path')
            if not found:
                raise Unavailable('resolved import missing from search path')
            if 'sysroot_import' in row:
                path = paths.resolve(row['sysroot_import'])
                if not path.is_relative_to(sysroot / 'lib/lean'):
                    raise Unavailable('false sysroot import')
                modules[name] = dict(sysroot_import=paths.identity(path, sysroot),
                                     file_bindings=sorted(set(bound_labels)))
                continue
            parts = row['parts']
            if [part['level'] for part in parts] not in (
                    ['exported'], ['exported', 'server'], ['exported', 'server', 'private']):
                raise Unavailable('unsupported import bundle')
            bound_parts = []
            for part in parts:
                path = paths.resolve(part['path'])
                if not any(path.is_relative_to(root) for root in managed):
                    raise Unavailable('import outside existing managed roots')
                bound_parts.append(dict(level=part['level'], path=paths.identity(path, sysroot),
                                        **bind(path)))
            for suffix in ('.server', '.private'):
                bind(Path(str(selected) + suffix))
            imports = []
            for entry in row['imports']:
                imported = paths.resolve(entry['olean'])
                if not imported.is_relative_to(sysroot / 'lib/lean'):
                    if not any(imported.is_relative_to(root) for root in managed):
                        raise Unavailable('import outside existing managed roots')
                    # Bind bytes at the first reference, not its later module
                    # row. Package mirrors can change link metadata meanwhile;
                    # final revalidation must compare against an ORIGINAL hash.
                    # Keep owner-specific manifest labels unchanged.
                    bind(imported, record_label=False)
                imports.append(dict(module=entry['module'],
                    path=paths.identity(entry['olean'], sysroot),
                    **{key: entry[key] for key in ('import_all', 'is_exported', 'is_meta')}))
            modules[name] = dict(parts=bound_parts, imports=sorted(imports, key=lambda x: _json(x)),
                                 file_bindings=sorted(set(bound_labels)))
        if any('Tablet.' + node not in modules for node in roots):
            raise Unavailable('missing replay root')
        if any(entry['module'] not in modules for row in modules.values() for entry in row.get('imports', [])):
            raise Unavailable('incomplete import graph')
        environment = dict(version=VERSION, roots=roots, sysroot=str(sysroot), tools=tools,
            toolchain_sha256=obs._kernel_replay_toolchain_sha256(repo),
            adapter_sha256=hashlib.sha256(script.read_bytes()).hexdigest(), modules=modules,
            search_path=[paths.identity(path, sysroot) for path in search_path], files=files,
            common_files=common_files)
        if len(_json(environment)) > MAX_MANIFEST_BYTES:
            raise CaptureUnavailable('bound import environment oversized')
        validation_ticket = telemetry.capture_phase_begin('final_validation')
        validation_complete = False
        try:
            paths.validate(budget)
            validation_complete = True
        finally:
            telemetry.capture_phase_end(validation_ticket, completed=validation_complete)
        stats.update(status='captured', bytes_hashed=budget.bytes - before_bytes, files_hashed=budget.files - before_files,
                     modules=len(modules), environment_sha256=digest(environment))
        return environment, stats
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as exc:
        stats['reason'] = str(exc)[:240]
        stats['failure_class'] = 'environment_capture' if isinstance(exc, CaptureUnavailable) else 'environment_binding'
        if isinstance(exc, Unavailable) and exc.detail is not None:
            stats['failure_detail'] = exc.detail
        telemetry_reason = str(exc)[:240] if isinstance(exc, Unavailable) else 'capture error'
        return None, stats
    finally:
        telemetry.capture_phase_end(hash_ticket, completed=stats['status'] == 'captured')
        if before_bytes is not None:
            stats.update(bytes_hashed=budget.bytes - before_bytes, files_hashed=budget.files - before_files,
                         content_revalidation_reads=budget.content_revalidations - before_revalidations,
                         cached_bytes=budget.cached_bytes - before_cached_bytes,
                         cached_files=budget.cached_files - before_cached_files)
        if hash_start is not None:
            stats['hash_wall_secs'] = time.monotonic() - hash_start
        # All Unavailable reasons are fixed strings. Raw OSError/JSON text
        # stays in the RPC result; telemetry selects bounded stat-change details.
        measured = dict(stats)
        if telemetry_reason is not None:
            measured['reason'] = telemetry_reason
        telemetry.portable_capture(measured)
        stats['wall_secs'] = time.monotonic() - start


def attach(repo, nodes, environment):
    """Mint portability only beside a just-completed ordinary replay."""
    from trellis.atomic_actions import observations as obs
    from trellis.dispatch.publication import atomic
    identity = digest(environment)
    atomic(Path(repo) / DIRECTORY / (identity + '.json'), _json(environment))
    for node in nodes:
        record = obs._read_olean_provenance(repo, node)
        if not obs.olean_has_current_kernel_replay(repo, node):
            continue
        record['kernel_replay']['portable_environment_sha256'] = identity
        obs._write_olean_provenance(repo, node, record)


class EnvironmentView:
    """Owner-specific transitive commitments from one shared batch inventory.

    Hash each graph vertex once. An unrelated changed batch sibling is not an
    assumption of this owner's replay. No artifact traversal/hashing repeats
    per owner, and no per-owner copy of the package inventory is persisted.
    """
    def __init__(self, environment):
        self.environment = environment
        self.modules = environment['modules']
        self.hashes = {}
        self.visiting = set()
        header = {k: v for k, v in environment.items() if k not in {'roots', 'modules', 'files'}}
        self.common = digest(dict(header=header,
            files={label: environment['files'][label] for label in environment['common_files']}))

    def adapter_comparison(self):
        """Comparison only: retain raw historical commitments and inventories."""
        if hasattr(self, '_adapter_comparison'):
            return self._adapter_comparison
        env = self.environment
        labels = [label for label in env['common_files']
                  if label.endswith('/scripts/lean_module_manifest.lean')]
        if len(labels) != 1:
            raise Unavailable('ambiguous host manifest adapter binding')
        label = labels[0]
        binding = env['files'].get(label)
        if (not label.startswith('external/') or not isinstance(binding, dict)
                or binding.get('sha256') != env['adapter_sha256']
                or any(label in row['file_bindings'] for row in self.modules.values())):
            raise Unavailable('invalid host manifest adapter binding')
        role = 'host/lean_module_manifest.lean'
        if role in env['files']:
            raise Unavailable('ambiguous host manifest adapter role')
        header = {k: v for k, v in env.items() if k not in {'roots', 'modules', 'files'}}
        header['common_files'] = sorted(role if key == label else key for key in env['common_files'])
        common = digest(dict(header=header, files={role if key == label else key: env['files'][key]
            for key in env['common_files']}))
        self._adapter_label = label
        self._adapter_comparison = common
        return common

    def same_owner(self, other, node):
        if self.owner_digest(node) == other.owner_digest(node):
            return True
        return (self.adapter_comparison() == other.adapter_comparison()
                and self._module_digest('Tablet.' + node) == other._module_digest('Tablet.' + node))

    def match_owned_observation(self, observed, nodes):
        """Recover an omitted inventory only against its saved full commitment."""
        self.adapter_comparison()  # Validate the uniquely bound external adapter.
        files = observed.get('artifact_files', {})
        historical = [path for path in files if path.endswith('/scripts/lean_module_manifest.lean')]
        if (observed.get('version') != 1 or len(historical) != 1
                or not Path(historical[0]).is_absolute()
                or files[historical[0]] != self.environment['adapter_sha256']):
            raise Unavailable('saved owner host adapter binding unavailable')
        old_label = 'external/' + historical[0]
        label = self._adapter_label
        if (old_label != label and old_label in self.environment['files']
                or any(old_label in row['file_bindings'] for row in self.modules.values())):
            raise Unavailable('saved host adapter overlaps imported artifacts')
        env = self.environment
        header = {k: v for k, v in env.items() if k not in {'roots', 'modules', 'files'}}
        header['common_files'] = sorted(old_label if key == label else key for key in env['common_files'])
        common = digest(dict(header=header, files={old_label if key == label else key: env['files'][key]
            for key in env['common_files']}))
        commitments = {}
        for node in nodes:
            value = digest(dict(common=common, owner='Tablet.' + node,
                                imports=self._module_digest('Tablet.' + node)))
            if observed.get('environment_sha256s', {}).get(node) != value:
                raise Unavailable('saved owner imported environment changed: ' + node)
            commitments[node] = value
        return commitments, dict(previous=old_label[:512], current=label[:512])

    def tablet_import_closure(self, nodes):
        pending, seen = ['Tablet.' + node for node in nodes], set()
        while pending:
            module = pending.pop()
            if module in seen:
                continue
            seen.add(module)
            pending.extend(entry['module'] for entry in self.modules[module].get('imports', []))
        return {module[7:] for module in seen if module.startswith('Tablet.')}

    def _module_digest(self, name):
        if name in self.hashes:
            return self.hashes[name]
        if name in self.visiting:
            raise Unavailable('cyclic imported module graph')
        self.visiting.add(name)
        row = self.modules[name]
        dependencies = {entry['module']: self._module_digest(entry['module']) for entry in row.get('imports', [])}
        value = digest(dict(module=name, observation=row, dependencies=dependencies,
            files={label: self.environment['files'][label] for label in row['file_bindings']}))
        self.visiting.remove(name)
        self.hashes[name] = value
        return value

    def owner_digest(self, node):
        return digest(dict(common=self.common, owner='Tablet.' + node,
                           imports=self._module_digest('Tablet.' + node)))

    def files_for(self, nodes):
        pending, seen = ['Tablet.' + node for node in nodes], set()
        labels = set(self.environment['common_files'])
        while pending:
            name = pending.pop()
            if name in seen:
                continue
            seen.add(name)
            row = self.modules[name]
            labels.update(row['file_bindings'])
            pending.extend(entry['module'] for entry in row.get('imports', []))
        return {label: self.environment['files'][label] for label in labels}


def environment_for(repo, identity):
    return _environment_for(repo, identity)[0]


def environment_bytes_for(repo, identity, *, max_bytes=MAX_MANIFEST_BYTES):
    """Return validated canonical bytes for an optional bounded cache copy.

    A large historical pretty representation may conservatively stay cold.
    Every copied value still passes the ordinary identity check; size metadata
    only rejects optional candidates and never establishes validity.
    """
    return _environment_for(repo, identity, max_bytes=max_bytes)[1]


def _environment_for(repo, identity, *, max_bytes=None):
    from trellis.dispatch.acceptance_receipts import regular
    from trellis.dispatch.retained_files import logical_size
    if not isinstance(identity, str) or len(identity) != 64 or any(c not in '0123456789abcdef' for c in identity):
        raise Unavailable('missing portable replay environment')
    path = Path(repo) / DIRECTORY / (identity + '.json')
    limit = MAX_MANIFEST_BYTES if max_bytes is None else min(MAX_MANIFEST_BYTES, max_bytes)
    if logical_size(path) > limit:
        raise Unavailable('oversized portable replay environment')
    value = json.loads(regular(path))
    if value.get('version') != VERSION:
        raise Unavailable('portable replay environment changed')
    data = _json(value)
    if hashlib.sha256(data).hexdigest() != identity:
        raise Unavailable('portable replay environment changed')
    if max_bytes is not None and len(data) > max_bytes:
        raise Unavailable('oversized portable replay environment')
    return value, data


def bound_path(label, repo, sysroot):
    if label.startswith('repo/'):
        return Path(repo) / label[5:]
    if label.startswith('sysroot/'):
        return Path(sysroot) / label[8:]
    if label.startswith('external/'):
        return Path(label[9:])
    raise Unavailable('invalid bound artifact path')


def rebind_record(repo, node, record, environment):
    """Relocate labels after exact environment validation; never add trust."""
    replay = dict(record['kernel_replay'])
    imports = {entry['module']: entry for entry in environment['modules']['Tablet.' + node].get('imports', [])}
    replay['trusted_direct_imports'] = [dict(module=entry['module'],
        olean=str(bound_path(imports[entry['module']]['path'], repo, environment['sysroot'])))
        for entry in replay['trusted_direct_imports']]
    return dict(record, kernel_replay=replay)


def materialized_environment(workspace, nodes):
    """Expose existing host replay bindings for one provisional probe batch.

    No collector, build, replay, donation or portability mint. The native
    caller must rehash the complete returned file/absence witness before using
    its provisional batch. Missing coverage keeps ordinary issuance available.
    """
    from trellis.atomic_actions import observations as obs
    from trellis.dispatch.candidate import CandidateError
    start = time.monotonic()
    try:
        roots = sorted(set(nodes))
        if not roots or len(roots) > MAX_MODULES:
            raise Unavailable('empty/oversized materialized replay roots')
        with workspace._workspace_lock.read_lock():
            if not workspace._closures_have_current_kernel_replay(roots):
                raise Unavailable('materialized replay is not current')
            views, owners, owner_hashes, files = {}, {}, {}, {}
            total_bytes = 0
            for node in roots:
                record = obs._read_olean_provenance(workspace.supervisor_repo, node)
                replay = record.get('kernel_replay') or {}
                identity = replay.get('portable_environment_sha256')
                if identity not in views:
                    if len(views) >= 128:
                        raise Unavailable('materialized environment inventory limit')
                    environment = environment_for(workspace.supervisor_repo, identity)
                    total_bytes += len(_json(environment))
                    if total_bytes > 64 << 20:
                        raise Unavailable('materialized environment byte limit')
                    views[identity] = EnvironmentView(environment)
                view = views[identity]
                if view.environment['toolchain_sha256'] != replay.get('toolchain_sha256'):
                    raise Unavailable('materialized environment toolchain differs from replay')
                row = view.modules['Tablet.' + node]
                bundle = [{key: part[key] for key in ('level', 'sha256', 'size_bytes')}
                          for part in row['parts']]
                if bundle != replay.get('artifact_bundle'):
                    raise Unavailable('materialized environment owner differs from replay')
                imports = sorted((entry['module'], str(bound_path(entry['path'], workspace.supervisor_repo,
                    view.environment['sysroot']))) for entry in row['imports'])
                replay_imports = sorted((entry['module'], entry['olean'])
                                        for entry in replay.get('trusted_direct_imports', []))
                if imports != replay_imports:
                    raise Unavailable('materialized environment imports differ from replay')
                owner_hashes[node] = view.owner_digest(node)
                owners.setdefault(identity, []).append(node)
            for identity, members in owners.items():
                view = views[identity]
                for label, binding in view.files_for(members).items():
                    path = str(bound_path(label, workspace.supervisor_repo, view.environment['sysroot']))
                    value = binding['sha256'] if binding is not None else None
                    if path in files and files[path] != value:
                        raise Unavailable('materialized environments disagree on imported files')
                    files[path] = value
                    if len(files) > MAX_FILE_BINDINGS:
                        raise Unavailable('materialized environment file limit')
            witness = dict(version=1, environment_sha256s=owner_hashes, artifact_files=files)
            if len(_json(witness)) > MAX_MANIFEST_BYTES:
                raise Unavailable('materialized artifact witness byte limit')
            return dict(environment=witness, measurement=dict(status='materialized',
                manifests=len(views), file_bindings=len(files), wall_secs=time.monotonic()-start))
    except (OSError, ValueError, KeyError, TypeError, RecursionError, CandidateError) as exc:
        return dict(environment=None, cold_reason=str(exc)[:240],
                    measurement=dict(status='unavailable', wall_secs=time.monotonic()-start))


def capture_current(workspace, nodes, *, timeout_secs=CAPTURE_SECONDS, environment_source=None):
    """Read-only observation seam; no sync, materialization or attestation mint.

    Callers bind this snapshot at the actual baseline/final observation phase.
    A later host capture alone cannot retroactively authenticate old facts.
    """
    with workspace._workspace_lock, workspace._sync_lock:
        environment, measurement = capture(workspace.supervisor_repo, nodes,
            timeout_secs=min(timeout_secs, CAPTURE_SECONDS), bwrap_role='lake_compiler',
            telemetry_origin='readonly_current')
        if environment is None:
            return dict(environment=None, measurement=measurement,
                        cold_reason=measurement.get('reason', 'imported environment unavailable'),
                        failure_class=measurement.get('failure_class', 'environment_binding'),
                        **({'failure_detail':measurement['failure_detail']} if 'failure_detail' in measurement else {}))
        try:
            view = EnvironmentView(environment)
            witness = dict(version=1, environment_sha256s={node: view.owner_digest(node) for node in sorted(set(nodes))},
                artifact_files={str(bound_path(label, workspace.supervisor_repo, environment['sysroot'])):
                    binding['sha256'] if binding is not None else None
                    for label, binding in view.files_for(nodes).items()})
            from trellis.dispatch.replay_donation import current_owner_aliases
            aliases, relocations = current_owner_aliases(workspace, view, nodes, environment_source=environment_source)
            if aliases:
                witness['environment_sha256s'].update(aliases)
                measurement['adapter_relocation_nodes'] = sorted(aliases)
                measurement['adapter_relocations'] = relocations
            if len(_json(witness)) > MAX_MANIFEST_BYTES:
                raise CaptureUnavailable('artifact witness byte limit')
            return dict(environment=witness, measurement=measurement)
        except (OSError, ValueError, KeyError, TypeError, RecursionError) as exc:
            return dict(environment=None, measurement=measurement, cold_reason=str(exc)[:240],
                        failure_class='environment_capture' if isinstance(exc, CaptureUnavailable) else 'environment_binding')
