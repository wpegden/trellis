"""Unified-checker UNIX-socket dispatcher.

Step 1 of the migration plan (design plan §4): scaffolding only. The server
is a sibling module to ``trellis.runtime.bridge`` and is not yet wired into
``restart_configured_run.sh``. It is callable manually for /tmp/ integration
testing via::

    python3 -m trellis.checker.server <runtime_root>

Architecture (per design plan §4 + threat-model mitigations):

- One acceptor thread owns the listening socket. Workers (in Step 2) will
  ``connect()`` once per burst and exchange line-delimited JSON requests.
- Per-connection handler reads one JSON line, dispatches to an op handler,
  writes one JSON line, and loops until ``EOF``.
- A workspace-level ``RLock`` guards the sync+lake critical section so
  concurrent verify-node requests serialize on the supervisor workspace
  (lake itself is not multi-process-safe on a single workspace).
- Every op handler runs the existing observation function with
  ``bwrap_role="lake_compiler"`` (mitigation 1): every supervisor-side
  lake invocation is bwrap-confined.
- Filesystem socket (NOT abstract namespace), mode ``0o660``, group
  the supervisor's group, ``os.umask(0o007)`` before ``bind()`` (mitigation 4).
  ``SO_PEERCRED`` uid check on accept; in-flight connection cap; per-line
  and per-message size caps.

Server log: append-only newline-JSON at
``<runtime_root>/checker-state/server.log`` with one record per request
(``request_id``, ``op``, ``nodes`` or ``node``, ``sync_changed_files``,
``lake_duration_ms``, ``returncode``).
"""

from __future__ import annotations

import argparse
import errno
import fcntl
import grp
import hashlib
import json
import logging
import os
import re
import signal
import socket
import struct
import sys
import threading
import time
import traceback
from concurrent.futures import ThreadPoolExecutor
from contextlib import suppress
from pathlib import Path
from typing import Any, Dict, Mapping, Optional, Sequence, Tuple

from trellis.atomic_actions import observations
from trellis.checker.protocol import (
    MAX_LINE_BYTES,
    MAX_MESSAGE_BYTES,
    NODE_NAME_REGEX,
    CheckerRequest,
    ProtocolError,
    encode_response,
    parse_line,
    rpc_error_envelope,
    validate_request,
)
from trellis.checker.sync import (
    LOCAL_CLOSURE_AXIOMS_CACHE_VERSION,
    PRINT_AXIOMS_CACHE_VERSION,
    SEMANTIC_PAYLOAD_CACHE_VERSION,
    SyncError,
    compute_semantic_payload_cache_key,
    load_fingerprint_cache,
    load_local_closure_axioms,
    load_print_axioms,
    load_semantic_payload,
    local_closure_axioms_cache_dir,
    print_axioms_cache_dir,
    semantic_payload_cache_dir,
    store_local_closure_axioms,
    store_print_axioms,
    store_semantic_payload,
    sync_tablet_dir,
)
from trellis.supervisor_workspace import authoritative_repo_path


_LOGGER = logging.getLogger("trellis.checker.server")


def _compile_output_has_sorry_warning(stdout: Any, stderr: Any) -> bool:
    """Return whether Lean's compile output contains a sorry warning.

    The Rust checker only uses this one semantic fact from compile
    stdout/stderr. Cache hits must preserve it; replacing a warning-bearing
    compile result with empty output makes an open proof look closed and can
    route it into ``#print axioms`` incorrectly.
    """
    output = f"{stdout or ''}\n{stderr or ''}"
    for line in output.splitlines():
        lower = line.lower()
        if "sorry" in lower and ("warning" in lower or "declaration uses" in lower):
            return True
    return False


class SingletonError(RuntimeError):
    """Raised when another checker server already holds the runtime PID lock.

    Surfaces the existing server's PID (from the on-disk pid file) so the
    operator can kill or investigate the live instance instead of stomping
    its UNIX socket.
    """

    def __init__(self, pid_path: Path, existing_pid: Optional[int]) -> None:
        if existing_pid is None:
            message = (
                f"another checker server holds the lock at {pid_path} "
                f"(pid file content unreadable)"
            )
        else:
            message = (
                f"another checker server is already running (pid={existing_pid}, "
                f"lock={pid_path})"
            )
        super().__init__(message)
        self.pid_path = pid_path
        self.existing_pid = existing_pid

# Default thread-pool size when TRELLIS_LEAN_PARALLELISM is unset.
DEFAULT_PARALLELISM = 6
# In-flight connection cap (mitigation 4). The bridge's
# prepare_compiled_support fans out multiple concurrent check.py
# invocations during worker prep — one connection per concurrent task —
# and easily exceeds the thread-pool size. Sized comfortably above
# DEFAULT_PARALLELISM so the bridge can run many parallel observation
# calls without tripping the cap. Connections in excess of the cap
# block on the semaphore for CONNECTION_ACQUIRE_TIMEOUT_SECS rather
# than being summarily rejected — silent rejection produced
# "Connection reset by peer" on the live run on 2026-04-28 when the
# bridge's prep flow hit the 4-connection cap mid-burst.
MAX_INFLIGHT_CONNECTIONS = 32
# How long an incoming connection waits for a slot before the server
# rejects it. Generous because lake operations can take 100+ seconds
# and the bridge legitimately holds connections for that long.
CONNECTION_ACQUIRE_TIMEOUT_SECS = 600.0
HEADER_READ_TIMEOUT_SECS = 60.0
# Group name owning the socket. Workers must be members of this group so
# `0o660` mode permits connect(). Empty by default: the group is derived from
# the running user's own primary group (see ``_resolve_socket_group_gid``).
# Override with an explicit group name only if a distinct burst uid needs
# group-based access.
SOCKET_GROUP_NAME = ""


def _resolve_socket_group_gid() -> Optional[int]:
    if SOCKET_GROUP_NAME:
        try:
            return int(grp.getgrnam(SOCKET_GROUP_NAME).gr_gid)
        except KeyError:
            # Fall back to the caller's group membership; hosts without the
            # configured group still get a working socket (chown is a no-op).
            return None
    # Default: own primary group. Self-chown is a no-op under single-uid.
    return os.getgid()


def _stat_mtime_ns(path: Path) -> Optional[int]:
    """Return the file's mtime in ns, or ``None`` if it does not exist
    or another I/O error occurs.

    Used by ``_dispatch_op`` to capture pre- and post-lake source
    mtimes for the sync-vs-lake race mitigation: if the source mtime
    changes during lake, the resulting olean was compiled from a
    different source revision and must NOT be recorded as
    ``known_current``. ``None`` from either call is treated as
    "skip recording" so the caller doesn't make assumptions about
    paths that race with a delete.
    """
    try:
        return int(path.stat().st_mtime_ns)
    except (FileNotFoundError, OSError):
        return None


def _sha256_file_or_empty(path: Path) -> str:
    """Return the file's SHA-256 hex digest, or ``""`` on any I/O failure.

    Used to fingerprint the lean-toolchain pin and the
    ``lean_semantic_fingerprint.lean`` script that gate the
    semantic-payload cache. Returning ``""`` on missing/unreadable files
    forces a cache skip for the affected request (see
    ``_handle_lean_semantic_payloads``) rather than a hard error — the
    cache must never be load-bearing for correctness.
    """
    try:
        with open(path, "rb") as handle:
            digest = hashlib.sha256()
            for chunk in iter(lambda: handle.read(1 << 16), b""):
                digest.update(chunk)
            return digest.hexdigest()
    except OSError:
        return ""


def _runtime_socket_dir(runtime_root: Path) -> Path:
    return runtime_root / "sockets"


def _runtime_socket_path(runtime_root: Path) -> Path:
    return _runtime_socket_dir(runtime_root) / "checker.sock"


def _runtime_state_dir(runtime_root: Path) -> Path:
    return runtime_root / "checker-state"


def _runtime_log_path(runtime_root: Path) -> Path:
    return _runtime_state_dir(runtime_root) / "server.log"


def _runtime_pid_path(runtime_root: Path) -> Path:
    return _runtime_state_dir(runtime_root) / "server.pid"


def _runtime_burst_tokens_path(runtime_root: Path) -> Path:
    """Path to the per-burst HMAC-token registry.

    Phase 2 of the bwrap-only migration plan
    (SANDBOX_BWRAP_ONLY_MIGRATION_PLAN_2026-06-03.md §3): the bridge mints
    ``secrets.token_urlsafe(16)`` at burst dispatch and atomically writes
    the live token set here (mode 0o600, supervisor-owned). The server
    reads this file on every connection accept (so token revocation
    takes effect within one connection of the bridge update). The bursts
    inside bwrap cannot read this file because ``<runtime>/`` is not
    bind-mounted into the burst's filesystem view; the token reaches
    them via ``--setenv TRELLIS_CHECKER_TOKEN`` only.
    """
    return _runtime_state_dir(runtime_root) / "burst-tokens.json"


def _runtime_fingerprint_cache_path(runtime_root: Path) -> Path:
    return _runtime_state_dir(runtime_root) / "sync-fingerprints.json"


def _resolve_worker_repo_for_runtime(runtime_root: Path) -> Path:
    """Derive the worker repo from a runtime_root path.

    Two layouts are supported:

    1. **Inner form** (``_bridge_state_dir`` convention in
       ``runtime/bridge.py``): ``<worker_repo>/.trellis/runtime/<name>``.
       Walk up three levels and sanity-check the resulting directory.

    2. **Outer form** (``restart_configured_run.sh`` and ``trellis.sh``
       convention): ``<parent>/<repo_basename>-runtime``, where
       ``<parent>/<repo_basename>`` is the worker repo (i.e. the runtime
       state lives as a sibling of the repo, not inside it). Strip the
       ``-runtime`` suffix from the runtime_root basename and look for a
       sibling directory containing ``.trellis/``.

    The two forms reflect the path-layout asymmetry between the
    supervisor runtime and the checker server; this function
    accepts either so ``restart_configured_run.sh --with-checker-rpc``
    can pass the supervisor's outer-form path through unchanged.
    """
    runtime_root = runtime_root.resolve()
    # Inner form first.
    if (
        runtime_root.parent.name == "runtime"
        and runtime_root.parent.parent.name == ".trellis"
    ):
        worker_repo = runtime_root.parent.parent.parent
        if not worker_repo.is_dir():
            raise ValueError(
                f"worker repo derived from runtime_root is not a directory: {worker_repo}"
            )
        return worker_repo
    # Outer form: <parent>/<repo_basename>-runtime → <parent>/<repo_basename>.
    name = runtime_root.name
    suffix = "-runtime"
    if name.endswith(suffix) and len(name) > len(suffix):
        repo_basename = name[: -len(suffix)]
        candidate = runtime_root.parent / repo_basename
        if candidate.is_dir() and (candidate / ".trellis").is_dir():
            return candidate
        raise ValueError(
            f"runtime_root '{runtime_root}' looks like outer form (basename "
            f"ends in '-runtime') but sibling repo '{candidate}' is not a "
            f"directory containing '.trellis/'"
        )
    raise ValueError(
        f"runtime_root path layout unexpected: expected inner form "
        f"(<worker_repo>/.trellis/runtime/<name>) or outer form "
        f"(<parent>/<repo_basename>-runtime), got {runtime_root}"
    )


def _format_log_record(
    *,
    request_id: int,
    op: str,
    started_ns: int,
    finished_ns: int,
    returncode: Any,
    sync_changed_files: int,
    extra: Mapping[str, Any],
) -> bytes:
    record = {
        "ts": time.time(),
        "request_id": request_id,
        "op": op,
        "duration_ms": int((finished_ns - started_ns) / 1_000_000),
        "returncode": returncode,
        "sync_changed_files": sync_changed_files,
        **dict(extra),
    }
    return (json.dumps(record, separators=(",", ":"), ensure_ascii=False) + "\n").encode("utf-8")


class _RequestLog:
    """Append-only newline-JSON request log with a per-server lock."""

    def __init__(self, path: Path) -> None:
        self._path = path
        self._lock = threading.Lock()
        path.parent.mkdir(parents=True, exist_ok=True)

    def emit(
        self,
        *,
        request_id: int,
        op: str,
        started_ns: int,
        finished_ns: int,
        returncode: Any,
        sync_changed_files: int,
        **extra: Any,
    ) -> None:
        line = _format_log_record(
            request_id=request_id,
            op=op,
            started_ns=started_ns,
            finished_ns=finished_ns,
            returncode=returncode,
            sync_changed_files=sync_changed_files,
            extra=extra,
        )
        with self._lock:
            try:
                with open(self._path, "ab", buffering=0) as fh:
                    fh.write(line)
            except OSError:
                # Logging never blocks a request — at worst we lose a line.
                _LOGGER.exception("failed to append checker server log line")


class CheckerServer:
    """Unified-checker UNIX-socket dispatcher.

    The lifecycle is owned by ``main()`` below: the server's caller
    constructs an instance, calls ``start()``, waits on ``serve_forever()``
    or ``stop_event``, and then calls ``shutdown()`` once SIGTERM lands.
    """

    def __init__(
        self,
        runtime_root: Path,
        *,
        parallelism: Optional[int] = None,
        socket_group_gid: Optional[int] = None,
    ) -> None:
        self.runtime_root = runtime_root.resolve()
        self.worker_repo = _resolve_worker_repo_for_runtime(self.runtime_root)
        self.supervisor_repo = authoritative_repo_path(self.worker_repo)
        self.socket_path = _runtime_socket_path(self.runtime_root)
        self.fingerprint_cache_path = _runtime_fingerprint_cache_path(self.runtime_root)
        self.semantic_payload_cache_dir = semantic_payload_cache_dir(
            _runtime_state_dir(self.runtime_root)
        )
        self.print_axioms_cache_dir = print_axioms_cache_dir(
            _runtime_state_dir(self.runtime_root)
        )
        # Patch C deferred cache: sidecar dir for local_closure_axioms.
        # Key derivation mirrors print_axioms (same closure-walked surface),
        # but uses the local-closure script's sha as `script_sha256` so the
        # cache invalidates on script edits independently from fingerprint
        # script edits. See `_handle_local_closure_axioms`.
        self.local_closure_axioms_cache_dir = local_closure_axioms_cache_dir(
            _runtime_state_dir(self.runtime_root)
        )
        # Inputs to the semantic-payload cache key beyond per-node sha:
        # the fingerprint script that lake invokes, the workspace's
        # lean-toolchain pin, and the lake-manifest (pins the mathlib
        # rev independently of lean-toolchain). Stored as Path so the
        # per-request handler can rehash on each call (small files, ~ms).
        self._fingerprint_script_path = (
            Path(__file__).resolve().parents[2]
            / "scripts"
            / "lean_semantic_fingerprint.lean"
        )
        # Patch A local-closure probe script (LOCAL_CLOSURE_IMPL_PLAN.md §5.5).
        # Lives in the same source ``scripts/`` dir as the fingerprint script;
        # sandbox.py mounts that whole directory read-only into the
        # ``lake_compiler`` bwrap (see trellis/sandbox.py:312-316), so no
        # additional bind is required when the new script is added.
        self._local_closure_script_path = (
            Path(__file__).resolve().parents[2]
            / "scripts"
            / "lean_local_closure.lean"
        )
        self._toolchain_path = self.supervisor_repo / "lean-toolchain"
        self._lake_manifest_path = self.supervisor_repo / "lake-manifest.json"
        self.log_path = _runtime_log_path(self.runtime_root)
        self.pid_path = _runtime_pid_path(self.runtime_root)
        # Phase 2 (bwrap-only migration): per-burst token registry. Mtime/
        # content reload on every accept, atomic-rename writer on the bridge
        # side. See _runtime_burst_tokens_path for the trust-model docstring.
        self.burst_tokens_path = _runtime_burst_tokens_path(self.runtime_root)
        # In-process per-server token allowlist, populated either via
        # register_burst_token (in-test, for unit tests that don't write a
        # file) or refreshed from `burst_tokens_path` on every accept.
        # When BOTH the in-process set and the on-disk file are empty, the
        # server falls back to the legacy SO_PEERCRED-uid gate: this keeps
        # the dormant-token scenario working (Phase 2 source can ship to
        # the running supervisor BEFORE the checker restart; the live
        # checker still on old code in memory does UID checks unchanged
        # and accepts current burst-user bursts; the new file-token
        # path activates only after restart).
        self._burst_tokens: set[str] = set()
        self._burst_tokens_lock = threading.Lock()
        self._burst_tokens_mtime_ns: Optional[int] = None
        self.parallelism = parallelism or _read_parallelism()
        if socket_group_gid is None:
            socket_group_gid = _resolve_socket_group_gid()
        self.socket_group_gid = socket_group_gid
        # Post-bwrap-only migration: workers run under the supervisor's own
        # uid inside the bwrap namespace, so the SO_PEERCRED allowlist is
        # just the server's euid. Token gate handles authentication; this
        # uid check is a defence-in-depth filter against unauthorised hosts
        # that somehow gained access to the socket directory.
        self.expected_peer_uids: set[int] = {os.geteuid()}

        self._workspace_lock = threading.RLock()
        # ``_sync_lock`` serializes ``sync_tablet_dir`` invocations
        # independently of ``_workspace_lock`` (the lake lock). Two
        # simultaneous ``sync_tablet_dir`` calls would race on the
        # supervisor's ``Tablet/`` files and the
        # ``sync-fingerprints.json`` cache — both are worker-supervisor
        # mirrors that must serialize. Lake invocations remain
        # serialized under ``_workspace_lock``; sync and lake CAN
        # overlap. The race that lets sync write a new source while
        # lake is mid-compile is mitigated by the per-node mtime
        # snapshot the lake dispatcher captures before/after each lake
        # call (see ``_dispatch_op``). Keeping these locks separate is
        # what lets cache-hit responses bypass ``_workspace_lock``
        # entirely while a long lake call is in flight on another
        # thread — the headline win of the unified-checker concurrency
        # restructure (was: 1.03x concurrency factor across 8.66h).
        self._sync_lock = threading.Lock()
        # Compile-cache: per-node "this node's olean is known current with
        # the supervisor's source tree" set, populated only after a
        # successful lake compile in *this* server lifetime. Cleared on any
        # sync diff (changed/removed/rejected). On cache hit we additionally
        # verify olean exists on disk with size>0 and mtime>=source mtime
        # before skipping lake — never trust the in-memory bit alone.
        self._oleans_known_current: set[str] = set()
        # Per-node Lean-derived compile warning fact. This is intentionally
        # separate from ``_oleans_known_current``: a current olean proves the
        # build artifact is reusable, but not that a synthetic compile result
        # may erase warning-bearing stdout/stderr. Absence means "unknown",
        # not "no sorry warning", so lean_compile_node falls through to lake.
        self._compile_has_sorry_warning: Dict[str, bool] = {}
        self._oleans_lock = threading.Lock()
        # ``prepare_compiled_support`` short-circuit cache: stores the
        # ``lake-manifest.json`` sha256 of the most recent successful prepare.
        # ``lake exe cache get`` is idempotent w.r.t. the manifest — once it
        # has populated the on-disk olean cache for a manifest revision,
        # re-running it is a multi-second no-op. The supervisor measured
        # ~13s/call on the live mathlib hot cache, ~65 calls/run = ~850s
        # wasted; the cache key is supervisor-computed so workers cannot
        # poison it. ``None`` (initial value) forces a real prepare on the
        # first request after server start, which seeds the cache for the
        # rest of the run. Protected by the workspace lock — writes only
        # happen on the lake side of the dispatch.
        self._last_successful_prepare_manifest: Optional[str] = None
        self._connection_semaphore = threading.BoundedSemaphore(MAX_INFLIGHT_CONNECTIONS)
        self._stop_event = threading.Event()
        self._listen_sock: Optional[socket.socket] = None
        self._executor: Optional[ThreadPoolExecutor] = None
        self._request_log = _RequestLog(self.log_path)
        self._started_at_ns = time.monotonic_ns()
        # PID-lock fd: held open for the lifetime of the server. Closing
        # the fd releases the kernel-level fcntl lock; that's what fences
        # any second instance out via SingletonError below.
        self._pid_lock_fd: Optional[int] = None

    # --------------------------- lifecycle ---------------------------

    def set_expected_peer_uid(self, uid: int) -> None:
        """Replace the SO_PEERCRED allowlist with a single explicit uid.

        Kept for legacy CLI compatibility (``--peer-uid``). Most callers
        should rely on the default allowlist (server's own euid) and not
        call this.
        """
        self.expected_peer_uids = {int(uid)}

    def add_expected_peer_uid(self, uid: int) -> None:
        """Extend the SO_PEERCRED allowlist with an additional uid."""
        self.expected_peer_uids.add(int(uid))

    def register_burst_token(self, token: str) -> None:
        """Add a token to the in-process burst-token allowlist.

        Phase 2 of the bwrap-only migration. Test helper plus optional
        in-process registration. Production bridges write tokens to
        ``burst_tokens_path`` via atomic rename; the server's per-accept
        refresh picks them up.
        """
        if not isinstance(token, str) or not token.strip():
            raise ValueError("burst token must be a non-empty string")
        with self._burst_tokens_lock:
            self._burst_tokens.add(token.strip())

    def revoke_burst_token(self, token: str) -> None:
        """Drop a token from the in-process burst-token allowlist."""
        with self._burst_tokens_lock:
            self._burst_tokens.discard(str(token).strip())

    def _reload_burst_tokens_if_changed(self) -> None:
        """Refresh ``self._burst_tokens`` from the on-disk registry file
        when its mtime has advanced since the last reload. Idempotent and
        best-effort: any I/O or JSON-decode error leaves the prior in-memory
        set in place so a transient bridge rename mid-read doesn't lock
        the server out.

        File schema: ``{"tokens": ["...", "..."]}`` written atomically by
        the bridge (``os.replace``); reads are racy by definition but the
        atomic rename guarantees we always see either the full pre-write
        contents or the full post-write contents, never a torn write.
        """
        path = self.burst_tokens_path
        try:
            st = path.stat()
        except FileNotFoundError:
            # File absent — preserve in-process tokens (e.g. registered
            # via register_burst_token in tests). Reset the mtime cache
            # so a future write is observed even with identical content.
            self._burst_tokens_mtime_ns = None
            return
        except OSError:
            return
        mtime_ns = int(st.st_mtime_ns)
        if (
            self._burst_tokens_mtime_ns is not None
            and mtime_ns == self._burst_tokens_mtime_ns
        ):
            return
        try:
            with open(path, "rb") as fh:
                data = json.loads(fh.read().decode("utf-8") or "{}")
        except (OSError, json.JSONDecodeError, UnicodeDecodeError):
            return
        if not isinstance(data, dict):
            return
        tokens_raw = data.get("tokens", [])
        if not isinstance(tokens_raw, list):
            return
        new_tokens: set[str] = set()
        for entry in tokens_raw:
            if isinstance(entry, str) and entry.strip():
                new_tokens.add(entry.strip())
        with self._burst_tokens_lock:
            self._burst_tokens = new_tokens
            self._burst_tokens_mtime_ns = mtime_ns

    def _has_any_burst_tokens(self) -> bool:
        """Return True when at least one token is registered (in-process
        or loaded from disk). When False, the gate falls back to legacy
        UID-only checking — the dormant-Phase-2 path."""
        with self._burst_tokens_lock:
            return bool(self._burst_tokens)

    def _check_request_token(self, request: CheckerRequest) -> bool:
        """Validate the ``auth_token`` field on the request envelope
        against the in-process burst-token allowlist.

        Returns True when accepted, False when rejected. When the
        allowlist is empty (i.e. no bridge has registered or written any
        token yet), this method returns True unconditionally — the
        legacy UID gate is the active line of defence and the token
        plumbing is dormant. Once any token is registered, the token gate
        becomes load-bearing and a request without (or with a wrong)
        token is rejected.
        """
        if not self._has_any_burst_tokens():
            return True
        raw_token = request.raw.get("auth_token")
        if not isinstance(raw_token, str) or not raw_token.strip():
            return False
        with self._burst_tokens_lock:
            return raw_token.strip() in self._burst_tokens

    def start(self) -> None:
        self.runtime_root.mkdir(parents=True, exist_ok=True)
        _runtime_socket_dir(self.runtime_root).mkdir(parents=True, exist_ok=True)
        _runtime_state_dir(self.runtime_root).mkdir(parents=True, exist_ok=True)

        # Acquire the PID lock BEFORE touching the socket so a concurrent
        # ``start()`` cannot stomp the live instance's listener. The
        # ``fcntl.LOCK_EX | LOCK_NB`` non-blocking exclusive lock on the
        # pid file is process-scoped at the OS level and released
        # automatically when the process exits (kernel cleanup) or when
        # we close the fd in ``shutdown()``. Opening as ``O_RDWR | O_CREAT``
        # (no truncation) lets us read the previous PID for the error
        # message even if the lock is already held.
        self._acquire_pid_lock()

        # Drop a stale socket node first; bind() is otherwise EADDRINUSE.
        # Safe now because we hold the singleton lock — any prior live
        # server has already failed at ``_acquire_pid_lock`` above.
        if self.socket_path.exists():
            with suppress(OSError):
                self.socket_path.unlink()

        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        # Phase 2 (bwrap-only migration): socket mode tightens 0o660 → 0o600
        # (owner-only). This is coupled to the Phase 4 cutover that drops
        # `sudo -n -u <burst-user>` from burst dispatch — once bursts run
        # as the supervisor's own uid, the previous group-rw
        # accommodation for the separate burst user is unnecessary. Until
        # Phase 4 ships, an operator-initiated checker restart without
        # the matching bridge update would break worker connect()s; the
        # cutover sequence in the plan keeps the two restarts coupled.
        prev_umask = os.umask(0o077)
        try:
            sock.bind(str(self.socket_path))
        finally:
            os.umask(prev_umask)
        try:
            os.chmod(self.socket_path, 0o600)
        except OSError as exc:
            _LOGGER.warning("could not chmod socket %s: %s", self.socket_path, exc)
        sock.listen(8)
        self._listen_sock = sock

        # Write our pid into the locked file (truncate first so a longer
        # prior PID doesn't leave trailing bytes). The fd we hold the lock
        # on is the same fd we write through.
        if self._pid_lock_fd is not None:
            try:
                os.lseek(self._pid_lock_fd, 0, os.SEEK_SET)
                os.ftruncate(self._pid_lock_fd, 0)
                os.write(self._pid_lock_fd, f"{os.getpid()}\n".encode("utf-8"))
            except OSError as exc:
                _LOGGER.warning(
                    "could not write pid to locked file %s: %s",
                    self.pid_path,
                    exc,
                )

        self._executor = ThreadPoolExecutor(
            max_workers=self.parallelism, thread_name_prefix="checker-worker"
        )

        # Pre-warm ``_oleans_known_current`` from on-disk state. Without this,
        # the first wave of requests after a server restart re-runs lake on
        # every node even if its olean is already current with its source —
        # the cache starts empty and only fills as lake invocations succeed.
        # The pre-warm is a pure read-only filesystem inspection of the
        # supervisor's ``Tablet/`` and ``.lake/build/lib/lean/Tablet/``
        # trees; the trust boundary is preserved because workers cannot
        # mutate either side from inside their bwrap (only the supervisor
        # writes the supervisor's ``.olean`` files via lake_compiler bwrap).
        # A node is considered current iff its olean exists, is non-empty,
        # and its mtime is at least the source's mtime — exactly the same
        # gate ``_compute_compile_cache_subset`` uses on the cache-hit path.
        self._prewarm_oleans_known_current()

        _LOGGER.info(
            "checker server listening at %s (parallelism=%d, supervisor_repo=%s, "
            "prewarmed_oleans=%d)",
            self.socket_path,
            self.parallelism,
            self.supervisor_repo,
            len(self._oleans_known_current),
        )

    def _prewarm_oleans_known_current(self) -> None:
        """Scan the supervisor repo for nodes whose olean is current with
        its source and seed ``_oleans_known_current`` with them.
        Idempotent and best-effort: any I/O error on a single file is
        swallowed so the server still starts on a partially-readable
        olean tree.
        """
        tablet_dir = self.supervisor_repo / "Tablet"
        olean_dir = (
            self.supervisor_repo / ".lake" / "build" / "lib" / "lean" / "Tablet"
        )
        if not tablet_dir.is_dir() or not olean_dir.is_dir():
            return
        try:
            sources = list(tablet_dir.glob("*.lean"))
        except OSError:
            return
        with self._oleans_lock:
            for src in sources:
                stem = src.stem
                if not NODE_NAME_REGEX.fullmatch(stem):
                    continue
                olean = olean_dir / f"{stem}.olean"
                try:
                    src_stat = src.stat()
                    olean_stat = olean.stat()
                except (FileNotFoundError, OSError):
                    continue
                if (
                    olean_stat.st_size > 0
                    and olean_stat.st_mtime >= src_stat.st_mtime
                ):
                    self._oleans_known_current.add(stem)

    def _acquire_pid_lock(self) -> None:
        """Take an exclusive non-blocking flock on the PID file.

        Raises ``SingletonError`` if another process already holds the lock,
        with the existing PID extracted from the on-disk file content for
        operator debugging.
        """
        self.pid_path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(
            str(self.pid_path),
            os.O_RDWR | os.O_CREAT | os.O_CLOEXEC,
            0o644,
        )
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            existing_pid = _read_pid_file_quietly(fd)
            os.close(fd)
            raise SingletonError(self.pid_path, existing_pid)
        except OSError:
            os.close(fd)
            raise
        self._pid_lock_fd = fd

    def shutdown(self) -> None:
        self._stop_event.set()
        if self._listen_sock is not None:
            with suppress(OSError):
                self._listen_sock.shutdown(socket.SHUT_RDWR)
            with suppress(OSError):
                self._listen_sock.close()
            self._listen_sock = None
        if self._executor is not None:
            self._executor.shutdown(wait=True)
            self._executor = None
        with suppress(OSError):
            if self.socket_path.exists():
                self.socket_path.unlink()
        # Release the PID lock and unlink the file. Closing the fd alone
        # would suffice (kernel releases the flock); we still ``flock(LOCK_UN)``
        # explicitly so a misbehaving caller racing on the file path sees
        # the unlock at the syscall level before the unlink.
        if self._pid_lock_fd is not None:
            with suppress(OSError):
                fcntl.flock(self._pid_lock_fd, fcntl.LOCK_UN)
            with suppress(OSError):
                os.close(self._pid_lock_fd)
            self._pid_lock_fd = None
        with suppress(OSError):
            if self.pid_path.exists():
                self.pid_path.unlink()

    @property
    def stop_event(self) -> threading.Event:
        return self._stop_event

    def serve_forever(self) -> None:
        assert self._listen_sock is not None and self._executor is not None
        sock = self._listen_sock
        sock.settimeout(0.5)  # short poll so SIGTERM unblocks accept loop
        while not self._stop_event.is_set():
            try:
                conn, _addr = sock.accept()
            except socket.timeout:
                continue
            except OSError as exc:
                if self._stop_event.is_set():
                    return
                _LOGGER.warning("accept() failed: %s", exc)
                continue

            # Wait up to CONNECTION_ACQUIRE_TIMEOUT_SECS for a slot.
            # Blocking with timeout (vs. immediate reject) means the
            # bridge's parallel observation calls queue rather than
            # observing "Connection reset by peer." Only summarily
            # rejected if the timeout elapses, in which case the cap is
            # genuinely undersized for the workload.
            if not self._connection_semaphore.acquire(
                blocking=True, timeout=CONNECTION_ACQUIRE_TIMEOUT_SECS
            ):
                _LOGGER.warning(
                    "rejecting connection: in-flight cap %d still saturated after %.0fs",
                    MAX_INFLIGHT_CONNECTIONS,
                    CONNECTION_ACQUIRE_TIMEOUT_SECS,
                )
                with suppress(OSError):
                    conn.close()
                continue

            if not self._check_peer_uid(conn):
                self._connection_semaphore.release()
                continue

            # Phase 2 (bwrap-only migration): refresh per-burst token
            # registry from disk on every accept. The on-disk file is
            # atomically replaced by the bridge per-dispatch, so each
            # connection sees a fresh snapshot of currently-live tokens.
            # Per-line token validation happens downstream in
            # _dispatch_line; this reload is a per-accept tax (one stat()
            # + occasional re-read) that keeps the gate decision local
            # to the validator.
            self._reload_burst_tokens_if_changed()

            try:
                self._executor.submit(self._handle_connection, conn)
            except Exception:
                _LOGGER.exception("could not dispatch connection to executor")
                with suppress(OSError):
                    conn.close()
                self._connection_semaphore.release()

    # --------------------------- connection ---------------------------

    def _check_peer_uid(self, conn: socket.socket) -> bool:
        """Validate ``SO_PEERCRED.uid`` against ``self.expected_peer_uid``.

        Returns ``True`` to accept the connection, ``False`` to reject it
        (and close the socket). Mitigation 4: deflects unauthorised hosts
        even if they somehow gained access to the socket directory.
        """
        try:
            data = conn.getsockopt(
                socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize("3i")
            )
        except OSError as exc:
            _LOGGER.warning("getsockopt(SO_PEERCRED) failed: %s", exc)
            with suppress(OSError):
                conn.close()
            return False
        _pid, peer_uid, _gid = struct.unpack("3i", data)
        if peer_uid not in self.expected_peer_uids:
            _LOGGER.warning(
                "rejecting connection: peer uid %d not in allowlist %s",
                peer_uid,
                sorted(self.expected_peer_uids),
            )
            with suppress(OSError):
                conn.close()
            return False
        return True

    def _handle_connection(self, conn: socket.socket) -> None:
        try:
            conn.settimeout(HEADER_READ_TIMEOUT_SECS)
            buffer = bytearray()
            while not self._stop_event.is_set():
                line = self._read_one_line(conn, buffer)
                if line is None:
                    return
                response_bytes = self._dispatch_line(line)
                try:
                    conn.sendall(response_bytes)
                except OSError as exc:
                    _LOGGER.warning("sendall() failed: %s", exc)
                    return
        except Exception:
            _LOGGER.exception("unhandled error in connection handler")
        finally:
            with suppress(OSError):
                conn.close()
            self._connection_semaphore.release()

    @staticmethod
    def _read_one_line(
        conn: socket.socket, buffer: bytearray
    ) -> Optional[bytes]:
        """Read one ``\\n``-terminated frame, enforcing the 64 KiB line cap.

        Returns the frame (with trailing newline) on success, ``None`` on
        clean ``EOF``. Raises ``ProtocolError(kind="malformed_request")``
        if the line cap is exceeded.
        """
        while True:
            newline_idx = buffer.find(b"\n")
            if newline_idx >= 0:
                line = bytes(buffer[: newline_idx + 1])
                del buffer[: newline_idx + 1]
                return line
            if len(buffer) > MAX_LINE_BYTES:
                raise ProtocolError(
                    "malformed_request",
                    f"request line exceeds {MAX_LINE_BYTES} bytes before newline",
                )
            try:
                chunk = conn.recv(8192)
            except socket.timeout:
                raise ProtocolError(
                    "malformed_request", "header read timed out"
                )
            except OSError as exc:
                raise ProtocolError(
                    "malformed_request", f"recv failed: {exc}"
                )
            if not chunk:
                if buffer:
                    raise ProtocolError(
                        "malformed_request", "EOF before terminating newline"
                    )
                return None
            buffer.extend(chunk)

    # --------------------------- dispatch ---------------------------

    def _dispatch_line(self, line: bytes) -> bytes:
        rid = 0
        try:
            payload = parse_line(line)
        except ProtocolError as exc:
            _LOGGER.warning("parse error: %s", exc.message)
            return encode_response(
                rpc_error_envelope(rid, exc.kind, exc.message)
            )

        # Pre-extract request_id for error path so even validation failure
        # echoes the caller's id where possible.
        try:
            rid = int(payload.get("request_id") or 0)
        except (TypeError, ValueError):
            rid = 0

        try:
            request = validate_request(payload)
        except ProtocolError as exc:
            return encode_response(
                rpc_error_envelope(rid, exc.kind, exc.message)
            )

        # Phase 2 (bwrap-only migration): per-request HMAC-token gate.
        # When no token is registered (dormant phase), this returns True
        # unconditionally and the UID gate at accept-time is the only
        # check. Once any token is registered (file or in-process), token
        # absence/mismatch fails the request loudly so callers can
        # distinguish auth failure from a tool failure.
        if not self._check_request_token(request):
            _LOGGER.warning(
                "rejecting request: auth_token missing or unknown (op=%s rid=%s)",
                request.op,
                request.request_id,
            )
            return encode_response(
                rpc_error_envelope(
                    request.request_id,
                    "auth_required",
                    "auth_token missing or unknown",
                )
            )

        try:
            response = self._handle_request(request)
        except SyncError as exc:
            self._request_log.emit(
                request_id=request.request_id,
                op=request.op,
                started_ns=time.monotonic_ns(),
                finished_ns=time.monotonic_ns(),
                returncode=None,
                sync_changed_files=0,
                error=str(exc),
                kind="sync_failed",
            )
            return encode_response(
                rpc_error_envelope(request.request_id, "sync_failed", str(exc))
            )
        except ProtocolError as exc:
            return encode_response(
                rpc_error_envelope(request.request_id, exc.kind, exc.message)
            )
        except Exception as exc:
            _LOGGER.exception("internal error handling op %s", request.op)
            self._request_log.emit(
                request_id=request.request_id,
                op=request.op,
                started_ns=time.monotonic_ns(),
                finished_ns=time.monotonic_ns(),
                returncode=None,
                sync_changed_files=0,
                error=str(exc),
                kind="internal_error",
                traceback=traceback.format_exc(limit=4),
            )
            return encode_response(
                rpc_error_envelope(
                    request.request_id, "internal_error", f"{type(exc).__name__}: {exc}"
                )
            )

        try:
            return encode_response(response)
        except ValueError as exc:
            _LOGGER.warning("response too large for op %s: %s", request.op, exc)
            return encode_response(
                rpc_error_envelope(
                    request.request_id, "internal_error", str(exc)
                )
            )

    def _handle_request(self, request: CheckerRequest) -> Mapping[str, Any]:
        """Per-request flow with split sync_lock / lake_lock.

        Phase 1 (under ``_sync_lock``): mirror the worker's tablet, apply
        per-node compile-cache invalidation. Sync writes the supervisor's
        ``Tablet/`` mirror and ``sync-fingerprints.json`` — both shared
        files that need serialization but unrelated to lake's
        single-process-per-workspace constraint.

        Phase 2 (no lock): try a cache-only response. The cache check
        helpers use fine-grained locks (``_oleans_lock``) for shared
        in-memory state and atomic file ops for sidecars, so this phase
        is safe to run concurrently with another thread's lake call.
        Returning a synthetic response here is the headline concurrency
        win — cache hits no longer queue behind a long lake invocation.

        Phase 3 (under ``_workspace_lock``): cache miss → run lake. The
        lake lock keeps the supervisor's lake invocation serial (lake
        is not multi-process-safe on a single workspace). Inside the
        lock we re-check the cache (a parallel thread may have populated
        it during our queue wait) before spending any lake time.

        Race protection: when sync runs concurrently with an in-flight
        lake call (because the two locks are now separate), the lake's
        compiled olean reflects the source state at the time lake read
        it, while the supervisor's on-disk source may have moved ahead
        by the time lake exits. ``_dispatch_op`` captures pre-lake
        source mtimes and only records nodes whose source mtime didn't
        change during lake — preventing the in-memory
        ``_oleans_known_current`` set from being seeded with stale
        olean→source mappings. See ``_dispatch_op``'s compile branch
        for the implementation.
        """
        if request.op == "ping":
            return self._op_ping(request)

        # Path containment is pure (no shared state); run before any locks
        # so a malformed-name request fails fast without contending on
        # sync_lock. The check is repeated inside ``_dispatch_op``'s
        # path-construction sites for defence in depth.
        self._assert_path_containment(request)

        # Phase 1: sync + cache invalidation under sync_lock.
        sync_lock_wait_started = time.monotonic_ns()
        with self._sync_lock:
            sync_lock_acquired = time.monotonic_ns()
            sync_result = sync_tablet_dir(
                self.worker_repo, self.supervisor_repo, self.fingerprint_cache_path
            )
            # Apply compile-cache invalidation INSIDE sync_lock so the
            # subsequent cache-only check (Phase 2) sees the post-sync
            # eviction state. ``_invalidate_compile_cache_for_sync`` takes
            # ``_oleans_lock`` internally; nesting sync_lock → oleans_lock
            # is consistent across all call sites (no other site takes
            # them in the reverse order).
            sync_changed = list(sync_result.get("changed", []) or [])
            sync_removed = list(sync_result.get("removed", []) or [])
            sync_rejected = list(sync_result.get("rejected", []) or [])
            if sync_changed or sync_removed or sync_rejected:
                self._invalidate_compile_cache_for_sync(
                    sync_changed + sync_removed + sync_rejected
                )
            sync_finished = time.monotonic_ns()
        sync_lock_wait_ms = int(
            (sync_lock_acquired - sync_lock_wait_started) / 1_000_000
        )

        # Phase 2: try cache-hit-only response (no workspace_lock).
        cache_hit = self._try_cache_hit_only(request, sync_result)
        if cache_hit is not None:
            response, returncode, op_log_extra = cache_hit
            self._emit_request_log(
                request=request,
                sync_result=sync_result,
                sync_started_ns=sync_lock_wait_started,
                sync_finished_ns=sync_finished,
                lake_started_ns=sync_finished,
                lake_finished_ns=sync_finished,
                lake_lock_wait_ms=0,
                sync_lock_wait_ms=sync_lock_wait_ms,
                returncode=returncode,
                op_log_extra=op_log_extra,
            )
            return response

        # Phase 3: cache miss → take workspace_lock for lake.
        lake_lock_wait_started = time.monotonic_ns()
        with self._workspace_lock:
            lake_lock_acquired = time.monotonic_ns()
            # Double-check cache: another thread may have completed a lake
            # build for the same request while we waited on the lock. Cheap
            # (~us) compared to a redundant lake call.
            cache_hit_recheck = self._try_cache_hit_only(request, sync_result)
            if cache_hit_recheck is not None:
                response, returncode, op_log_extra = cache_hit_recheck
                lake_finished = time.monotonic_ns()
            else:
                response, returncode, op_log_extra = self._dispatch_op(
                    request, sync_result
                )
                lake_finished = time.monotonic_ns()
        lake_lock_wait_ms = int(
            (lake_lock_acquired - lake_lock_wait_started) / 1_000_000
        )

        self._emit_request_log(
            request=request,
            sync_result=sync_result,
            sync_started_ns=sync_lock_wait_started,
            sync_finished_ns=sync_finished,
            lake_started_ns=lake_lock_acquired,
            lake_finished_ns=lake_finished,
            lake_lock_wait_ms=lake_lock_wait_ms,
            sync_lock_wait_ms=sync_lock_wait_ms,
            returncode=returncode,
            op_log_extra=op_log_extra,
        )
        return response

    def _emit_request_log(
        self,
        *,
        request: CheckerRequest,
        sync_result: Mapping[str, Any],
        sync_started_ns: int,
        sync_finished_ns: int,
        lake_started_ns: int,
        lake_finished_ns: int,
        lake_lock_wait_ms: int,
        sync_lock_wait_ms: int,
        returncode: Any,
        op_log_extra: Mapping[str, Any],
    ) -> None:
        """Emit one request-log record. Hoisted out of ``_handle_request``
        so the cache-hit fast path and the lake fallthrough share the same
        log shape."""
        log_extra: Dict[str, Any] = {
            "sync_duration_ms": int(
                (sync_finished_ns - sync_started_ns) / 1_000_000
            ),
            "lake_duration_ms": int(
                (lake_finished_ns - lake_started_ns) / 1_000_000
            ),
            "sync_rejected": len(sync_result.get("rejected", []) or []),
            "sync_removed": len(sync_result.get("removed", []) or []),
            "sync_lock_wait_ms": sync_lock_wait_ms,
            "lake_lock_wait_ms": lake_lock_wait_ms,
        }
        if request.node_name:
            log_extra["node"] = request.node_name
        if request.nodes:
            log_extra["nodes"] = list(request.nodes)
        if op_log_extra:
            log_extra.update(op_log_extra)
        self._request_log.emit(
            request_id=request.request_id,
            op=request.op,
            started_ns=sync_started_ns,
            finished_ns=lake_finished_ns,
            returncode=returncode,
            sync_changed_files=len(sync_result.get("changed", []) or []),
            **log_extra,
        )

    def _assert_path_containment(self, request: CheckerRequest) -> None:
        """Belt-and-braces path containment check (mitigation 2)."""
        if request.node_name:
            self._reject_if_escapes(request.node_name)
        for name in request.nodes:
            self._reject_if_escapes(name)

    def _reject_if_escapes(self, node_name: str) -> None:
        # The regex already guarantees no slashes / dots / nul bytes; this
        # call is the second line of defence — Path.resolve() will canonicalise
        # any future-introduced exotic input and surface the escape.
        if NODE_NAME_REGEX.fullmatch(node_name) is None:
            raise ProtocolError(
                "malformed_request",
                f"node_name no longer matches whitelist after sync: {node_name!r}",
            )
        tablet_dir = (self.supervisor_repo / "Tablet").resolve()
        candidate = (tablet_dir / f"{node_name}.lean").resolve()
        if not candidate.is_relative_to(tablet_dir):
            raise ProtocolError(
                "malformed_request",
                f"node_name resolves outside Tablet/: {node_name!r}",
            )

    def _dispatch_op(
        self,
        request: CheckerRequest,
        sync_result: Mapping[str, Any],
    ) -> Tuple[Mapping[str, Any], Any, Mapping[str, Any]]:
        """Run the appropriate observation function under
        ``bwrap_role="lake_compiler"``. Returns
        ``(response_payload, returncode, extra_log_fields)``.

        ``returncode`` is hoisted out for the request log; it is the lake
        process's exit code (or ``None`` for non-process ops). The third
        member is an op-specific dict of fields the request log should
        include (e.g. semantic-payload cache hit/miss counts); it is
        empty for ops that don't need it.

        Note: compile-cache invalidation for the sync diff happens in
        Phase 1 of ``_handle_request`` under ``_sync_lock``, BEFORE this
        function is called. The ``sync_result`` argument is retained for
        future use and for compatibility with tests that monkey-patch
        this method.
        """
        op = request.op
        rid = request.request_id
        no_extra: Mapping[str, Any] = {}

        if op in {"verify_node", "lean_compile_node"}:
            assert request.node_name is not None
            cached = self._maybe_synthesize_compile_hit(
                [request.node_name], include_compile_warning_fact=True
            )
            if cached is not None:
                cached["node"] = request.node_name
                return (
                    {"request_id": rid, **cached},
                    0,
                    {"compile_cache_hit": True, "compile_cache_nodes": 1},
                )
            # Race protection: capture pre-lake source mtimes. Because
            # sync (Phase 1) and lake (Phase 3) now hold separate locks,
            # a parallel sync from another thread can write a new source
            # version between when this lake started reading sources and
            # when it exits. Recording an in-memory "current" bit for a
            # node whose source moved during the build would let the next
            # cache check serve a stale olean (the disk's
            # ``olean.mtime >= src.mtime`` filter is fooled when sync
            # rewrote the source AFTER lake built the olean — the olean
            # is "newer" than the post-sync source, but the bytes were
            # compiled from the pre-sync source). Per-node mtime snapshot
            # is the cheapest mitigation: if the source mtime is unchanged
            # across the lake call, we know our olean genuinely reflects
            # the current source.
            pre_mtime_ns = _stat_mtime_ns(
                self.supervisor_repo / "Tablet" / f"{request.node_name}.lean"
            )
            payload = observations.compile_node(
                self.supervisor_repo,
                request.node_name,
                timeout_secs=request.timeout_secs,
                bwrap_role="lake_compiler",
            )
            if payload.get("returncode") == 0:
                post_mtime_ns = _stat_mtime_ns(
                    self.supervisor_repo / "Tablet" / f"{request.node_name}.lean"
                )
                if pre_mtime_ns is not None and pre_mtime_ns == post_mtime_ns:
                    self._record_oleans_built(
                        [request.node_name],
                        compile_warning_facts={
                            request.node_name: _compile_output_has_sorry_warning(
                                payload.get("stdout"), payload.get("stderr")
                            )
                        },
                    )
                # Else: a parallel sync changed the source during lake.
                # Don't record — the next cache check (post-sync's
                # invalidation in another thread) will route this node
                # back through lake at the correct source state.
            return (
                {"request_id": rid, **payload},
                payload.get("returncode"),
                {"compile_cache_hit": False, "compile_cache_nodes": 0},
            )

        if op == "materialize_oleans":
            requested = list(request.nodes)
            cached_set = self._compute_compile_cache_subset(requested)
            uncached = [name for name in requested if name not in cached_set]
            if not uncached:
                # Full hit: every requested node is already known-current.
                synth = self._maybe_synthesize_compile_hit(requested)
                if synth is not None:
                    return (
                        {"request_id": rid, **synth},
                        0,
                        {
                            "compile_cache_hit": True,
                            "compile_cache_nodes": len(requested),
                        },
                    )
                # Stat re-verification (inside _maybe_synthesize_compile_hit)
                # may have evicted entries between the two calls; fall
                # through to lake on the now-uncached set.
                cached_set = self._compute_compile_cache_subset(requested)
                uncached = [name for name in requested if name not in cached_set]
            # Race protection: capture pre-lake source mtimes for every
            # node we might record. Same rationale as the compile branch
            # above. We snapshot the FULL closure that lake will produce
            # (its ``materialized_nodes`` includes transitive deps), but
            # at this point we only know the nodes WE asked for; capture
            # those — the closure deps lake adds will be filtered by the
            # observation's own stat-walk and any whose source mtime
            # subsequently moves will be rejected at cache-check time
            # via the ``olean.mtime >= src.mtime`` gate. This pre/post
            # check tightens the previously-only-disk-state guard.
            pre_mtimes: Dict[str, Optional[int]] = {
                name: _stat_mtime_ns(
                    self.supervisor_repo / "Tablet" / f"{name}.lean"
                )
                for name in uncached
            }
            payload = observations.materialize_tablet_oleans(
                self.supervisor_repo,
                uncached,
                timeout_secs=request.timeout_secs,
                bwrap_role="lake_compiler",
            )
            if payload.get("returncode") == 0:
                # Record only nodes whose source mtime didn't move during
                # lake. lake's ``materialized_nodes`` already filters by
                # ``olean.mtime >= src.mtime``; this further filters out
                # nodes where a parallel sync rewrote the source mid-lake.
                built_by_lake_raw = list(
                    payload.get("materialized_nodes", []) or []
                )
                safe_to_record: list[str] = []
                for name in built_by_lake_raw:
                    pre_t = pre_mtimes.get(name)
                    if pre_t is None:
                        # Not in our snapshot (closure dep we didn't ask
                        # for); the observation's own stat-walk vouches
                        # for olean.mtime >= src.mtime, accept it.
                        safe_to_record.append(name)
                        continue
                    post_t = _stat_mtime_ns(
                        self.supervisor_repo / "Tablet" / f"{name}.lean"
                    )
                    if pre_t == post_t:
                        safe_to_record.append(name)
                self._record_oleans_built(safe_to_record)
            # Merge: lake-built ∪ cache hits. The lake response's
            # ``materialized_nodes`` carries the FULL closure of
            # ``uncached`` (deps lake had to compile to satisfy the
            # request); ``cached_set`` covers nodes the caller explicitly
            # named that were already current. Preserve lake's ordering
            # for the closure portion, then append any cached entries
            # lake didn't see (they're already in ``_oleans_known_current``).
            built_by_lake = list(payload.get("materialized_nodes", []) or [])
            built_by_lake_set = set(built_by_lake)
            merged_materialized: list[str] = list(built_by_lake)
            for name in requested:
                if name in cached_set and name not in built_by_lake_set:
                    merged_materialized.append(name)
            response_payload = dict(payload)
            response_payload["materialized_nodes"] = merged_materialized
            response_payload["requested_nodes"] = requested
            return (
                {"request_id": rid, **response_payload},
                payload.get("returncode"),
                {
                    "compile_cache_hit": bool(cached_set),
                    "compile_cache_nodes": len(cached_set),
                },
            )

        if op == "lean_semantic_payloads":
            return self._handle_lean_semantic_payloads(request)

        if op == "print_axioms":
            assert request.node_name is not None
            return self._handle_print_axioms(request)

        if op == "local_closure_axioms":
            assert request.node_name is not None
            return self._handle_local_closure_axioms(request)

        if op == "lean_build_tablet":
            payload = observations.build_tablet(
                self.supervisor_repo,
                timeout_secs=request.timeout_secs,
                bwrap_role="lake_compiler",
            )
            return ({"request_id": rid, **payload}, payload.get("returncode"), no_extra)

        if op == "prepare_compiled_support":
            # Short-circuit when we have already prepared this manifest in
            # this server lifetime. ``lake exe cache get`` is idempotent
            # w.r.t. ``lake-manifest.json`` (same manifest sha → identical
            # set of fetched oleans), and lake never garbage-collects them
            # under us; if a prior prepare for this manifest exited 0,
            # re-running it is a no-op that nonetheless takes ~13s on the
            # live mathlib hot cache. Cache key is supervisor-computed.
            current_manifest_sha = _sha256_file_or_empty(self._lake_manifest_path)
            if (
                current_manifest_sha
                and current_manifest_sha == self._last_successful_prepare_manifest
            ):
                synth = {
                    "steps_completed": ["cache_get"],
                    "returncode": 0,
                    "stdout": "",
                    "stderr": "",
                    "timed_out": False,
                    "spawn_error": "",
                }
                return (
                    {"request_id": rid, **synth},
                    0,
                    {"prepare_cache_hit": True},
                )
            payload = observations.prepare_compiled_support(
                self.supervisor_repo,
                timeout_secs=request.timeout_secs,
                bwrap_role="lake_compiler",
            )
            # Update the cache marker only on a clean success; failures
            # (timeout, spawn error, non-zero rc) must not become a
            # permanent skip — the next request will retry as today.
            if (
                current_manifest_sha
                and payload.get("returncode") == 0
                and not payload.get("timed_out")
                and not payload.get("spawn_error")
            ):
                self._last_successful_prepare_manifest = current_manifest_sha
            return (
                {"request_id": rid, **payload},
                payload.get("returncode"),
                {"prepare_cache_hit": False},
            )

        # ping is handled in _handle_request before lock acquisition.
        raise ProtocolError("unknown_op", f"dispatcher missing op {op!r}")

    # --------------------------- cache-hit-only fast path ---------------------------

    def _try_cache_hit_only(
        self,
        request: CheckerRequest,
        sync_result: Mapping[str, Any],
    ) -> Optional[Tuple[Mapping[str, Any], Any, Mapping[str, Any]]]:
        """Return a fully-synthesized response if the request can be
        satisfied entirely from cache, else ``None``.

        Side-effect-free with respect to lake (no ``observation.*`` call
        is invoked). Touches ``_oleans_known_current`` only via
        ``_compute_compile_cache_subset`` and ``_maybe_synthesize_compile_hit``,
        both of which use ``_oleans_lock`` for fine-grained synchronisation.
        Sidecar caches (semantic payloads, print axioms) are read via
        atomic file ops.

        Caller (``_handle_request``) invokes this in two places:
          1. After Phase 1 (sync + invalidation) but BEFORE acquiring
             ``_workspace_lock`` — the headline concurrency win, lets
             cache hits bypass the lake lock entirely.
          2. After acquiring ``_workspace_lock`` (double-check) — handles
             the rare race where another thread populated the cache while
             we were waiting on the lock.
        """
        op = request.op
        rid = request.request_id

        if op in {"verify_node", "lean_compile_node"}:
            assert request.node_name is not None
            cached = self._maybe_synthesize_compile_hit(
                [request.node_name], include_compile_warning_fact=True
            )
            if cached is None:
                return None
            cached["node"] = request.node_name
            return (
                {"request_id": rid, **cached},
                0,
                {"compile_cache_hit": True, "compile_cache_nodes": 1},
            )

        if op == "materialize_oleans":
            requested = list(request.nodes)
            cached_set = self._compute_compile_cache_subset(requested)
            if len(cached_set) != len(set(requested)):
                return None
            synth = self._maybe_synthesize_compile_hit(requested)
            if synth is None:
                return None
            return (
                {"request_id": rid, **synth},
                0,
                {
                    "compile_cache_hit": True,
                    "compile_cache_nodes": len(requested),
                },
            )

        if op == "print_axioms":
            assert request.node_name is not None
            return self._try_print_axioms_cache_hit(request)

        if op == "local_closure_axioms":
            assert request.node_name is not None
            return self._try_local_closure_axioms_cache_hit(request)

        if op == "lean_semantic_payloads":
            return self._try_lean_semantic_payloads_full_cache_hit(request)

        if op == "prepare_compiled_support":
            current_manifest_sha = _sha256_file_or_empty(self._lake_manifest_path)
            if (
                current_manifest_sha
                and current_manifest_sha == self._last_successful_prepare_manifest
            ):
                synth = {
                    "steps_completed": ["cache_get"],
                    "returncode": 0,
                    "stdout": "",
                    "stderr": "",
                    "timed_out": False,
                    "spawn_error": "",
                }
                return (
                    {"request_id": rid, **synth},
                    0,
                    {"prepare_cache_hit": True},
                )
            return None

        # ``lean_build_tablet`` has no per-request cache — always lake.
        return None

    def _try_print_axioms_cache_hit(
        self, request: CheckerRequest
    ) -> Optional[Tuple[Mapping[str, Any], Any, Mapping[str, Any]]]:
        """Cache-hit-only branch of ``_handle_print_axioms``: derive the
        cache key, attempt a load, return a synth response if it hits.
        Returns ``None`` on miss or unkeyed (caller falls through to lake).
        """
        rid = request.request_id
        node_name = request.node_name
        assert node_name is not None

        script_sha = _sha256_file_or_empty(self._fingerprint_script_path)
        toolchain_sha = _sha256_file_or_empty(self._toolchain_path)
        manifest_sha = _sha256_file_or_empty(self._lake_manifest_path)
        if not (script_sha and toolchain_sha):
            return None
        sync_cache = load_fingerprint_cache(self.fingerprint_cache_path)
        try:
            cache_key = compute_semantic_payload_cache_key(
                self.supervisor_repo,
                node_name,
                sync_cache,
                script_sha,
                toolchain_sha,
                manifest_sha,
                PRINT_AXIOMS_CACHE_VERSION,
            )
        except Exception:
            _LOGGER.exception(
                "compute_semantic_payload_cache_key failed for print_axioms %s",
                node_name,
            )
            return None
        if cache_key is None:
            return None

        try:
            sidecar = load_print_axioms(
                self.print_axioms_cache_dir,
                cache_key,
                expected_version=PRINT_AXIOMS_CACHE_VERSION,
            )
        except Exception:
            _LOGGER.exception(
                "load_print_axioms failed for %s/%s", node_name, cache_key
            )
            return None
        if sidecar is None:
            return None
        response = {
            "request_id": rid,
            "node": node_name,
            "returncode": sidecar.get("returncode"),
            "stdout": str(sidecar.get("stdout", "") or ""),
            "stderr": str(sidecar.get("stderr", "") or ""),
            "timed_out": bool(sidecar.get("timed_out", False)),
            "spawn_error": str(sidecar.get("spawn_error", "") or ""),
        }
        return (response, sidecar.get("returncode"), {"print_axioms_cache_hit": True})

    def _try_local_closure_axioms_cache_hit(
        self, request: CheckerRequest
    ) -> Optional[Tuple[Mapping[str, Any], Any, Mapping[str, Any]]]:
        """Cache-hit-only branch of ``_handle_local_closure_axioms``: derive
        the cache key, attempt a load, return a synth response if it hits.
        Returns ``None`` on miss or unkeyed (caller falls through to lake
        under ``_workspace_lock``). Pattern mirrors
        ``_try_print_axioms_cache_hit`` — both ops share the same closure-
        walked cache key surface and benefit equally from bypassing the
        workspace lock on a cache hit.
        """
        rid = request.request_id
        node_name = request.node_name
        assert node_name is not None

        script_sha = _sha256_file_or_empty(self._local_closure_script_path)
        toolchain_sha = _sha256_file_or_empty(self._toolchain_path)
        manifest_sha = _sha256_file_or_empty(self._lake_manifest_path)
        if not (script_sha and toolchain_sha):
            return None
        sync_cache = load_fingerprint_cache(self.fingerprint_cache_path)
        try:
            cache_key_base = compute_semantic_payload_cache_key(
                self.supervisor_repo,
                node_name,
                sync_cache,
                script_sha,
                toolchain_sha,
                manifest_sha,
                LOCAL_CLOSURE_AXIOMS_CACHE_VERSION,
            )
        except Exception:
            _LOGGER.exception(
                "compute_semantic_payload_cache_key failed for local_closure_axioms %s",
                node_name,
            )
            return None
        if cache_key_base is None:
            return None
        no_axcheck = bool(request.raw.get("no_axcheck", False))
        cache_key = f"{cache_key_base}-noax" if no_axcheck else cache_key_base

        try:
            sidecar = load_local_closure_axioms(
                self.local_closure_axioms_cache_dir,
                cache_key,
                expected_version=LOCAL_CLOSURE_AXIOMS_CACHE_VERSION,
            )
        except Exception:
            _LOGGER.exception(
                "load_local_closure_axioms failed for %s/%s", node_name, cache_key
            )
            return None
        if sidecar is None or not isinstance(sidecar.get("response"), dict):
            return None
        cached_response = dict(sidecar["response"])
        # Refresh the per-request request_id so the response echoes this
        # request's id, not the one stored.
        cached_response["request_id"] = rid
        return (
            cached_response,
            cached_response.get("returncode"),
            {"local_closure_axioms_cache_hit": True},
        )

    def _try_lean_semantic_payloads_full_cache_hit(
        self, request: CheckerRequest
    ) -> Optional[Tuple[Mapping[str, Any], Any, Mapping[str, Any]]]:
        """Cache-hit-only branch of ``_handle_lean_semantic_payloads``:
        every requested node must be served from the sidecar cache. If
        any node misses (or is unkeyed), return ``None`` so the caller
        runs the batched observation under the lake lock.
        """
        rid = request.request_id
        requested = list(request.nodes)
        if not requested:
            # Degenerate: empty request. Match the lake-path response shape.
            return (
                {"request_id": rid, "nodes": {}},
                None,
                {
                    "cache_hits": 0,
                    "cache_misses": 0,
                    "cache_unkeyed": 0,
                },
            )

        script_sha = _sha256_file_or_empty(self._fingerprint_script_path)
        toolchain_sha = _sha256_file_or_empty(self._toolchain_path)
        manifest_sha = _sha256_file_or_empty(self._lake_manifest_path)
        if not (script_sha and toolchain_sha):
            return None
        sync_cache = load_fingerprint_cache(self.fingerprint_cache_path)

        nodes_response: Dict[str, Dict[str, Any]] = {}
        for node_name in requested:
            try:
                cache_key = compute_semantic_payload_cache_key(
                    self.supervisor_repo,
                    node_name,
                    sync_cache,
                    script_sha,
                    toolchain_sha,
                    manifest_sha,
                    SEMANTIC_PAYLOAD_CACHE_VERSION,
                )
            except Exception:
                _LOGGER.exception(
                    "compute_semantic_payload_cache_key failed for %s",
                    node_name,
                )
                return None
            if cache_key is None:
                return None
            try:
                sidecar = load_semantic_payload(
                    self.semantic_payload_cache_dir,
                    cache_key,
                    expected_version=SEMANTIC_PAYLOAD_CACHE_VERSION,
                )
            except Exception:
                _LOGGER.exception(
                    "load_semantic_payload failed for %s/%s",
                    node_name,
                    cache_key,
                )
                return None
            if sidecar is None or not sidecar.get("ok"):
                return None
            nodes_response[node_name] = {
                "ok": True,
                "payload": str(sidecar.get("payload", "") or ""),
                "error": str(sidecar.get("error", "") or ""),
            }

        return (
            {"request_id": rid, "nodes": nodes_response},
            None,
            {
                "cache_hits": len(requested),
                "cache_misses": 0,
                "cache_unkeyed": 0,
            },
        )

    # --------------------------- compile-cache helpers ---------------------------

    def _compute_compile_cache_subset(
        self, node_names: Sequence[str]
    ) -> set[str]:
        """Return the subset of ``node_names`` whose oleans are known-current.

        Three-step safety check (mirrors ``_maybe_synthesize_compile_hit``):
          1. The node is in ``_oleans_known_current`` (this server has
             previously confirmed lake exit 0 for it since the last sync
             diff that touched its closure).
          2. The expected ``.olean`` exists on disk and is non-empty.
          3. The olean's mtime is at least the source's mtime.

        Any node failing 2 or 3 is evicted from ``_oleans_known_current``
        and excluded from the returned set so the caller routes it back
        through lake.

        Used both by the full-hit synthesizer (when the subset equals the
        input) and by ``materialize_oleans`` to skip already-current nodes
        from the lake invocation list.
        """
        names = list(node_names)
        if not names:
            return set()
        with self._oleans_lock:
            candidate = {name for name in names if name in self._oleans_known_current}
        if not candidate:
            return set()
        evicted: list[str] = []
        valid: set[str] = set()
        for name in names:
            if name not in candidate:
                continue
            src = self.supervisor_repo / "Tablet" / f"{name}.lean"
            olean = (
                self.supervisor_repo
                / ".lake"
                / "build"
                / "lib"
                / "lean"
                / "Tablet"
                / f"{name}.olean"
            )
            try:
                src_stat = src.stat()
                olean_stat = olean.stat()
            except (FileNotFoundError, OSError):
                evicted.append(name)
                continue
            if olean_stat.st_size == 0 or olean_stat.st_mtime < src_stat.st_mtime:
                evicted.append(name)
                continue
            valid.add(name)
        if evicted:
            with self._oleans_lock:
                for name in evicted:
                    self._oleans_known_current.discard(name)
                    self._compile_has_sorry_warning.pop(name, None)
        return valid

    def _maybe_synthesize_compile_hit(
        self,
        node_names: Sequence[str],
        *,
        include_compile_warning_fact: bool = False,
    ) -> Optional[Dict[str, Any]]:
        """Return a synthetic success payload if it is provably safe to skip
        lake for *every* requested node, else ``None``.

        Thin wrapper over ``_compute_compile_cache_subset``: the subset must
        cover every requested node. Any miss (in-memory bit absent, olean
        missing/empty, mtime stale) triggers eviction inside the subset
        helper and returns ``None`` so the caller falls through to lake.

        ``include_compile_warning_fact`` is required for
        ``lean_compile_node``/``verify_node`` responses, whose stdout/stderr
        feed Rust validity checks. In that mode every requested node must
        have a cached Lean-derived sorry-warning fact; missing means unknown,
        so the caller must fall through to lake. ``materialize_oleans`` does
        not consume stdout/stderr semantically and may still use olean-only
        hits.
        """
        names = list(node_names)
        if not names:
            return None
        cached = self._compute_compile_cache_subset(names)
        if len(cached) != len(set(names)):
            return None
        has_sorry_warning = False
        if include_compile_warning_fact:
            with self._oleans_lock:
                warning_facts: Dict[str, bool] = {}
                for name in names:
                    fact = self._compile_has_sorry_warning.get(name)
                    if fact is None:
                        return None
                    warning_facts[name] = fact
            has_sorry_warning = any(warning_facts.values())
        return {
            "requested_nodes": names,
            "materialized_nodes": names,
            "returncode": 0,
            "stdout": (
                "warning: cached compile result: declaration uses 'sorry'\n"
                if has_sorry_warning
                else ""
            ),
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    def _record_oleans_built(
        self,
        node_names: Sequence[str],
        *,
        compile_warning_facts: Optional[Mapping[str, bool]] = None,
    ) -> None:
        """Mark each name as having a known-current olean for this server's
        lifetime. Called after a successful lake invocation that exited 0.

        ``compile_warning_facts`` is only supplied by real
        ``lean_compile_node`` calls. Batched materialization can prove oleans
        current, but it does not provide a per-request compile observation
        whose warnings may be synthesized later.
        """
        if not node_names:
            return
        facts = compile_warning_facts or {}
        with self._oleans_lock:
            for name in node_names:
                if name:
                    self._oleans_known_current.add(name)
                    if name in facts:
                        self._compile_has_sorry_warning[name] = bool(facts[name])

    def _invalidate_compile_cache_for_sync(
        self, changed_paths: Sequence[str]
    ) -> None:
        """Drop only the nodes whose source moved + everything that
        transitively imports them. For pure deletions of an unimported
        orphan: nothing else is invalidated.

        ``sync_tablet_dir`` returns paths *relative to* ``Tablet/`` (e.g.
        ``"Foo.lean"``, not ``"Tablet/Foo.lean"``). Subdirectories under
        ``Tablet/`` would surface as e.g. ``"sub/Foo.lean"`` — we don't
        currently model nested layouts so we full-wipe those.

        Conservative fallback (full cache wipe) when we can't safely
        reason per-node:
          - Path contains ``/`` (nested layout we don't model)
          - Path lacks an extension or has a node-name that fails the
            regex (unexpected shape)
        """
        # Map relative-to-Tablet paths to node names; bail to full wipe
        # on anything weird.
        affected: set[str] = set()
        for raw_path in changed_paths:
            path = str(raw_path).strip()
            if not path:
                continue
            # Defensive: in case a future caller passes Tablet/-prefixed
            # paths, strip the prefix transparently. sync_tablet_dir
            # produces relative-to-Tablet, but other callers may not.
            if path.startswith("Tablet/"):
                path = path[len("Tablet/"):]
            if "/" in path:
                with self._oleans_lock:
                    self._oleans_known_current.clear()
                    self._compile_has_sorry_warning.clear()
                return
            if "." not in path:
                with self._oleans_lock:
                    self._oleans_known_current.clear()
                    self._compile_has_sorry_warning.clear()
                return
            node_name = path.rsplit(".", 1)[0]
            if not NODE_NAME_REGEX.fullmatch(node_name):
                with self._oleans_lock:
                    self._oleans_known_current.clear()
                    self._compile_has_sorry_warning.clear()
                return
            affected.add(node_name)

        if not affected:
            return

        # Walk reverse-import closure: any node that transitively imports
        # an affected node has a stale olean too.
        rev_imports = self._compute_reverse_import_graph()
        closure: set[str] = set(affected)
        frontier = list(affected)
        while frontier:
            n = frontier.pop()
            for consumer in rev_imports.get(n, ()):
                if consumer not in closure:
                    closure.add(consumer)
                    frontier.append(consumer)

        with self._oleans_lock:
            self._oleans_known_current -= closure
            for name in closure:
                self._compile_has_sorry_warning.pop(name, None)

    _IMPORT_LINE_RE = re.compile(r"^\s*import\s+Tablet\.([A-Za-z][A-Za-z0-9_]*)")

    def _compute_reverse_import_graph(self) -> Dict[str, set[str]]:
        """Scan ``<supervisor_repo>/Tablet/*.lean`` for ``import Tablet.X``
        lines and return a map from imported-node → set of importer nodes.

        Recomputed on every invalidation; the cost is dominated by ~32
        small file reads (single-digit ms) which is well below the
        per-op compile time we save.
        """
        tablet_dir = self.supervisor_repo / "Tablet"
        rev: Dict[str, set[str]] = {}
        try:
            files = list(tablet_dir.glob("*.lean"))
        except OSError:
            return rev
        for lean_file in files:
            consumer = lean_file.stem
            if not NODE_NAME_REGEX.fullmatch(consumer):
                continue
            try:
                text = lean_file.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            for line in text.splitlines():
                m = self._IMPORT_LINE_RE.match(line)
                if not m:
                    continue
                imported = m.group(1)
                rev.setdefault(imported, set()).add(consumer)
        return rev

    def _handle_print_axioms(
        self, request: CheckerRequest
    ) -> Tuple[Mapping[str, Any], Any, Mapping[str, Any]]:
        """Handle ``print_axioms`` with a per-node sidecar cache.

        The cache key reuses ``compute_semantic_payload_cache_key`` (same
        closure-walked surface: source shas, olean shas, toolchain pin,
        lake manifest, fingerprint script) — both ops want to be invalidated
        on the same fingerprint changes. The persisted record schema is
        ``{cache_version, node_name, key_blob_sha256, returncode, stdout,
        stderr, timed_out, spawn_error}``.

        Caching policy: only successes (returncode == 0, not timed_out, no
        spawn_error) are persisted. Failures are inherently transient (a
        sandbox blip, a timeout, a missing olean that triggers a
        rebuild-and-retry cycle) and must not become permanent state.

        Cache-key derivation can fail (returns ``None``) when the olean
        closure isn't fingerprinted yet, when an expected olean is absent,
        or when the toolchain/manifest sha is unreadable. In that case
        we fall through to lake without storing — same policy as the
        ``cache_unkeyed`` branch in ``_handle_lean_semantic_payloads``.
        """
        rid = request.request_id
        node_name = request.node_name
        assert node_name is not None

        script_sha = _sha256_file_or_empty(self._fingerprint_script_path)
        toolchain_sha = _sha256_file_or_empty(self._toolchain_path)
        manifest_sha = _sha256_file_or_empty(self._lake_manifest_path)
        sync_cache = load_fingerprint_cache(self.fingerprint_cache_path)

        cache_key: Optional[str] = None
        if script_sha and toolchain_sha:
            try:
                cache_key = compute_semantic_payload_cache_key(
                    self.supervisor_repo,
                    node_name,
                    sync_cache,
                    script_sha,
                    toolchain_sha,
                    manifest_sha,
                    PRINT_AXIOMS_CACHE_VERSION,
                )
            except Exception:
                _LOGGER.exception(
                    "compute_semantic_payload_cache_key failed for print_axioms %s",
                    node_name,
                )
                cache_key = None

        if cache_key is not None:
            try:
                sidecar = load_print_axioms(
                    self.print_axioms_cache_dir,
                    cache_key,
                    expected_version=PRINT_AXIOMS_CACHE_VERSION,
                )
            except Exception:
                _LOGGER.exception(
                    "load_print_axioms failed for %s/%s", node_name, cache_key
                )
                sidecar = None
            if sidecar is not None:
                response = {
                    "request_id": rid,
                    "node": node_name,
                    "returncode": sidecar.get("returncode"),
                    "stdout": str(sidecar.get("stdout", "") or ""),
                    "stderr": str(sidecar.get("stderr", "") or ""),
                    "timed_out": bool(sidecar.get("timed_out", False)),
                    "spawn_error": str(sidecar.get("spawn_error", "") or ""),
                }
                return (
                    response,
                    sidecar.get("returncode"),
                    {"print_axioms_cache_hit": True},
                )

        payload = observations.print_axioms(
            self.supervisor_repo,
            node_name,
            timeout_secs=request.timeout_secs,
            bwrap_role="lake_compiler",
        )
        # Cache only clean successes — caching a timeout/spawn/sandbox
        # failure would persist a transient glitch into subsequent
        # requests. The next live call will retry the same key.
        # Race protection: re-derive the cache key after the lake call.
        # Sync (Phase 1 of another thread's request) and lake (Phase 3 of
        # this request) hold separate locks under the unified-checker
        # concurrency restructure, so a parallel sync may have rewritten
        # supervisor sources during this lake. Re-deriving and comparing
        # to the pre-lake key catches that case: the lake's output is for
        # the post-lake source state, not the pre-lake key. Storing under
        # the pre-lake key when it doesn't match the post-lake key would
        # make subsequent lookups for the pre-lake key serve a result for
        # a different source revision. Only persist when the key is
        # stable across the lake call.
        if (
            cache_key is not None
            and payload.get("returncode") == 0
            and not payload.get("timed_out")
            and not payload.get("spawn_error")
        ):
            sync_cache_post = load_fingerprint_cache(self.fingerprint_cache_path)
            try:
                cache_key_post = compute_semantic_payload_cache_key(
                    self.supervisor_repo,
                    node_name,
                    sync_cache_post,
                    script_sha,
                    toolchain_sha,
                    manifest_sha,
                    PRINT_AXIOMS_CACHE_VERSION,
                )
            except Exception:
                cache_key_post = None
            if cache_key_post == cache_key:
                try:
                    store_print_axioms(
                        self.print_axioms_cache_dir,
                        cache_key,
                        node_name=node_name,
                        returncode=payload.get("returncode"),
                        stdout=str(payload.get("stdout", "") or ""),
                        stderr=str(payload.get("stderr", "") or ""),
                        timed_out=bool(payload.get("timed_out", False)),
                        spawn_error=str(payload.get("spawn_error", "") or ""),
                        cache_version=PRINT_AXIOMS_CACHE_VERSION,
                    )
                except Exception:
                    _LOGGER.exception(
                        "store_print_axioms failed for %s/%s", node_name, cache_key
                    )

        return (
            {"request_id": rid, **payload},
            payload.get("returncode"),
            {"print_axioms_cache_hit": False},
        )

    def _handle_local_closure_axioms(
        self, request: CheckerRequest
    ) -> Tuple[Mapping[str, Any], Any, Mapping[str, Any]]:
        """Handle ``local_closure_axioms`` (LOCAL_CLOSURE_IMPL_PLAN.md §5.5).

        Patch A: additive observation only. The handler invokes
        ``scripts/lean_local_closure.lean`` under bwrap'd lake against the
        supervisor repo, parses the script's JSON envelope on stdout, and
        surfaces both the structured closure data and the transport-level
        ``returncode``/``timed_out``/``stdout``/``stderr`` to the caller.
        Gating, persistence, and policy live in the Rust kernel (Patch B
        / Patch C); this handler does not interpret the closure data.

        Caching: per-key sidecar cache (Patch C deferred work, landed in
        commit e54a52b). The cache-hit fast path runs in
        ``_try_cache_hit_only`` BEFORE this handler acquires
        ``_workspace_lock`` — concurrent workers see cache hits without
        waiting on each other's lake calls. The cache-miss fall-through
        runs lake here and persists clean successes (status=="ok",
        returncode==0, !timed_out, !spawn_error) post-call with
        race-protected key re-derivation. The lock-acquired path also
        does a double-check load to catch the rare race where another
        thread populated the cache while this request was queued on the
        lock.

        Materialization: this handler does NOT itself invoke
        ``materialize_oleans``. It mirrors ``_handle_print_axioms``: the
        kernel-side caller (Patch A's ``run_local_closure_axioms``
        wrapper, plus the existing
        ``ensure_worker_checker_support_available`` precondition path)
        issues the materialization op separately before invoking this
        op. Path containment was already enforced upstream in
        ``_handle_request`` via ``_assert_path_containment``.

        Concurrency: runs under the same ``_workspace_lock`` as every
        other lake-spawning op; this handler is invoked from
        ``_dispatch_op`` which is called inside that lock.
        """
        rid = request.request_id
        node_name = request.node_name
        assert node_name is not None

        script_path = self._local_closure_script_path
        if not script_path.exists():
            return (
                {
                    "request_id": rid,
                    "node": node_name,
                    "returncode": None,
                    "stdout": "",
                    "stderr": (
                        f"local-closure script not found: {script_path}"
                    ),
                    "timed_out": False,
                    "spawn_error": (
                        f"local-closure script not found: {script_path}"
                    ),
                    "status": "internal_error",
                    "root_kind": "other",
                    "kernel_axioms": [],
                    "boundary_theorems": [],
                    "strict_theorem_deps": [],
                    "strict_definition_deps": [],
                    "errors": [
                        f"local-closure script not found: {script_path}"
                    ],
                },
                None,
                {"local_closure_script_missing": True},
            )

        # Match the ``observe_lean_semantic_payloads`` invocation shape:
        # ``lake env lean --run <script> <node>``. The script's main entry
        # point is ``def main (args : List String) : IO UInt32`` so it
        # is run directly via ``--run`` rather than elaborated as a
        # library file.
        #
        # Plan §4.6.1 kill-switch: pass ``--no-axcheck`` to the script
        # when the caller set ``no_axcheck`` on the request envelope.
        # The script then skips the secondary axiomization collector and
        # emits ``axiomization_check: {skipped: true, agreed: true}``;
        # the Rust wrapper accepts the (skipped) cross-check trivially.
        no_axcheck = bool(request.raw.get("no_axcheck", False))
        script_args = [str(script_path), node_name]
        if no_axcheck:
            script_args.append("--no-axcheck")

        # Patch C deferred cache (LOCAL_CLOSURE_IMPL_PLAN.md §5.5): same
        # closure-walked cache key as print_axioms, but with the local-
        # closure script's sha as ``script_sha256`` (so script edits
        # invalidate the cache). Cache key is suffixed with the
        # no_axcheck flag so the two axcheck-enabled-vs-disabled outputs
        # are stored separately. Cache miss falls through to the live
        # probe below.
        local_closure_script_sha = _sha256_file_or_empty(script_path)
        toolchain_sha = _sha256_file_or_empty(self._toolchain_path)
        manifest_sha = _sha256_file_or_empty(self._lake_manifest_path)
        sync_cache = load_fingerprint_cache(self.fingerprint_cache_path)
        cache_key: Optional[str] = None
        if local_closure_script_sha and toolchain_sha:
            try:
                cache_key_base = compute_semantic_payload_cache_key(
                    self.supervisor_repo,
                    node_name,
                    sync_cache,
                    local_closure_script_sha,
                    toolchain_sha,
                    manifest_sha,
                    LOCAL_CLOSURE_AXIOMS_CACHE_VERSION,
                )
            except Exception:
                _LOGGER.exception(
                    "compute_semantic_payload_cache_key failed for local_closure_axioms %s",
                    node_name,
                )
                cache_key_base = None
            if cache_key_base is not None:
                cache_key = (
                    f"{cache_key_base}-noax" if no_axcheck else cache_key_base
                )

        if cache_key is not None:
            try:
                sidecar = load_local_closure_axioms(
                    self.local_closure_axioms_cache_dir,
                    cache_key,
                    expected_version=LOCAL_CLOSURE_AXIOMS_CACHE_VERSION,
                )
            except Exception:
                _LOGGER.exception(
                    "load_local_closure_axioms failed for %s/%s",
                    node_name,
                    cache_key,
                )
                sidecar = None
            if sidecar is not None and isinstance(sidecar.get("response"), dict):
                cached_response = dict(sidecar["response"])
                # Refresh the per-request request_id so the response
                # echoes this request's id, not the one cached.
                cached_response["request_id"] = rid
                return (
                    cached_response,
                    cached_response.get("returncode"),
                    {"local_closure_axioms_cache_hit": True},
                )

        payload = observations._run_lake_command(
            self.supervisor_repo,
            ["env", "lean", "--run", *script_args],
            timeout_secs=request.timeout_secs,
            bwrap_role="lake_compiler",
        )

        stdout = str(payload.get("stdout", "") or "")
        stderr = str(payload.get("stderr", "") or "")
        returncode = payload.get("returncode")
        timed_out = bool(payload.get("timed_out", False))
        spawn_error = str(payload.get("spawn_error", "") or "")

        # Parse the script's JSON envelope. Surface a structured
        # parse-failure response (rather than raising) so the kernel
        # caller can treat it as a probe-internal error and either
        # retry or fail closed per Patch B/C policy.
        parsed: Optional[Mapping[str, Any]] = None
        parse_error: str = ""
        if stdout.strip():
            # The script emits exactly one JSON line on stdout via
            # ``IO.println (... .compress)``. Take the last
            # non-empty line so any incidental Lean stderr/info that
            # leaks to stdout (compiler warnings, etc.) doesn't shadow
            # the payload.
            candidate_line: Optional[str] = None
            for line in reversed(stdout.splitlines()):
                if line.strip():
                    candidate_line = line.strip()
                    break
            if candidate_line is not None:
                try:
                    decoded = json.loads(candidate_line)
                except json.JSONDecodeError as exc:
                    parse_error = (
                        f"local-closure script stdout is not valid JSON: {exc.msg}"
                    )
                else:
                    if not isinstance(decoded, dict):
                        parse_error = (
                            "local-closure script JSON is not an object: "
                            f"{type(decoded).__name__}"
                        )
                    else:
                        parsed = decoded

        # Compose the response envelope. Preserve the script's fields
        # verbatim when the parse succeeded; otherwise fall back to a
        # structured ``internal_error`` envelope so the response shape
        # is stable regardless of probe outcome.
        if parsed is not None:
            response: Dict[str, Any] = {
                "request_id": rid,
                "node": node_name,
                "returncode": returncode,
                "stdout": stdout,
                "stderr": stderr,
                "timed_out": timed_out,
                "spawn_error": spawn_error,
                # Script fields preserved verbatim per plan §5.3.
                "status": str(parsed.get("status", "internal_error") or "internal_error"),
                "root_kind": str(parsed.get("root_kind", "other") or "other"),
                "kernel_axioms": list(parsed.get("kernel_axioms", []) or []),
                "boundary_theorems": list(parsed.get("boundary_theorems", []) or []),
                "strict_theorem_deps": list(parsed.get("strict_theorem_deps", []) or []),
                "strict_definition_deps": list(
                    parsed.get("strict_definition_deps", []) or []
                ),
                "errors": list(parsed.get("errors", []) or []),
            }
            # Plan §4.6.1 dual-collector cross-check: forward the
            # `axiomization_check` sub-object verbatim (it is a JSON
            # object emitted by the merged Lean script). Pre-merge
            # scripts omit the field; pass through the absence so the
            # Rust wrapper's `Option<AxiomizationCheckOutput>` deserializes
            # as `None` (the wrapper treats `None` as "trust primary").
            axcheck = parsed.get("axiomization_check")
            if axcheck is not None:
                response["axiomization_check"] = axcheck
        else:
            errors: list[str] = []
            if parse_error:
                errors.append(parse_error)
            elif timed_out:
                errors.append("local-closure probe timed out before emitting JSON")
            elif spawn_error:
                errors.append(f"local-closure probe spawn error: {spawn_error}")
            elif returncode != 0:
                errors.append(
                    f"local-closure probe exited with returncode={returncode}"
                )
            else:
                errors.append("local-closure probe produced no stdout")
            response = {
                "request_id": rid,
                "node": node_name,
                "returncode": returncode,
                "stdout": stdout,
                "stderr": stderr,
                "timed_out": timed_out,
                "spawn_error": spawn_error,
                "status": "internal_error",
                "root_kind": "other",
                "kernel_axioms": [],
                "boundary_theorems": [],
                "strict_theorem_deps": [],
                "strict_definition_deps": [],
                "errors": errors,
            }

        log_extra: Dict[str, Any] = {
            "local_closure_status": response["status"],
            "local_closure_kernel_axioms": len(response["kernel_axioms"]),
            "local_closure_boundary_theorems": len(response["boundary_theorems"]),
            "local_closure_strict_theorem_deps": len(response["strict_theorem_deps"]),
            "local_closure_strict_definition_deps": len(
                response["strict_definition_deps"]
            ),
            "local_closure_errors": len(response["errors"]),
            "local_closure_axioms_cache_hit": False,
        }

        # Patch C deferred cache: persist clean successes only. Failures
        # (timeout, spawn error, non-zero returncode, status!="ok") are
        # transient — never store them. Mirror print_axioms's policy at
        # server.py:1853-1856.
        #
        # Race protection: re-derive the cache key after the lake call.
        # If a parallel sync rewrote supervisor sources during the lake
        # run, the post-lake key differs from the pre-lake key. Storing
        # under the pre-lake key when it doesn't match the post-lake key
        # would serve a stale result for a different source revision.
        # Only persist when the key is stable across the lake call.
        if (
            cache_key is not None
            and returncode == 0
            and not timed_out
            and not spawn_error
            and response.get("status") == "ok"
        ):
            sync_cache_post = load_fingerprint_cache(self.fingerprint_cache_path)
            try:
                cache_key_base_post = compute_semantic_payload_cache_key(
                    self.supervisor_repo,
                    node_name,
                    sync_cache_post,
                    local_closure_script_sha,
                    toolchain_sha,
                    manifest_sha,
                    LOCAL_CLOSURE_AXIOMS_CACHE_VERSION,
                )
            except Exception:
                cache_key_base_post = None
            cache_key_post = (
                f"{cache_key_base_post}-noax"
                if cache_key_base_post is not None and no_axcheck
                else cache_key_base_post
            )
            if cache_key_post == cache_key:
                try:
                    store_local_closure_axioms(
                        self.local_closure_axioms_cache_dir,
                        cache_key,
                        node_name=node_name,
                        response=response,
                        cache_version=LOCAL_CLOSURE_AXIOMS_CACHE_VERSION,
                    )
                except Exception:
                    _LOGGER.exception(
                        "store_local_closure_axioms failed for %s/%s",
                        node_name,
                        cache_key,
                    )

        return (response, returncode, log_extra)

    def _handle_lean_semantic_payloads(
        self, request: CheckerRequest
    ) -> Tuple[Mapping[str, Any], Any, Mapping[str, Any]]:
        """Handle ``lean_semantic_payloads`` with the per-node sidecar cache.

        For each requested node we:

        1. Derive a content-addressed cache key from the supervisor-side
           lean source + olean closure, the fingerprint script, the
           lean-toolchain pin, and the lake-manifest sha.
        2. Try to load the sidecar at ``checker-state/semantic-payloads/<key>.json``.
        3. Treat the request as ``cache_unkeyed`` (key derivation returned
           ``None``) when sync hasn't fingerprinted the closure yet, when
           an expected ``.olean`` is missing, or when the toolchain /
           manifest sha is unreadable. The lean call still runs through
           the live lake path — only the cache step is bypassed.

        Cache misses are batched into a single
        ``observe_lean_semantic_payloads`` call so the per-process Lean
        startup cost is paid once for the request, not once per node.
        Successes are persisted; failures are NOT cached (a transient
        sandbox or timeout error must not become permanent state).
        """
        rid = request.request_id
        requested = list(request.nodes)

        # Pre-hash the cache-key inputs that don't vary per-node. Tiny
        # files (~1 KiB toolchain pin, ~5 KiB script, ~10 KiB lake
        # manifest) so per-request rehash is cheaper than guarding a
        # refresh policy. Manifest sha pins the mathlib rev: ``lake
        # update`` rewrites lake-manifest.json without touching
        # ``lean-toolchain``, so it has to be its own input.
        script_sha = _sha256_file_or_empty(self._fingerprint_script_path)
        toolchain_sha = _sha256_file_or_empty(self._toolchain_path)
        manifest_sha = _sha256_file_or_empty(self._lake_manifest_path)

        # Read the latest sync-fingerprint cache so the closure walk's
        # per-node sha values reflect the just-completed sync_tablet_dir
        # call. ``_handle_request`` ran sync immediately before invoking
        # us, so the on-disk cache is fresh.
        sync_cache = load_fingerprint_cache(self.fingerprint_cache_path)

        nodes_response: Dict[str, Dict[str, Any]] = {}
        misses: list[str] = []
        miss_keys: Dict[str, str] = {}
        cache_hits = 0
        # Renamed from cache_skipped: "skipped" was misleading because
        # the lean call still runs in this branch — only the cache step
        # is bypassed. "unkeyed" labels the underlying state: we couldn't
        # derive a key for this node (missing fingerprint, missing
        # olean, missing toolchain/manifest sha, etc.).
        cache_unkeyed = 0

        for node_name in requested:
            cache_key: Optional[str] = None
            # Defensive: guard cache-key derivation. A bug here must
            # degrade gracefully into a cache-miss path, never into a
            # request-level error.
            if script_sha and toolchain_sha:
                try:
                    cache_key = compute_semantic_payload_cache_key(
                        self.supervisor_repo,
                        node_name,
                        sync_cache,
                        script_sha,
                        toolchain_sha,
                        manifest_sha,
                        SEMANTIC_PAYLOAD_CACHE_VERSION,
                    )
                except Exception:
                    _LOGGER.exception(
                        "compute_semantic_payload_cache_key failed for %s",
                        node_name,
                    )
                    cache_key = None

            if cache_key is None:
                cache_unkeyed += 1
                misses.append(node_name)
                continue

            try:
                sidecar = load_semantic_payload(
                    self.semantic_payload_cache_dir,
                    cache_key,
                    expected_version=SEMANTIC_PAYLOAD_CACHE_VERSION,
                )
            except Exception:
                _LOGGER.exception(
                    "load_semantic_payload failed for %s/%s",
                    node_name,
                    cache_key,
                )
                sidecar = None

            if sidecar is not None and sidecar.get("ok"):
                cache_hits += 1
                nodes_response[node_name] = {
                    "ok": True,
                    "payload": str(sidecar.get("payload", "") or ""),
                    "error": str(sidecar.get("error", "") or ""),
                }
                continue

            misses.append(node_name)
            miss_keys[node_name] = cache_key

        if misses:
            fresh = observations.observe_lean_semantic_payloads(
                self.supervisor_repo,
                misses,
                timeout_secs=request.timeout_secs,
                bwrap_role="lake_compiler",
            )
            # Race protection: re-load the sync-fingerprint cache after
            # the lake call so we can re-derive each node's cache key.
            # Sync (Phase 1 of another thread's request) may have written
            # supervisor sources during this lake — the lake observation
            # ran on the post-sync state, so storing under the pre-sync
            # key would cache the post-sync output for a key that
            # represents pre-sync content. Re-derive and only persist if
            # the key is stable across the lake call.
            sync_cache_post = load_fingerprint_cache(self.fingerprint_cache_path)
            for node_name in misses:
                entry = dict(fresh.get(node_name) or {"ok": False, "payload": "", "error": ""})
                nodes_response[node_name] = {
                    "ok": bool(entry.get("ok", False)),
                    "payload": str(entry.get("payload", "") or ""),
                    "error": str(entry.get("error", "") or ""),
                }
                # Cache only successes — caching a timeout / spawn / sandbox
                # failure would persist a transient sandbox glitch into
                # subsequent requests. The next live call will retry the
                # same key, which is the right policy for those errors.
                if not entry.get("ok"):
                    continue
                key = miss_keys.get(node_name)
                if key is None:
                    continue
                try:
                    key_post = compute_semantic_payload_cache_key(
                        self.supervisor_repo,
                        node_name,
                        sync_cache_post,
                        script_sha,
                        toolchain_sha,
                        manifest_sha,
                        SEMANTIC_PAYLOAD_CACHE_VERSION,
                    )
                except Exception:
                    key_post = None
                if key_post != key:
                    # Source state moved during lake. The fresh result
                    # is for the new state; storing it under the old key
                    # would corrupt subsequent lookups. Skip.
                    continue
                try:
                    store_semantic_payload(
                        self.semantic_payload_cache_dir,
                        key,
                        node_name=node_name,
                        ok=True,
                        payload=str(entry.get("payload", "") or ""),
                        error=str(entry.get("error", "") or ""),
                        cache_version=SEMANTIC_PAYLOAD_CACHE_VERSION,
                    )
                except Exception:
                    _LOGGER.exception(
                        "store_semantic_payload failed for %s/%s",
                        node_name,
                        key,
                    )

        log_extra = {
            "cache_hits": cache_hits,
            "cache_misses": len(miss_keys),
            "cache_unkeyed": cache_unkeyed,
        }
        return (
            {"request_id": rid, "nodes": nodes_response},
            None,
            log_extra,
        )

    def _op_ping(self, request: CheckerRequest) -> Mapping[str, Any]:
        uptime_secs = (time.monotonic_ns() - self._started_at_ns) / 1_000_000_000
        return {
            "request_id": request.request_id,
            "pong": True,
            "server_pid": os.getpid(),
            "uptime_secs": round(uptime_secs, 3),
            "supervisor_repo": str(self.supervisor_repo),
            "worker_repo": str(self.worker_repo),
        }


def _read_pid_file_quietly(fd: int) -> Optional[int]:
    """Read the integer PID from an open file descriptor without raising.

    Used by the singleton-error path: we need the rival's PID for the
    error message but already failed to take the lock, so any I/O issue
    here is informational at best.
    """
    try:
        os.lseek(fd, 0, os.SEEK_SET)
        data = os.read(fd, 64)
    except OSError:
        return None
    text = data.decode("utf-8", errors="replace").strip()
    if not text:
        return None
    try:
        return int(text.split()[0])
    except (ValueError, IndexError):
        return None


def _read_parallelism() -> int:
    raw = os.environ.get("TRELLIS_LEAN_PARALLELISM", "").strip()
    if not raw:
        return DEFAULT_PARALLELISM
    try:
        value = int(raw)
    except ValueError:
        return DEFAULT_PARALLELISM
    if value < 1:
        return DEFAULT_PARALLELISM
    return value


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        prog="trellis.checker.server",
        description="Unified-checker UNIX-socket dispatcher (Step 1 scaffolding).",
    )
    parser.add_argument(
        "runtime_root",
        type=Path,
        help="Path to the runtime root, e.g. <repo>/.trellis/runtime/<name>.",
    )
    parser.add_argument(
        "--peer-uid",
        type=int,
        default=None,
        help="Override expected SO_PEERCRED uid (default: server's own euid).",
    )
    parser.add_argument(
        "--parallelism",
        type=int,
        default=None,
        help="Override TRELLIS_LEAN_PARALLELISM thread-pool size.",
    )
    parser.add_argument(
        "--log-level",
        default="INFO",
        help="Python logging level (default: INFO).",
    )
    args = parser.parse_args(argv)

    logging.basicConfig(
        level=args.log_level.upper(),
        format="%(asctime)s %(levelname)s %(name)s :: %(message)s",
    )

    server = CheckerServer(args.runtime_root, parallelism=args.parallelism)
    if args.peer_uid is not None:
        server.set_expected_peer_uid(args.peer_uid)

    try:
        server.start()
    except SingletonError as exc:
        print(f"checker server refusing to start: {exc}", file=sys.stderr)
        return 2

    def _handle_signal(_signum: int, _frame: Any) -> None:
        server.shutdown()

    signal.signal(signal.SIGTERM, _handle_signal)
    signal.signal(signal.SIGINT, _handle_signal)
    # Defensive: tmux usually nohups its children, but if a parent shell does
    # forward SIGHUP, the default action is to terminate. Handling it here
    # turns hang-up into a graceful shutdown like the other two signals.
    signal.signal(signal.SIGHUP, _handle_signal)

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.shutdown()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
