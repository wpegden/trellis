"""Host-issued workspace capabilities for the single authoritative checker.

Each task has separate sources, artifacts, caches and locks. Only the listener
and compiler admission are shared. The wire never accepts a workspace path.
The registry lives outside every agent mount; entries are written by launchers.
"""
from __future__ import annotations

from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import secrets
import threading
import time

from trellis.incremental_check import _process_identity
from trellis.checker.protocol import ProtocolError

TOKEN_PREFIX = "workspace:"
MAX_CONTEXTS = 32
LIVENESS_POLL_SECONDS = .1


def registry_path(runtime):
    return Path(runtime) / "checker-state" / "workspace-capabilities"


def capability_live(data):
    """An owner-bound grant still ends on death, PID reuse or revocation."""
    if data['expires'] is not None and time.time() >= data['expires']:
        return False
    owner = data['owner']
    stat = (Path('/proc') / str(owner['pid']) / 'stat').read_text()
    return (stat[stat.rfind(')') + 2:].split()[0] not in ('Z', 'X')
            and _process_identity(owner['pid']) == owner)


def _metadata(task_runtime):
    raw = (task_runtime / "runtime_metadata.json").read_bytes()
    metadata = json.loads(raw)
    repo = Path(metadata["repo_path"]).resolve()
    if repo != task_runtime.parent / "repo" or not (repo / ".trellis").is_dir():
        raise ValueError("task runtime must name its own sibling repo workspace")
    return raw


def register(runtime, task_runtime, *, lifetime=86400, capacity_recovery=False):
    """Register a prepared private runtime; return its scoped checker channel.

    None binds host acceptance to its launcher's lifetime instead of a clock.
    Explicit revocation and exact owner identity checks apply to every grant.
    """
    runtime, task_runtime = Path(runtime).resolve(), Path(task_runtime).resolve()
    if not task_runtime.is_relative_to(runtime / "dispatch"):
        raise ValueError("task checker runtime must be inside this run's dispatch directory")
    if not (task_runtime / "runtime_metadata.json").is_file():
        raise ValueError("task checker runtime has no prepared metadata")
    _metadata(task_runtime)
    if lifetime is not None and (type(lifetime) not in (int, float) or not 0 < lifetime <= 7 * 86400):
        raise ValueError("workspace capability lifetime must be positive and at most seven days")
    if type(capacity_recovery) is not bool:
        raise ValueError('capacity recovery grant must be a host boolean')
    token = TOKEN_PREFIX + secrets.token_urlsafe(32)
    key = hashlib.sha256(token.encode()).hexdigest()
    root = registry_path(runtime)
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    entry = dict(runtime=str(task_runtime), owner=_process_identity(os.getpid()),
                 capacity_recovery=capacity_recovery,
                 expires=None if lifetime is None else time.time() + lifetime)
    path = root / (key + ".json")
    temp = root / (key + ".tmp")
    with temp.open("x") as stream:
        os.fchmod(stream.fileno(), 0o600)
        json.dump(entry, stream)
        stream.flush()
        os.fsync(stream.fileno())
    temp.replace(path)
    return {"TRELLIS_CHECKER_SOCKET": str(runtime / "sockets" / "checker.sock"), "TRELLIS_CHECKER_TOKEN": token}


def revoke(runtime, token):
    if not isinstance(token, str) or not token.startswith(TOKEN_PREFIX):
        raise ValueError("expected a task workspace capability")
    key = hashlib.sha256(token.encode()).hexdigest()
    directory = registry_path(runtime)
    (directory / (key + ".json")).unlink(missing_ok=True)
    if directory.exists():
        fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)


class _RequestLiveness:
    """Bounded authentication polling for one acquired capture request."""
    def __init__(self, router, key, root, entry, recovery):
        self.router, self.key, self.root = router, key, root
        self.entry, self.recovery = entry, recovery
        self.cancelled = threading.Event()
        self.lock = threading.Lock()
        self.next_check = 0.0

    def __call__(self):
        self._check(force=False)

    def force(self):
        self._check(force=True)

    def _check(self, *, force):
        with self.lock:
            if self.cancelled.is_set():
                raise ProtocolError('auth_required', 'unknown or expired task workspace capability')
            now = time.monotonic()
            if not force and now < self.next_check:
                return
            root, signature, recovery = self.router._entry(self.key)
            with self.router.lock:
                if (self.cancelled.is_set() or root != self.root
                        or self.router.contexts.get(root) is not self.entry
                        or signature != self.entry['signature'] or recovery != self.recovery):
                    raise ProtocolError('auth_required', 'task workspace binding changed during its check')
            # Count from the start: a slow authentication cannot extend reuse.
            self.next_check = now + LIVENESS_POLL_SECONDS


class WorkspaceRouter:
    def __init__(self, runtime, factory, *, max_contexts=MAX_CONTEXTS):
        self.runtime = Path(runtime).resolve()
        self.factory = factory
        self.max_contexts = max_contexts
        self.lock = threading.RLock()
        self.contexts = {}
        self.retiring = {}

    def release(self, token):
        """Revoke without opening a context; wait for active checks and reap.

        Multiple tokens for one workspace must all be revoked before that
        workspace can be reclaimed. A missing or failed cleanup never frees a
        worker slot. After a checker restart no old context may be adopted.
        """
        if not isinstance(token, str) or not token.startswith(TOKEN_PREFIX) or not 32 <= len(token) <= 128:
            raise ProtocolError('auth_required', 'missing task workspace capability')
        key = hashlib.sha256(token.encode()).hexdigest()
        with self.lock:
            for root, entry in self.contexts.items():
                if key in entry['tokens']:
                    self.retiring[key] = root
                    for request in entry['requests']:
                        if request.key == key:
                            request.cancelled.set()
            revoke(self.runtime, token)
            self._collect()
            from trellis.checker import admission
            if admission.ACTIVE is not None and admission.ACTIVE.cleanup_failed:
                raise ProtocolError('internal_error', 'compiler cleanup incomplete; retain the worker slot and restart the checker to reconcile recorded processes')
            drained = self.retiring.get(key) not in self.contexts
            if drained:
                self.retiring.pop(key, None)
            return {'drained': drained}

    def check_live(self, token):
        key = hashlib.sha256(token.encode()).hexdigest()
        root, signature, recovery = self._entry(key)
        with self.lock:
            entry = self.contexts.get(root)
            if entry is None or signature != entry["signature"]:
                raise ProtocolError("auth_required", "task workspace binding changed during its check")

    def _entry(self, key):
        try:
            data = json.loads((registry_path(self.runtime) / (key + ".json")).read_text())
            root = Path(data["runtime"]).resolve()
            if not root.is_relative_to(self.runtime / "dispatch"):
                raise ValueError("workspace is outside this run")
            if not capability_live(data):
                raise ValueError("attempt has ended")
            metadata = _metadata(root)
            # Changing the host's workspace binding cannot retarget a live
            # capability to different sources or inherit another scope's cache.
            recovery = data.get('capacity_recovery', False)
            if type(recovery) is not bool:
                raise ValueError('invalid capacity recovery grant')
            signature = hashlib.sha256(str(root).encode() + b"\0" + metadata).hexdigest()
            return root, signature, recovery
        except (OSError, ValueError, KeyError, TypeError):
            raise ProtocolError("auth_required", "unknown or expired task workspace capability") from None

    def _collect(self):
        for root, entry in list(self.contexts.items()):
            if entry["active"]:
                continue
            for key in list(entry["tokens"]):
                try:
                    path, signature, _recovery = self._entry(key)
                    if path == root and signature == entry["signature"]:
                        continue
                except ProtocolError:
                    pass
                entry["tokens"].remove(key)
            if entry["tokens"]:
                continue
            entry["handler"].shutdown(require_reaped=True)
            del self.contexts[root]

    @contextmanager
    def acquire(self, token, *, with_recovery=False, with_liveness=False):
        if not isinstance(token, str) or not token.startswith(TOKEN_PREFIX) or not 32 <= len(token) <= 128:
            raise ProtocolError("auth_required", "missing task workspace capability")
        key = hashlib.sha256(token.encode()).hexdigest()
        with self.lock:
            root, signature, recovery = self._entry(key)
            self._collect()
            entry = self.contexts.get(root)
            if entry is not None and entry["signature"] != signature:
                raise ProtocolError("auth_required", "task workspace binding changed during an active check")
            if entry is None:
                if len(self.contexts) >= self.max_contexts:
                    raise ProtocolError("resource_exhausted", "task checker workspace capacity is full; finish or release existing attempts")
                entry = dict(handler=self.factory(root), signature=signature, active=0, tokens=set(), requests=set())
                self.contexts[root] = entry
            entry["tokens"].add(key)
            entry["active"] += 1
            request = _RequestLiveness(self, key, root, entry, recovery) if with_liveness else None
            if request is not None:
                entry['requests'].add(request)
        try:
            if request is not None:
                yield entry['handler'], recovery, request
            else:
                yield (entry["handler"], recovery) if with_recovery else entry['handler']
        finally:
            with self.lock:
                if request is not None:
                    request.cancelled.set()
                    entry['requests'].remove(request)
                entry["active"] -= 1

    def shutdown(self):
        with self.lock:
            if any(e["active"] for e in self.contexts.values()):
                raise RuntimeError("cannot close task checker workspaces while requests are active")
            for entry in self.contexts.values():
                entry["handler"].shutdown()
            self.contexts.clear()
