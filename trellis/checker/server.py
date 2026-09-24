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
- A workspace-level reader/writer lock keeps every workspace-mutating lake
  operation exclusive.  Replay-attested ``local_closure_axioms`` probes use
  the shared side: they run ``lake env lean --run`` against already-current
  oleans and do not build or mutate the workspace.
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
from collections import OrderedDict
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager, suppress
from pathlib import Path
from typing import Any, Dict, Iterator, Mapping, Optional, Sequence, Tuple

from trellis.atomic_actions import isabelle_observations, observations
from trellis.checker import telemetry
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
# Elaboration-cost replay memo bound (see ``CheckerServer.__init__`` for the
# memo's reason to exist). LRU-evicted beyond this. Sized an order of
# magnitude above the live tablet's node count (~400) so eviction only fires
# under pathological churn; each entry is a small dict (~250 B), so the
# worst-case footprint is ~1 MiB.
_ELABORATION_COST_MEMO_MAX_ENTRIES = 4096


class _WorkspaceReaderWriterLock:
    """Writer-preferring workspace gate with an ``RLock``-like writer API.

    Lake builds, materialization, and Isabelle operations take the writer
    side through ``with lock``.  The only reader is the local-closure probe,
    after its complete olean closure has current kernel-replay attestations.
    Writer preference prevents a stream of long probes from starving an
    operation that needs to repair or mutate the workspace.

    ``acquire``/``release`` are retained for the writer side because a few
    diagnostics and tests deliberately hold the workspace gate directly.
    """

    def __init__(self) -> None:
        self._condition = threading.Condition(threading.Lock())
        self._readers = 0
        self._writer_thread: Optional[int] = None
        self._writer_depth = 0
        self._waiting_writers = 0

    def acquire(self) -> bool:
        ident = threading.get_ident()
        with self._condition:
            if self._writer_thread == ident:
                self._writer_depth += 1
                return True
            self._waiting_writers += 1
            try:
                while self._writer_thread is not None or self._readers:
                    self._condition.wait()
                self._writer_thread = ident
                self._writer_depth = 1
                return True
            finally:
                self._waiting_writers -= 1

    def release(self) -> None:
        ident = threading.get_ident()
        with self._condition:
            if self._writer_thread != ident:
                raise RuntimeError("workspace writer lock released by non-owner")
            self._writer_depth -= 1
            if self._writer_depth == 0:
                self._writer_thread = None
                self._condition.notify_all()

    def __enter__(self) -> "_WorkspaceReaderWriterLock":
        self.acquire()
        return self

    def __exit__(self, exc_type: Any, exc: Any, traceback: Any) -> None:
        self.release()

    @contextmanager
    def read_lock(self) -> Iterator[None]:
        ident = threading.get_ident()
        with self._condition:
            # A writer may safely enter a nested read section: it still owns
            # exclusive access.  Ordinary readers yield to queued writers.
            if self._writer_thread != ident:
                while self._writer_thread is not None or self._waiting_writers:
                    self._condition.wait()
            self._readers += 1
        try:
            yield
        finally:
            with self._condition:
                self._readers -= 1
                if self._readers == 0:
                    self._condition.notify_all()


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


@telemetry.measured("file_hash")
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

    def _repo_from_metadata() -> Optional[Path]:
        # S1 fallback: honor the explicit repo_path the supervisor recorded in
        # runtime_metadata.json (prep recomputes it from the actual prepped
        # repo). Used ONLY when the structural forms below cannot locate a
        # co-located repo — a genuinely relocated runtime. Structural forms take
        # PRECEDENCE because physical containment is authoritative: a stale but
        # still-valid baked repo_path must not override the real co-located repo
        # (that would re-introduce a split-brain on the no-prep clone path).
        meta_path = runtime_root / "runtime_metadata.json"
        try:
            meta = json.loads(meta_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError, UnicodeDecodeError, ValueError):
            return None
        if isinstance(meta, dict):
            repo_path_raw = meta.get("repo_path")
            if isinstance(repo_path_raw, str) and repo_path_raw.strip():
                candidate = Path(repo_path_raw).resolve()
                if candidate.is_dir() and (candidate / ".trellis").is_dir():
                    return candidate
        return None

    # Inner form first (runtime physically inside the repo — authoritative).
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
    # Outer form: <parent>/<repo_basename>-runtime → <parent>/<repo_basename>
    # (runtime beside the repo — authoritative when the sibling exists).
    name = runtime_root.name
    suffix = "-runtime"
    if name.endswith(suffix) and len(name) > len(suffix):
        repo_basename = name[: -len(suffix)]
        candidate = runtime_root.parent / repo_basename
        if candidate.is_dir() and (candidate / ".trellis").is_dir():
            return candidate
        # Not co-located → fall back to the explicit recorded repo_path (S1),
        # which prep recomputes so both sides still agree on a relocated runtime.
        meta_repo = _repo_from_metadata()
        if meta_repo is not None:
            return meta_repo
        raise ValueError(
            f"runtime_root '{runtime_root}' looks like outer form (basename "
            f"ends in '-runtime') but sibling repo '{candidate}' is not a "
            f"directory containing '.trellis/' and runtime_metadata.json has no "
            f"valid repo_path fallback"
        )
    # Unrecognized layout: last-resort explicit repo_path before failing.
    meta_repo = _repo_from_metadata()
    if meta_repo is not None:
        return meta_repo
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


def _cost_log_extra(payload: Mapping[str, Any]) -> Dict[str, Any]:
    """Surface elaboration-cost telemetry to the request log.

    Emits ``stale_node_count`` whenever lake actually ran, and a ``cost``
    block on top of it when the attribution rule in
    ``trellis.atomic_actions.observations`` held — one lake invocation whose
    stale set was exactly one node. Logging both makes the rule falsifiable
    from the log alone rather than on the author's word:

      * ``cost`` present with ``stale_node_count`` above 1 — the gate is
        broken and the measurement is fabricated.
      * ``stale_node_count`` of 1 with no ``cost`` — the measurement dropped
        (the sampler observed nothing, the peak fell below the zero-import
        `lean` floor — then a ``cost_floor_violation`` block says so — or
        the node did not end up materialized).
      * ``cost`` WITHOUT ``replayed: true`` on a ``compile_cache_hit: true``
        line, or on a ``stale_node_count: 0`` line — impossible by
        construction, since neither spawned a lake that elaborated anything;
        seeing one means the synthesis path started copying lake payloads.
        A ``cost`` WITH ``replayed: true`` on those lines is legitimate: it
        is this server re-serving a measurement it itself took earlier for
        byte-identical content (see ``_memoize_elaboration_cost``).

    A single-stale-node ``materialize_oleans`` line legitimately carries a
    cost: the attribution rule is about what one lake process elaborated, not
    about which op asked for it. The count is what tells the two apart.

    The ``cost`` block also carries the anti-echo context (see
    ``observations._HostProcPeakSampler``): ``peak_rss_process`` should read
    ``lean``, and ``peak_rss_kib`` should track neither ``wait4_maxrss_kib``
    nor ``checker_self_rss_kib`` — under the old ``wait4`` measurement it
    was byte-identical to the latter.

    Pure telemetry: nothing in the checker reads any of it back.
    """
    extra: Dict[str, Any] = {}
    stale_node_count = payload.get("stale_node_count")
    if isinstance(stale_node_count, int):
        extra["stale_node_count"] = stale_node_count
    cost = payload.get("cost")
    if isinstance(cost, Mapping):
        extra["cost"] = dict(cost)
    floor_violation = payload.get("cost_floor_violation")
    if isinstance(floor_violation, Mapping):
        extra["cost_floor_violation"] = dict(floor_violation)
    return extra


class _RequestLog:
    """Append-only newline-JSON request log with a per-server lock."""

    def __init__(self, path: Path) -> None:
        self._path = path
        self._lock = threading.Lock()
        path.parent.mkdir(parents=True, exist_ok=True)

    @telemetry.deferred_log
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

        self._workspace_lock = _WorkspaceReaderWriterLock()
        # ``_sync_lock`` serializes ``sync_tablet_dir`` invocations
        # independently of ``_workspace_lock`` (the lake lock). Two
        # simultaneous ``sync_tablet_dir`` calls would race on the
        # supervisor's ``Tablet/`` files and the
        # ``sync-fingerprints.json`` cache — both are worker-supervisor
        # mirrors that must serialize. Lake invocations remain
        # serialized on the writer side of ``_workspace_lock``; only
        # replay-attested local-closure probes may share its reader side.
        # Sync and workspace operations CAN overlap. The race that lets sync
        # write a new source while
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
        # verify the olean is current with its source CONTENT
        # (``observations.olean_is_content_current`` — olean exists, size>0,
        # provenance sidecar matches the source-closure hash) before skipping
        # lake — never trust the in-memory bit alone, and never trust mtime.
        self._oleans_known_current: set[str] = set()
        # Per-node Lean-derived compile output. This is intentionally
        # separate from ``_oleans_known_current``: a current olean proves the
        # build artifact is reusable, but not that a synthetic compile result
        # may erase diagnostic-bearing stdout/stderr. The Rust checker uses
        # this output both for sorry detection and cleanup-lint identity, so
        # replaying only a derived sorry bit is insufficient. Absence means
        # "unknown", not "the compile was quiet", and forces a real compile.
        self._compile_diagnostic_output: Dict[str, Tuple[str, str]] = {}
        self._oleans_lock = threading.Lock()
        # Elaboration-cost replay memo, keyed by (node, source_closure_hash).
        # THE PROCESS THAT MEASURES IS NOT THE PROCESS WHOSE RESPONSE REACHES
        # THE ENGINE: the worker's in-burst self-check drives the genuine
        # compile miss through this server (lake runs, the sampler measures,
        # the cost rides that response) — but that response goes only to the
        # worker's kernel CLI, whose output the engine deliberately discards
        # because worker-supplied data is forgeable. The supervisor's check —
        # the only response the engine applies — arrives after the compile
        # cache is warm, and a synthesized hit performs no elaboration, so
        # without this memo the supervisor can never carry a cost and every
        # measurement is dropped. This memo lets the ONE long-lived process
        # that actually took the measurement re-serve it for byte-identical
        # content; the number still never originates in a worker payload, so
        # forgery resistance is preserved. Do not remove as redundant with
        # the compile cache: the compile cache proves the olean is reusable,
        # while this memo carries the telemetry of the build that produced it.
        # In-memory only, starts empty on server restart — DELIBERATE: a memo
        # miss yields no cost (absence is the feature's ordinary, honest
        # state), and persisting measurements to disk would add a staleness
        # and tamper surface that pure telemetry does not warrant. The cost
        # of that choice is one dropped measurement after a server restart;
        # compile validity is re-established independently and never depends
        # on this telemetry. Guarded by ``_cost_memo_lock``; bounded by
        # ``_ELABORATION_COST_MEMO_MAX_ENTRIES`` (LRU).
        self._elaboration_cost_memo: "OrderedDict[Tuple[str, str], Dict[str, Any]]" = (
            OrderedDict()
        )
        self._cost_memo_lock = threading.Lock()
        # Asynchronous heartbeat measurement (``trellis.checker.heartbeat``).
        # Created lazily on the first measured cost that lacks a heartbeat
        # count, via a guarded import so a broken or absent module can never
        # affect server startup or any request path — the whole channel is
        # fail-open instrumentation. ``False`` = tried and unavailable.
        self._heartbeat_measurer: Any = None
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

        # Isabelle/HOL checker (B2a). A single warm HOL session per server
        # lifetime, created lazily on the first Isabelle op and reaped on
        # shutdown. The raw TCP port + password live INSIDE the session
        # object and never reach this server's AF_UNIX wire (the trust
        # property report-02 requires). ``_isabelle_session`` is None on
        # the Lean path (no IsabelleHol tablet is live → no Isabelle op
        # runs), so the Lean checker is byte-untouched. Guarded by
        # ``_isabelle_lock`` because session spawn + the single TCP socket
        # are not concurrency-safe; Isabelle ops already serialize under
        # ``_workspace_lock`` like lake, but the lazy-init/reap needs its
        # own guard to stay correct if a future caller relaxes that.
        self._isabelle_session: Optional[Any] = None
        self._isabelle_lock = threading.Lock()
        # Per-server unique Isabelle server name, derived from the runtime
        # root basename + pid so co-resident runs never collide on a
        # ``servers.db`` row (Appendix A.5 stale-row hazard). Reaped by
        # this exact name on shutdown.
        self._isabelle_server_name = (
            f"trellis-{self.runtime_root.name}-{os.getpid()}"
        )
        # Phase 2 warm-gate (flag-gated OFF by default). The warm config is
        # loaded lazily + cached on the first Isabelle node op from
        # ``<supervisor_repo>/trellis.config.json`` (env switch overrides). When
        # OFF (the default), the cold node-check path below is byte-for-byte
        # unchanged and ``_isabelle_warm_gate`` stays None. When ON, the held
        # ``_isabelle_session`` is the warm (``warm_prefix_enabled=True``)
        # session and the node check routes through ``IsabelleWarmGate`` (which
        # reconciles the warm prefix, falls back to cold on a session anomaly,
        # and cold cross-checks on a cadence — halting on warm-vs-cold mismatch).
        self._isabelle_warm_config: Optional[Any] = None
        self._isabelle_warm_gate: Optional[Any] = None

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
        # S4: ignore a token file that does not belong to THIS runtime — a
        # copied/seeded checker-state, or a legacy file written before the
        # runtime_root stamp existed. Honoring a stale file would flip the auth
        # gate active and reject legitimate token-less clients (the prep
        # `auth_required` footgun). Keep the gate dormant; the bridge for this
        # runtime rewrites the file with a matching runtime_root on its next
        # dispatch, at which point the tokens are honored.
        file_runtime = data.get("runtime_root")
        if not isinstance(file_runtime, str) or file_runtime != str(
            self.runtime_root.resolve()
        ):
            with self._burst_tokens_lock:
                self._burst_tokens = set()
                self._burst_tokens_mtime_ns = mtime_ns
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

        # Do not pre-warm ``_oleans_known_current`` from build-directory
        # sidecars. The lake elaborator can write that directory, so an
        # unchecked declaration installed under `debug.skipKernelTC` could
        # forge its own source-closure marker. A server lifetime learns a
        # current olean only from a materialization response that completed
        # the independent leanchecker replay. The kernel's trusted disk cache
        # still avoids repeat work across processes after such a response.

        _LOGGER.info(
            "checker server listening at %s (parallelism=%d, supervisor_repo=%s, "
            "known_current_oleans=%d)",
            self.socket_path,
            self.parallelism,
            self.supervisor_repo,
            len(self._oleans_known_current),
        )

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
        # Stop the background heartbeat measurer first (never blocks; the
        # daemon thread notices the flag and dies with the process).
        try:
            if self._heartbeat_measurer not in (None, False):
                self._heartbeat_measurer.shutdown()
        except Exception:
            pass
        if self._listen_sock is not None:
            with suppress(OSError):
                self._listen_sock.shutdown(socket.SHUT_RDWR)
            with suppress(OSError):
                self._listen_sock.close()
            self._listen_sock = None
        if self._executor is not None:
            self._executor.shutdown(wait=True)
            self._executor = None
        # Reap the Isabelle server BY NAME (Appendix A.5): after the
        # executor drains there are no in-flight Isabelle ops, so closing
        # the warm HOL session + stopping the named server + deleting its
        # servers.db row is safe. Best-effort; never blocks shutdown.
        self._close_isabelle_session()
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

    @telemetry.request
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

        Phase 3 (under ``_workspace_lock``): cache miss → run the operation.
        Workspace-mutating lake/Isabelle operations take the exclusive side.
        A full ``local_closure_axioms`` probe may take the shared side only
        when its entire olean closure already has current kernel-replay
        attestations.  That command is then a read-only ``lake env lean
        --run`` consumer; it cannot fall through to materialization.  Inside
        either side we re-check the cache before spending any lake time.

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
        with telemetry.lock(self._sync_lock, "sync"):
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

        # Phase 3: cache miss → take the appropriate workspace gate.  A
        # local-closure request first enters the shared gate and re-checks
        # replay readiness *inside* it.  If cold, it leaves the shared gate
        # and falls back to the writer side, where its handler may safely
        # materialize.  All other operations are writers unconditionally.
        lake_lock_wait_started = time.monotonic_ns()
        used_shared_workspace = False
        if request.op == "local_closure_axioms" and not bool(
            request.raw.get("scan_only", False)
        ):
            with telemetry.lock(self._workspace_lock.read_lock(), "shared"):
                shared_lock_acquired = time.monotonic_ns()
                assert request.node_name is not None
                if self._closures_have_current_kernel_replay(
                    [request.node_name]
                ):
                    lake_lock_acquired = shared_lock_acquired
                    cache_hit_recheck = self._try_cache_hit_only(
                        request, sync_result
                    )
                    if cache_hit_recheck is not None:
                        response, returncode, op_log_extra = cache_hit_recheck
                    else:
                        response, returncode, op_log_extra = self._dispatch_op(
                            request, sync_result
                        )
                    lake_finished = time.monotonic_ns()
                    used_shared_workspace = True

        if not used_shared_workspace:
            with telemetry.lock(self._workspace_lock, "exclusive"):
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

        op_log_extra = {
            **op_log_extra,
            "workspace_lock_mode": (
                "shared" if used_shared_workspace else "exclusive"
            ),
        }
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
                [request.node_name], include_compile_output=True
            )
            if cached is not None:
                cached["node"] = request.node_name
                self._attach_replayed_cost(cached, request.node_name)
                return (
                    {"request_id": rid, **cached},
                    0,
                    {
                        "compile_cache_hit": True,
                        "compile_cache_nodes": 1,
                        **_cost_log_extra(cached),
                    },
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
                        compile_outputs={
                            request.node_name: (
                                str(payload.get("stdout") or ""),
                                str(payload.get("stderr") or ""),
                            )
                        },
                    )
                # Else: a parallel sync changed the source during lake.
                # Don't record — the next cache check (post-sync's
                # invalidation in another thread) will route this node
                # back through lake at the correct source state.
            self._memoize_elaboration_cost(payload)
            if (
                payload.get("cost") is None
                and payload.get("returncode") == 0
                and payload.get("stale_node_count") == 0
                and request.node_name
                in set(payload.get("materialized_nodes", []) or [])
            ):
                # Lake ran but had nothing to elaborate (the on-disk olean
                # was content-current; only this server's in-memory bit —
                # e.g. a missing sorry-warning fact — forced the dispatch).
                # The attribution rule rightly recorded nothing, but the
                # content is byte-identical to a build this server may have
                # measured; re-serve that measurement if the memo has it.
                self._attach_replayed_cost(payload, request.node_name)
            return (
                {"request_id": rid, **payload},
                payload.get("returncode"),
                {
                    "compile_cache_hit": False,
                    "compile_cache_nodes": 0,
                    **_cost_log_extra(payload),
                },
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
            # Feed the replay memo (never attach here: no kernel consumer
            # reads cost off a materialize response — the memoized entry
            # reaches the engine via the node's next compile op instead).
            self._memoize_elaboration_cost(payload)
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
                    **_cost_log_extra(payload),
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

        if op in {
            "isabelle_check_node",
            "isabelle_thm_oracles",
            "isabelle_thm_deps",
            "isabelle_build_session",
            "isabelle_sync_session",
            "isabelle_warm_advisory",
        }:
            return self._dispatch_isabelle_op(request)

        # ping is handled in _handle_request before lock acquisition.
        raise ProtocolError("unknown_op", f"dispatcher missing op {op!r}")

    # --------------------------- cache-hit-only fast path ---------------------------

    @telemetry.measured("cache_lookup")
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
                [request.node_name], include_compile_output=True
            )
            if cached is None:
                return None
            cached["node"] = request.node_name
            self._attach_replayed_cost(cached, request.node_name)
            return (
                {"request_id": rid, **cached},
                0,
                {
                    "compile_cache_hit": True,
                    "compile_cache_nodes": 1,
                    **_cost_log_extra(cached),
                },
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

        # ``lean_build_tablet`` and every Isabelle op have no per-request
        # cache — always take the workspace lock and drive the tool.
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
        if not self._closures_have_current_kernel_replay([node_name]):
            return None

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

    @telemetry.measured("local_closure_cache_prefilter")
    def _local_closure_axioms_cache_definitely_misses(
        self, request: CheckerRequest
    ) -> bool:
        """Advisory only: True skips a cache attempt; False runs its validator.

        No key, response, bundle or freshness result escapes this lookup.
        Even incorrect/stale advisory inputs can only cause an extra live
        probe or defer to the original readiness -> fresh key -> load path.
        Nothing is retained, including negative decisions. Keep this cheap
        key/load duplication separate from the authoritative hit branch.
        """
        try:
            node_name = request.node_name
            if node_name is None or bool(request.raw.get("scan_only", False)):
                return False
            no_axcheck = bool(request.raw.get("no_axcheck", False))
            module_owner = bool(request.raw.get("module_owner", False))
            principal_name = request.raw.get("principal_name")
            if module_owner:
                if node_name != "Preamble" or principal_name is not None:
                    return False
                principal_identity = "<module-owner>"
            else:
                if not isinstance(principal_name, str) or not principal_name:
                    return False
                principal_identity = principal_name

            script_sha = _sha256_file_or_empty(self._local_closure_script_path)
            toolchain_sha = _sha256_file_or_empty(self._toolchain_path)
            manifest_sha = _sha256_file_or_empty(self._lake_manifest_path)
            if not (script_sha and toolchain_sha):
                return True
            sync_cache = load_fingerprint_cache(self.fingerprint_cache_path)
            cache_key_base = compute_semantic_payload_cache_key(
                self.supervisor_repo,
                node_name,
                sync_cache,
                script_sha,
                toolchain_sha,
                manifest_sha,
                LOCAL_CLOSURE_AXIOMS_CACHE_VERSION,
            )
            if cache_key_base is None:
                return True
            principal_key = hashlib.sha256(principal_identity.encode("utf-8")).hexdigest()
            cache_key = f"{cache_key_base}-principal-{principal_key}"
            if no_axcheck:
                cache_key += "-noax"
            sidecar = load_local_closure_axioms(
                self.local_closure_axioms_cache_dir,
                cache_key,
                expected_version=LOCAL_CLOSURE_AXIOMS_CACHE_VERSION,
            )
            return sidecar is None or not isinstance(sidecar.get("response"), dict)
        except Exception:
            # Uncertain input or an unexpected advisory failure must not
            # replace any outcome of the existing authoritative validator.
            return False

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

        Like every cache-hit path, this one performs no elaboration and must
        never carry an elaboration-cost measurement. Doubly excluded here:
        ``local_closure_axioms`` imports an already-built olean rather than
        producing one, so even its lake-spawning miss path takes no
        measurement (it calls ``_run_lake_command`` without a ``metrics``
        out-dict).
        """
        rid = request.request_id
        node_name = request.node_name
        assert node_name is not None

        # Scan-only requests are never served from this cache. An earlier
        # synthesis inferred a scan-only success from any cached full-probe
        # success, on the premise that the full probe runs the same
        # owner-file scan (``ownerFileScanRejection``) before its closure
        # walk. That premise no longer holds: the Lean-side refactor
        # (865274c3) replaced the scan with ``ownerFilePolicyScan``, whose
        # only call site is inside ``runScanOnly`` — the full certificate
        # probe never runs it — so a stored full-probe ``status="ok"`` does
        # not prove the owner-file policy scan passed. The scan is the gate
        # that rejects declaration-forging command kinds (``macro``,
        # ``elab``, ``syntax``, ``run_cmd``, ...), which elaborate cleanly
        # and earn a full-probe ``ok``. Fall through to the live, parse-only
        # scan unconditionally (the pre-reuse behavior; fail-closed).
        scan_only = bool(request.raw.get("scan_only", False))
        if scan_only:
            return None
        if self._local_closure_axioms_cache_definitely_misses(request) is True:
            return None
        # Serving the FULL cached result replays closure facts about olean
        # artifacts, so it additionally requires a current kernel-replay
        # attestation for the exact olean closure (observation cache versions
        # describe record shape, not replay state).
        if not self._closures_have_current_kernel_replay([node_name]):
            return None

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
        module_owner = bool(request.raw.get("module_owner", False))
        principal_name = request.raw.get("principal_name")
        if module_owner:
            if node_name != "Preamble" or principal_name is not None:
                return None
            principal_identity = "<module-owner>"
        else:
            if not isinstance(principal_name, str) or not principal_name:
                return None
            principal_identity = principal_name
        principal_key = hashlib.sha256(principal_identity.encode("utf-8")).hexdigest()
        cache_key = f"{cache_key_base}-principal-{principal_key}"
        if no_axcheck:
            cache_key += "-noax"

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
        principal_names = request.raw.get("principal_names")
        if not isinstance(principal_names, dict) or any(
            not str(principal_names.get(node, "")).strip() for node in requested
        ):
            return None
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

        if not self._closures_have_current_kernel_replay(requested):
            return None

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
                if cache_key is not None:
                    principal = str(principal_names[node_name]).strip()
                    cache_key += "-principal-" + hashlib.sha256(
                        principal.encode("utf-8")
                    ).hexdigest()
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

    @telemetry.measured("replay_readiness")
    def _closures_have_current_kernel_replay(
        self, node_names: Sequence[str]
    ) -> bool:
        """Require replay attestations for every olean a cache entry trusts.

        Observation cache versions describe record shape, not replay state.
        This explicit consistency predicate lets byte-identical pre-deploy
        cache entries survive migration while making them unusable until the
        exact olean closure has acquired current toolchain-bound attestations.
        """
        closure: list[str] = []
        seen: set[str] = set()
        for node_name in node_names:
            for member in observations.materialization_order(
                self.supervisor_repo, [node_name]
            ):
                if member not in seen:
                    seen.add(member)
                    closure.append(member)
        return bool(closure) and all(
            observations.olean_is_content_current(self.supervisor_repo, member)
            and observations.olean_has_current_kernel_replay(
                self.supervisor_repo, member
            )
            for member in closure
        )

    @telemetry.measured("ensure_replay")
    def _ensure_closures_have_current_kernel_replay(
        self,
        node_names: Sequence[str],
        *,
        timeout_secs: float,
    ) -> Optional[Dict[str, Any]]:
        """Materialize/replay before any live olean-consuming observation.

        Cache guards alone are insufficient during first-resume migration: a
        cold cache must not fall through to semantic/axiom probes that read
        legacy, unattested oleans.  This method runs under the server's normal
        workspace lock and returns the failed materialize payload, if any.
        """
        requested = [str(name).strip() for name in node_names if str(name).strip()]
        if not requested or self._closures_have_current_kernel_replay(requested):
            return None
        payload = observations.materialize_tablet_oleans(
            self.supervisor_repo,
            requested,
            timeout_secs=timeout_secs,
            bwrap_role="lake_compiler",
        )
        if (
            payload.get("returncode") == 0
            and not payload.get("timed_out")
            and not payload.get("spawn_error")
            and self._closures_have_current_kernel_replay(requested)
        ):
            return None
        failed = dict(payload)
        if failed.get("returncode") == 0:
            failed["returncode"] = 1
            prior = str(failed.get("stderr", "") or "")
            detail = "kernel replay precondition remained unsatisfied"
            failed["stderr"] = f"{prior}\n{detail}".lstrip("\n")
        return failed

    def _compute_compile_cache_subset(
        self, node_names: Sequence[str]
    ) -> set[str]:
        """Return the subset of ``node_names`` whose oleans are known-current.

        Two-step safety check (mirrors ``_maybe_synthesize_compile_hit``):
          1. The node is in ``_oleans_known_current`` (this server has
             previously confirmed lake exit 0 for it since the last sync
             diff that touched its closure).
          2. The node's olean is current with its source CONTENT, decided by
             ``observations.olean_is_content_current`` (olean exists,
             non-empty, and its provenance sidecar equals the current
             source-closure hash). Mtime is never consulted — an operator
             ``touch`` / ``git reset`` cannot make a content-stale olean look
             current here.

        Any node failing step 2 is evicted from ``_oleans_known_current``
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
            try:
                current = (
                    observations.olean_is_content_current(
                        self.supervisor_repo, name
                    )
                    and observations.olean_has_current_kernel_replay(
                        self.supervisor_repo, name
                    )
                )
            except OSError:
                current = False
            if not current:
                evicted.append(name)
                continue
            valid.add(name)
        if evicted:
            with self._oleans_lock:
                for name in evicted:
                    self._oleans_known_current.discard(name)
                    self._compile_diagnostic_output.pop(name, None)
        return valid

    def _maybe_synthesize_compile_hit(
        self,
        node_names: Sequence[str],
        *,
        include_compile_output: bool = False,
    ) -> Optional[Dict[str, Any]]:
        """Return a synthetic success payload if it is provably safe to skip
        lake for *every* requested node, else ``None``.

        Thin wrapper over ``_compute_compile_cache_subset``: the subset must
        cover every requested node. Any miss (in-memory bit absent, olean
        missing/empty, or content-stale by source-closure hash) triggers
        eviction inside the subset helper and returns ``None`` so the caller
        falls through to lake.

        ``include_compile_output`` is required for
        ``lean_compile_node``/``verify_node`` responses, whose stdout/stderr
        feed Rust validity checks. In that mode every requested node must have
        the complete cached Lean-derived streams; missing means unknown, so
        the caller must fall through to lake. ``materialize_oleans`` does not
        consume stdout/stderr semantically and may still use olean-only hits.

        A fresh elaboration cost is structurally absent from the synthesized
        payload: a cache hit performs no elaboration, so there is nothing to
        measure, and this dict is built from scratch, never copied from a
        lake payload. The compile-op callers may afterwards attach a
        ``replayed: true`` memo entry via ``_attach_replayed_cost`` — that is
        a re-serve of a measurement this server itself took for
        byte-identical content, not a measurement of the hit.
        """
        names = list(node_names)
        if not names:
            return None
        cached = self._compute_compile_cache_subset(names)
        if len(cached) != len(set(names)):
            return None
        stdout = ""
        stderr = ""
        if include_compile_output:
            with self._oleans_lock:
                outputs: list[Tuple[str, str]] = []
                for name in names:
                    output = self._compile_diagnostic_output.get(name)
                    if output is None:
                        return None
                    outputs.append(output)
            stdout = "".join(output[0] for output in outputs)
            stderr = "".join(output[1] for output in outputs)
        return {
            "requested_nodes": names,
            "materialized_nodes": names,
            "returncode": 0,
            "stdout": stdout,
            "stderr": stderr,
            "timed_out": False,
            "spawn_error": "",
        }

    def _memoize_elaboration_cost(self, payload: Mapping[str, Any]) -> None:
        """Feed the replay memo from a lake payload that carries a ``cost``.

        Called on every lake-dispatched payload — both the compile ops and
        ``materialize_oleans``. Feeding from materialize is load-bearing, not
        opportunistic: a single-stale materialize measures honestly (the
        attribution rule is op-agnostic) but no kernel consumer reads cost
        off a materialize response, so without the memo those measurements
        are dropped by every consumer. Replaying them on the node's next
        compile op is the only path by which they reach the engine.

        The key is exactly ``(node, source_closure_hash)`` — the hash already
        folds in the toolchain, manifest, preamble, node file and transitive
        dep closure (see ``observations.tablet_source_closure_hash``), so a
        stale entry can never be served for changed content. Keying on the
        node name alone would break that guarantee; do not widen the key.
        """
        cost = payload.get("cost")
        if not isinstance(cost, Mapping):
            return
        node_name = str(cost.get("node") or "")
        closure_hash = str(cost.get("source_closure_hash") or "")
        if not node_name or not closure_hash:
            return
        # Never store a replay marker: the memo holds measurements only, so
        # a re-serve is marked at attach time and cannot compound.
        entry = {key: value for key, value in cost.items() if key != "replayed"}
        with self._cost_memo_lock:
            memo = self._elaboration_cost_memo
            memo[(node_name, closure_hash)] = entry
            memo.move_to_end((node_name, closure_hash))
            while len(memo) > _ELABORATION_COST_MEMO_MAX_ENTRIES:
                memo.popitem(last=False)
        # A fresh measurement without a heartbeat count is exactly the
        # trigger for the asynchronous instrumented re-elaboration: the
        # acceptance build stays uninstrumented by design (instrumentation
        # spends the budget it measures), so the count has to come from a
        # separate background build. Fail-open, non-blocking, never affects
        # this response.
        if entry.get("heartbeats") is None:
            self._maybe_enqueue_heartbeat_measurement(node_name, closure_hash)

    def _maybe_enqueue_heartbeat_measurement(
        self, node_name: str, closure_hash: str
    ) -> None:
        """Hand one (node, hash) to the background heartbeat measurer.

        Guarded end-to-end: a missing/broken ``trellis.checker.heartbeat``
        module, a constructor failure, or an enqueue failure all degrade to
        "no measurement" and nothing else. The measurer itself is bounded
        (one build at a time, bounded drop-on-full queue, niced, MemAvailable
        floor) — see the module docstring for the full contract.
        """
        try:
            if self._heartbeat_measurer is False:
                return
            if self._heartbeat_measurer is None:
                from trellis.checker.heartbeat import HeartbeatMeasurer

                self._heartbeat_measurer = HeartbeatMeasurer(
                    self.supervisor_repo,
                    self.runtime_root,
                    log=self._log_heartbeat_measure_event,
                )
            self._heartbeat_measurer.enqueue(node_name, closure_hash)
        except Exception:
            self._heartbeat_measurer = False

    def _log_heartbeat_measure_event(self, message: str) -> None:
        """Best-effort request-log line for measurement telemetry."""
        try:
            now_ns = time.monotonic_ns()
            self._request_log.emit(
                request_id=0,
                op="heartbeat_measure",
                started_ns=now_ns,
                finished_ns=now_ns,
                returncode=None,
                sync_changed_files=0,
                detail=str(message),
            )
        except Exception:
            pass

    def _attach_replayed_cost(
        self, payload: Dict[str, Any], node_name: str
    ) -> None:
        """Attach the memoized measurement for ``node_name``'s CURRENT
        content to ``payload``, if one exists. Mutates ``payload`` in place.

        Callers invoke this on the two response shapes that structurally
        cannot carry a fresh measurement — the synthesized compile hit and
        the lake dispatch whose stale set was zero — which is exactly the
        shape of the supervisor's re-check of content the worker's self-check
        already built through this server. A memo miss attaches nothing: no
        cost is the honest answer, never a guess. The attached copy carries
        ``replayed: true`` so the request log can tell a re-serve from a
        fabricated hit-with-cost (`_cost_log_extra`); the kernel's serde
        drops that marker, so the record itself is indistinguishable from
        one recorded at measurement time — which it is.
        """
        if payload.get("cost") is not None:
            return
        try:
            closure_hash = observations.tablet_source_closure_hash(
                self.supervisor_repo, node_name
            )
        except OSError:
            return
        if not closure_hash:
            return
        with self._cost_memo_lock:
            entry = self._elaboration_cost_memo.get((node_name, closure_hash))
            if entry is None:
                return
            self._elaboration_cost_memo.move_to_end((node_name, closure_hash))
            cost = dict(entry)
        cost["replayed"] = True
        payload["cost"] = cost

    def _record_oleans_built(
        self,
        node_names: Sequence[str],
        *,
        compile_outputs: Optional[Mapping[str, Tuple[str, str]]] = None,
    ) -> None:
        """Mark each name as having a known-current olean for this server's
        lifetime. Called only after materialization exited cleanly, including
        the independent leanchecker replay prerequisite.

        ``compile_outputs`` is only supplied by real
        ``lean_compile_node`` calls. Batched materialization can prove oleans
        current, but it does not provide a per-request compile observation
        whose diagnostic streams may be replayed later.
        """
        if not node_names:
            return
        outputs = compile_outputs or {}
        with self._oleans_lock:
            for name in node_names:
                if name:
                    self._oleans_known_current.add(name)
                    if name in outputs:
                        self._compile_diagnostic_output[name] = outputs[name]

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
                    self._compile_diagnostic_output.clear()
                return
            if "." not in path:
                with self._oleans_lock:
                    self._oleans_known_current.clear()
                    self._compile_diagnostic_output.clear()
                return
            node_name = path.rsplit(".", 1)[0]
            if not NODE_NAME_REGEX.fullmatch(node_name):
                with self._oleans_lock:
                    self._oleans_known_current.clear()
                    self._compile_diagnostic_output.clear()
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
                self._compile_diagnostic_output.pop(name, None)

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
            for imported in observations._kernel_extract_tablet_imports(text):
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

        replay_failure = self._ensure_closures_have_current_kernel_replay(
            [node_name], timeout_secs=request.timeout_secs
        )
        if replay_failure is not None:
            response = {
                "request_id": rid,
                "node": node_name,
                "returncode": replay_failure.get("returncode"),
                "stdout": str(replay_failure.get("stdout", "") or ""),
                "stderr": str(replay_failure.get("stderr", "") or ""),
                "timed_out": bool(replay_failure.get("timed_out", False)),
                "spawn_error": str(replay_failure.get("spawn_error", "") or ""),
            }
            return (
                response,
                replay_failure.get("returncode"),
                {"kernel_replay_precondition_failed": True},
            )

        script_sha = _sha256_file_or_empty(self._fingerprint_script_path)
        toolchain_sha = _sha256_file_or_empty(self._toolchain_path)
        manifest_sha = _sha256_file_or_empty(self._lake_manifest_path)
        sync_cache = load_fingerprint_cache(self.fingerprint_cache_path)
        replay_ready = self._closures_have_current_kernel_replay([node_name])

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

        if cache_key is not None and replay_ready:
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
            and self._closures_have_current_kernel_replay([node_name])
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

        Materialization: the kernel-side caller normally issues the support
        precondition first. This handler also checks the authoritative replay
        attestations and materializes on a cold migration path, so a direct or
        reordered request cannot probe an unattested olean. Path containment
        was already enforced upstream in ``_handle_request`` via
        ``_assert_path_containment``.

        Concurrency: a full probe whose complete olean closure has current
        kernel-replay attestations runs on the shared side of
        ``_workspace_lock``.  It only reads source/olean/replay files and
        executes ``lake env lean --run`` without a metrics side channel or
        build target.  Cache sidecars use atomic per-key stores.  Cold and
        scan-only requests, plus every operation capable of materializing or
        building, stay on the exclusive side.
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
        # ``--scan-only`` runs only the source-based worker-authoring policy
        # check. It is separate from artifact certificate traversal.
        scan_only = bool(request.raw.get("scan_only", False))
        module_owner = bool(request.raw.get("module_owner", False))
        principal_name_raw = request.raw.get("principal_name")
        principal_name = principal_name_raw if isinstance(principal_name_raw, str) else ""
        if module_owner and (node_name != "Preamble" or principal_name):
            error = "module-owner probing is reserved for Preamble and takes no principal"
            return (
                {
                    "request_id": rid,
                    "node": node_name,
                    "returncode": 0,
                    "stdout": "",
                    "stderr": "",
                    "timed_out": False,
                    "spawn_error": "",
                    "status": "principal_registration_error",
                    "root_kind": "other",
                    "kernel_axioms": [],
                    "boundary_theorems": [],
                    "strict_theorem_deps": [],
                    "strict_definition_deps": [],
                    "errors": [error],
                },
                0,
                {"principal_registration_error": True},
            )
        if not scan_only and (
            (not principal_name and not module_owner)
            or len(principal_name) > 1024
            or any(ch.isspace() or ord(ch) < 0x20 for ch in principal_name)
        ):
            error = "full node-certificate probe requires a valid exact principal_name registration"
            return (
                {
                    "request_id": rid,
                    "node": node_name,
                    "returncode": 0,
                    "stdout": "",
                    "stderr": "",
                    "timed_out": False,
                    "spawn_error": "",
                    "status": "principal_registration_error",
                    "root_kind": "other",
                    "kernel_axioms": [],
                    "boundary_theorems": [],
                    "strict_theorem_deps": [],
                    "strict_definition_deps": [],
                    "errors": [error],
                },
                0,
                {"principal_registration_error": True},
            )
        if not scan_only:
            replay_failure = self._ensure_closures_have_current_kernel_replay(
                [node_name], timeout_secs=request.timeout_secs
            )
            if replay_failure is not None:
                error = str(replay_failure.get("stderr", "") or "") or str(
                    replay_failure.get("spawn_error", "") or ""
                )
                response = {
                    "request_id": rid,
                    "node": node_name,
                    "returncode": replay_failure.get("returncode"),
                    "stdout": str(replay_failure.get("stdout", "") or ""),
                    "stderr": str(replay_failure.get("stderr", "") or ""),
                    "timed_out": bool(replay_failure.get("timed_out", False)),
                    "spawn_error": str(replay_failure.get("spawn_error", "") or ""),
                    "status": "internal_error",
                    "root_kind": "other",
                    "kernel_axioms": [],
                    "boundary_theorems": [],
                    "strict_theorem_deps": [],
                    "strict_definition_deps": [],
                    "errors": [error or "kernel replay precondition failed"],
                }
                return (
                    response,
                    replay_failure.get("returncode"),
                    {"kernel_replay_precondition_failed": True},
                )
        script_args = [str(script_path), node_name]
        if not scan_only:
            if module_owner:
                script_args.append("--module-owner")
            else:
                script_args.append(f"--principal={principal_name}")
        if no_axcheck:
            script_args.append("--no-axcheck")
        if scan_only:
            script_args.append("--scan-only")

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
        replay_ready = (
            scan_only
            or self._closures_have_current_kernel_replay([node_name])
        )
        # Source-policy scan-only is uncached (see ``script_args`` note).
        # Leaving ``cache_key`` None makes both the lock-acquired load
        # double-check and the post-probe store below no-ops.
        if (
            not scan_only
            and replay_ready
            and local_closure_script_sha
            and toolchain_sha
        ):
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
                principal_identity = "<module-owner>" if module_owner else principal_name
                principal_key = hashlib.sha256(principal_identity.encode("utf-8")).hexdigest()
                cache_key = f"{cache_key_base}-principal-{principal_key}"
                if no_axcheck:
                    cache_key += "-noax"

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
            metadata_manifest = list(parsed.get("declaration_manifest", []) or [])
            replay_evidence = (
                None
                if scan_only
                else observations.read_kernel_replay_artifact_evidence(
                    self.supervisor_repo, node_name
                )
            )
            replay_manifest = (
                [] if scan_only else None if replay_evidence is None else replay_evidence["declaration_manifest"]
            )
            visibility_manifests = (
                [] if replay_evidence is None else replay_evidence["visibility_manifests"]
            )
            direct_imports = (
                []
                if scan_only
                else None
                if replay_evidence is None
                else replay_evidence["direct_imports"]
            )
            artifact_bundle = (
                [] if replay_evidence is None else replay_evidence["artifact_bundle"]
            )
            metadata_by_key = {
                (str(entry.get("name", "")), str(entry.get("kind", ""))): dict(entry)
                for entry in metadata_manifest
                if isinstance(entry, dict)
            }
            replay_by_key = {
                (str(entry.get("name", "")), str(entry.get("kind", ""))): dict(entry)
                for entry in (replay_manifest or [])
                if isinstance(entry, dict)
            }
            closure_only = [
                metadata_by_key[key]
                for key in sorted(metadata_by_key.keys() - replay_by_key.keys())
            ]
            replay_only = [
                replay_by_key[key]
                for key in sorted(replay_by_key.keys() - metadata_by_key.keys())
            ]
            manifest_gap = None
            parsed_errors = list(parsed.get("errors", []) or [])
            parsed_status = str(
                parsed.get("status", "internal_error") or "internal_error"
            )
            source_sha256 = ""
            if not scan_only:
                try:
                    source_sha256 = hashlib.sha256(
                        (self.supervisor_repo / "Tablet" / f"{node_name}.lean").read_bytes()
                    ).hexdigest()
                except OSError as exc:
                    parsed_status = "artifact_binding_error"
                    parsed_errors.append(
                        f"cannot bind closure evidence to source/artifact bytes: {exc}"
                    )
            if not scan_only and (
                replay_manifest is None or closure_only or replay_only
            ):
                manifest_gap = {
                    "closure_only": closure_only,
                    "replay_only": replay_only,
                }
                parsed_status = "manifest_gap"
                parsed_errors.append(
                    "ManifestGap: artifact-local closure manifest differs from the replay-attested ModuleData.constants manifest"
                )
            response: Dict[str, Any] = {
                "request_id": rid,
                "node": node_name,
                "returncode": returncode,
                "stdout": stdout,
                "stderr": stderr,
                "timed_out": timed_out,
                "spawn_error": spawn_error,
                # Script fields preserved verbatim per plan §5.3.
                "status": parsed_status,
                "root_kind": str(parsed.get("root_kind", "other") or "other"),
                "principal_declaration": str(parsed.get("principal_declaration", "") or ""),
                "declaration_manifest": list(parsed.get("declaration_manifest", []) or []),
                "replay_declaration_manifest": list(replay_manifest or []),
                "direct_imports": direct_imports,
                "visibility_manifests": list(visibility_manifests),
                "artifact_bundle": list(artifact_bundle),
                "exact_declaration_uses": list(parsed.get("exact_declaration_uses", []) or []),
                "source_sha256": source_sha256,
                "kernel_axioms": list(parsed.get("kernel_axioms", []) or []),
                "boundary_theorems": list(parsed.get("boundary_theorems", []) or []),
                "strict_theorem_deps": list(parsed.get("strict_theorem_deps", []) or []),
                "strict_definition_deps": list(
                    parsed.get("strict_definition_deps", []) or []
                ),
                "errors": parsed_errors,
            }
            if manifest_gap is not None:
                response["manifest_gap"] = manifest_gap
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
            and self._closures_have_current_kernel_replay([node_name])
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
            cache_key_post = None
            if cache_key_base_post is not None:
                principal_identity = "<module-owner>" if module_owner else principal_name
                principal_key = hashlib.sha256(principal_identity.encode("utf-8")).hexdigest()
                cache_key_post = f"{cache_key_base_post}-principal-{principal_key}"
                if no_axcheck:
                    cache_key_post += "-noax"
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
        principal_names_raw = request.raw.get("principal_names")
        principal_names = (
            {
                node: str(principal_names_raw.get(node, "")).strip()
                for node in requested
            }
            if isinstance(principal_names_raw, dict)
            else {}
        )
        missing_principals = [
            node for node in requested if not principal_names.get(node)
        ]
        if missing_principals:
            message = (
                "exact principal registration missing for semantic fingerprint node(s): "
                + ", ".join(missing_principals)
            )
            return (
                {
                    "request_id": rid,
                    "nodes": {
                        node: {"ok": False, "payload": "", "error": message}
                        for node in requested
                    },
                },
                None,
                {"principal_registration_error": True},
            )

        replay_failure = self._ensure_closures_have_current_kernel_replay(
            requested, timeout_secs=request.timeout_secs
        )
        if replay_failure is not None:
            error = str(replay_failure.get("stderr", "") or "") or str(
                replay_failure.get("spawn_error", "") or ""
            )
            response = {
                "request_id": rid,
                "nodes": {
                    node_name: {
                        "ok": False,
                        "payload": "",
                        "error": error or "kernel replay precondition failed",
                    }
                    for node_name in requested
                },
            }
            return (
                response,
                replay_failure.get("returncode"),
                {"kernel_replay_precondition_failed": True},
            )

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
            replay_ready = self._closures_have_current_kernel_replay([node_name])
            if replay_ready and script_sha and toolchain_sha:
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
                    if cache_key is not None:
                        cache_key += "-principal-" + hashlib.sha256(
                            principal_names[node_name].encode("utf-8")
                        ).hexdigest()
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
                principal_names={node: principal_names[node] for node in misses},
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
                if not self._closures_have_current_kernel_replay([node_name]):
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
                    if key_post is not None:
                        key_post += "-principal-" + hashlib.sha256(
                            principal_names[node_name].encode("utf-8")
                        ).hexdigest()
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

    # --------------------------- Isabelle/HOL ops (B2a) ---------------------------

    def _isabelle_session_dir(self) -> Path:
        """Directory holding the per-tablet Isabelle session scaffold.

        The session scaffold (``ROOT`` + ``Tablet_Preamble.thy`` + per-node
        ``Tablet_<Node>.thy``) lives here; ``isabelle_sync_session`` (B2d)
        renders ``ROOT`` + the Preamble base into it and ``isabelle_build_
        session`` builds its heap. The descriptor's ``umbrella_relpath`` is
        ``"ROOT"`` (a file at the session root). The convention is
        ``<supervisor_repo>/isabelle/`` so the Isabelle theories never collide
        with the Lean ``Tablet/`` tree. Derived from the socket-resolved
        supervisor repo — never from any request field (the trust property).
        """
        return self.supervisor_repo / "isabelle"

    def _isabelle_theory_for_node(self, node_name: str) -> str:
        return f"Tablet_{node_name}"

    def _isabelle_warm_cfg(self) -> Any:
        """Load + cache the warm-gate config (flag OFF by default).

        Read once per server lifetime from
        ``<supervisor_repo>/trellis.config.json`` (``isabelle_warm_session``
        object); the env switch ``TRELLIS_ISABELLE_WARM_SESSION`` overrides
        ``enabled`` so an operator can flip it without editing the file. A
        missing file/object → disabled, so the capability stays inert until
        opted in. Cached so the per-op gate path does not re-read the file.
        """
        if self._isabelle_warm_config is None:
            from trellis.checker.isabelle_warm_config import (
                IsabelleWarmSessionConfig,
                env_warm_session_enabled,
            )
            from trellis.project_paths import project_config_path

            cfg = IsabelleWarmSessionConfig.load(
                project_config_path(self.supervisor_repo)
            )
            # The env switch is the low-level override the session itself reads;
            # honor it at the config layer too so the gate decision agrees with
            # the session's own ``warm_prefix_enabled``.
            if env_warm_session_enabled() and not cfg.enabled:
                from dataclasses import replace

                cfg = replace(cfg, enabled=True)
            self._isabelle_warm_config = cfg
        return self._isabelle_warm_config

    def _get_or_create_isabelle_session(self) -> Any:
        """Return the warm HOL session, creating it on first use.

        Holds ``_isabelle_lock`` for the spawn so two threads cannot race
        on the registry / socket. Caller already holds ``_workspace_lock``
        (Isabelle ops dispatch inside it), so contention here is nil in
        practice; the lock is correctness insurance, not a hot path.

        When the warm gate is ON the session is constructed with the
        node-level warm prefix enabled (Phase 1's ``warm_prefix_enabled`` +
        the tuned ``consolidate_delay``); when OFF (the default) it is the
        plain cold session, byte-identical to before Phase 2.
        """
        from trellis.checker.isabelle_session import IsabelleSession

        cfg = self._isabelle_warm_cfg()
        # The scaffold dir defines the ``Tablet_Base`` base session the warm
        # ``session_start`` parents on; pass it as ``dirs`` so the server can
        # resolve the session (with ``system_heaps=true``, set inside the
        # session, the prebuilt base image is reused from the system heaps —
        # never rebuilt).
        session_dirs = [str(self._isabelle_session_dir())]
        with self._isabelle_lock:
            if self._isabelle_session is None:
                if cfg.enabled:
                    session = IsabelleSession(
                        name=self._isabelle_server_name,
                        session_dirs=session_dirs,
                        warm_prefix_enabled=True,
                        consolidate_delay=cfg.consolidate_delay,
                    )
                else:
                    session = IsabelleSession(
                        name=self._isabelle_server_name,
                        session_dirs=session_dirs,
                    )
                session.start()
                self._isabelle_session = session
            return self._isabelle_session

    def _create_cold_isabelle_session(self) -> Any:
        """Spawn a FRESH, started, flag-OFF Isabelle session (cold channel).

        The warm gate's cold-fallback + cold-cross-check channel: a plain
        ``IsabelleSession`` (no warm prefix) that re-elaborates the node graph
        from source — the pre-Phase-2 behavior. Named distinctly from the warm
        server so the two never collide on a ``servers.db`` row; reaped BY NAME
        by the gate's ``finally``. Caller holds ``_workspace_lock``, and the gate
        reaps the warm session before spawning this, so only one ``isabelle``
        process is live during a cold check.
        """
        from trellis.checker.isabelle_session import IsabelleSession

        cold_name = f"{self._isabelle_server_name}-cold"
        session = IsabelleSession(
            name=cold_name,
            session_dirs=[str(self._isabelle_session_dir())],
            # EXPLICITLY cold. `warm_prefix_enabled` defaults to the
            # `TRELLIS_ISABELLE_WARM_SESSION` env switch, so omitting it made the
            # "cold" channel inherit the warm setting on exactly the runs where
            # the gate is enabled — the cross-check was comparing a warm verdict
            # against a second warm-configured session, which is the one thing it
            # exists to avoid.
            #
            # Every op on the cold path goes through `check_theory`, which carries
            # no warm requirement, so a genuinely cold session runs them all. The
            # soundness floor is unaffected: `quick_and_dirty` defaults to `false`
            # in Isabelle, so the cold session rejects `sorry` at load without the
            # explicit option the warm path sends. What is given up is the
            # content-keyed alias — unnecessary here, since this session is
            # discarded after the check and cannot hold a residency-stale copy —
            # and the tuned consolidation delay, which costs latency on a path
            # that runs on a cadence rather than per check.
            warm_prefix_enabled=False,
        )
        session.start()
        return session

    def _get_or_create_isabelle_warm_gate(self) -> Any:
        """Return the warm gate, creating it on first use (flag ON only).

        Wires the gate to this server's session lifecycle: the warm-session
        factory is ``_get_or_create_isabelle_session`` (re-creates a reaped
        warm session lazily), the reaper nulls the held handle, and the
        cold-session factory is ``_create_cold_isabelle_session`` (the gate's
        cross-check recomputes the cert NODE-SCOPED on a fresh cold session,
        matching the warm verdict's scope — it does not drive a whole-session
        ``isabelle build``; the kernel's separate per-cert-op spine
        ``isabelle_build_session`` remains the phase-aware whole-tablet gate).
        Cached for the server lifetime.
        """
        if self._isabelle_warm_gate is None:
            from trellis.checker.isabelle_warm_gate import IsabelleWarmGate

            self._isabelle_warm_gate = IsabelleWarmGate(
                session_dir=self._isabelle_session_dir(),
                runtime_root=self.runtime_root,
                config=self._isabelle_warm_cfg(),
                warm_session_factory=self._get_or_create_isabelle_session,
                reap_warm_session=self._close_isabelle_session,
                cold_session_factory=self._create_cold_isabelle_session,
            )
        return self._isabelle_warm_gate

    def _close_isabelle_session(self) -> None:
        """Reap the Isabelle session BY NAME. Best-effort + idempotent."""
        with self._isabelle_lock:
            session = self._isabelle_session
            self._isabelle_session = None
        if session is not None:
            try:
                session.close()
            except Exception:
                _LOGGER.exception("error closing Isabelle session")

    def _dispatch_isabelle_op(
        self, request: CheckerRequest
    ) -> Tuple[Mapping[str, Any], Any, Mapping[str, Any]]:
        """Route an Isabelle op to its handler, holding the session internally.

        Runs under ``_workspace_lock`` (the dispatcher's lake-serialization
        lock), so the single warm HOL session + its one TCP socket are
        used serially. The supervisor repo is socket-derived; the request
        carries only the node name (the trust property forbids
        ``repo_path``, enforced in ``validate_request``).
        """
        op = request.op

        if op in {"isabelle_check_node", "isabelle_thm_oracles", "isabelle_thm_deps"}:
            assert request.node_name is not None
            return self._handle_isabelle_node_op(request)
        if op == "isabelle_warm_advisory":
            assert request.node_name is not None
            return self._handle_isabelle_warm_advisory(request)
        if op in {"isabelle_build_session", "isabelle_sync_session"}:
            return self._handle_isabelle_workspace_op(request)
        raise ProtocolError("unknown_op", f"dispatcher missing Isabelle op {op!r}")

    def _handle_isabelle_node_op(
        self, request: CheckerRequest
    ) -> Tuple[Mapping[str, Any], Any, Mapping[str, Any]]:
        op = request.op
        rid = request.request_id
        node_name = request.node_name
        assert node_name is not None

        session_dir = self._isabelle_session_dir()
        theory = self._isabelle_theory_for_node(node_name)
        thy_path = session_dir / f"{theory}.thy"
        # The cert theorem name convention is the node name (B2d's scaffold
        # emits ``lemma <node> : ...``). For B2a the theory scaffold is
        # produced by B2d; if it is absent (the production state today, no
        # IsabelleHol tablet live), fail closed with a structured envelope
        # rather than driving the session against a missing file.
        if not thy_path.is_file():
            message = (
                f"Isabelle theory scaffold absent: {thy_path} "
                f"(per-tablet ROOT/Tablet_<Node>.thy generation is B2d)"
            )
            if op == "isabelle_thm_deps":
                response = {
                    "request_id": rid,
                    "node": node_name,
                    **isabelle_observations._internal_error_cert(message),
                }
            else:
                response = {
                    "request_id": rid,
                    "node": node_name,
                    **isabelle_observations.external_command_envelope(
                        returncode=None, stderr=message, spawn_error=message
                    ),
                }
            return (
                response,
                response.get("returncode"),
                {"isabelle_scaffold_missing": True},
            )

        # Phase-2 warm gate (flag ON only). When enabled, route the node check
        # through the warm session + cold backstop. When OFF (the default), this
        # branch is skipped entirely and the cold path below is byte-for-byte
        # unchanged. The gate manages its own session creation / anomaly→cold
        # fallback / cadence cross-check, so it owns the session-start handling
        # for the warm path; a gate-level failure to even construct the gate
        # falls back to the cold path below (fail-safe to the proven path).
        if self._isabelle_warm_cfg().enabled:
            warm_result = self._handle_isabelle_node_op_warm(request, theory)
            if warm_result is not None:
                return warm_result
            # warm_result is None only if the gate could not be constructed at
            # all (e.g. an import error) — fall through to the cold path so a
            # node is still checked rather than dropped.

        try:
            session = self._get_or_create_isabelle_session()
        except Exception as exc:
            message = f"could not start Isabelle session: {exc}"
            _LOGGER.exception("Isabelle session start failed")
            if op == "isabelle_thm_deps":
                response = {
                    "request_id": rid,
                    "node": node_name,
                    **isabelle_observations._internal_error_cert(message),
                }
            else:
                response = {
                    "request_id": rid,
                    "node": node_name,
                    **isabelle_observations.external_command_envelope(
                        returncode=None, stderr=message, spawn_error=message
                    ),
                }
            return (response, response.get("returncode"), {"isabelle_spawn_failed": True})

        master_dir = str(session_dir)
        cert_theorem = node_name
        # SERVER-side fully-qualified principal name ``Tablet_<Node>.<node>``
        # (B2c-gate Slice 1 / S1). Constructed HERE from the socket-derived
        # theory + node name — never from worker text. The thm_oracles /
        # thm_deps ops resolve the theorem by THIS name in a CHECKER-OWNED
        # probe theory and build the cert from the probe outcome, so the
        # worker can neither author, omit, nor redirect its certificate.
        qualified_thm = f"{theory}.{cert_theorem}"
        if op == "isabelle_check_node":
            payload = isabelle_observations.check_node_server_side(
                session,
                master_dir=master_dir,
                theory=theory,
                cert_theorem=cert_theorem,
                timeout_secs=request.timeout_secs,
            )
        elif op == "isabelle_thm_oracles":
            payload = isabelle_observations.thm_oracles_server_side(
                session,
                master_dir=master_dir,
                theory=theory,
                cert_theorem=cert_theorem,
                qualified_thm=qualified_thm,
                timeout_secs=request.timeout_secs,
            )
        else:  # isabelle_thm_deps
            payload = isabelle_observations.thm_deps_server_side(
                session,
                master_dir=master_dir,
                theory=theory,
                cert_theorem=cert_theorem,
                qualified_thm=qualified_thm,
                timeout_secs=request.timeout_secs,
            )
        response = {"request_id": rid, "node": node_name, **payload}
        log_extra = {"isabelle_op": op, "isabelle_returncode": payload.get("returncode")}
        if op == "isabelle_thm_deps":
            log_extra["isabelle_status"] = payload.get("status")
        return (response, payload.get("returncode"), log_extra)

    def _handle_isabelle_node_op_warm(
        self, request: CheckerRequest, theory: str
    ) -> Optional[Tuple[Mapping[str, Any], Any, Mapping[str, Any]]]:
        """Phase-2 warm-gate node check (flag ON). Returns the same triple shape.

        Constructs (cached) the warm gate and routes the op through it
        (reconcile → warm check → anomaly→cold fallback → cadence cold
        cross-check that HALTS on warm-vs-cold mismatch). Returns ``None`` ONLY
        if the gate itself cannot be constructed (so the caller falls back to
        the proven cold path rather than dropping the check). A gate-internal
        anomaly is already absorbed into the cold fallback, so the gate returns
        a sound envelope in every normal case.
        """
        op = request.op
        rid = request.request_id
        node_name = request.node_name
        assert node_name is not None
        cert_theorem = node_name
        qualified_thm = f"{theory}.{cert_theorem}"
        try:
            gate = self._get_or_create_isabelle_warm_gate()
        except Exception:  # noqa: BLE001 — never let gate-construction drop a check
            _LOGGER.exception(
                "isabelle warm gate construction failed — falling back to cold "
                "for node=%s",
                node_name,
            )
            return None
        payload, used_cold_fallback = gate.run_node_op(
            op=op,
            node_name=node_name,
            theory=theory,
            cert_theorem=cert_theorem,
            qualified_thm=qualified_thm,
            timeout_secs=request.timeout_secs,
        )
        response = {"request_id": rid, "node": node_name, **payload}
        log_extra = {
            "isabelle_op": op,
            "isabelle_returncode": payload.get("returncode"),
            "isabelle_warm_gate": True,
            "isabelle_cold_fallback": used_cold_fallback,
        }
        if op == "isabelle_thm_deps":
            log_extra["isabelle_status"] = payload.get("status")
        return (response, payload.get("returncode"), log_extra)

    def _handle_isabelle_warm_advisory(
        self, request: CheckerRequest
    ) -> Tuple[Mapping[str, Any], Any, Mapping[str, Any]]:
        """Phase-3 worker inner-loop advisory (warm-elaborate only the node).

        The Isabelle analogue of the Lean ``incremental-check`` warm pre-check:
        the worker drives this in its edit/check loop to get a sub-second
        pass/fail+errors against the warm accepted prefix, WITHOUT the cert /
        cross-check (that is the deterministic ``isabelle_thm_deps`` gate's job,
        not the inner loop). The response is the advisory shape
        ``{ok, seconds, errors, advisory_unavailable}``.

        This is meaningful only when the warm flag is ON (the warm session holds
        the accepted prefix). When OFF (the default), there is no warm prefix to
        check against, so the advisory is structurally unavailable: return
        ``advisory_unavailable=True`` so the worker falls back to ``isabelle
        build``. The Lean/flag-OFF path is therefore inert here — no warm session
        is created, byte-for-byte as before.
        """
        rid = request.request_id
        node_name = request.node_name
        assert node_name is not None
        theory = self._isabelle_theory_for_node(node_name)

        if not self._isabelle_warm_cfg().enabled:
            # Warm capability off → no warm prefix exists → the advisory cannot
            # add value over `isabelle build`. Inert: do NOT spawn a session.
            response = {
                "request_id": rid,
                "node": node_name,
                "ok": False,
                "seconds": 0.0,
                "errors": ["isabelle warm advisory is disabled (warm flag off)"],
                "advisory_unavailable": True,
            }
            return (
                response,
                None,
                {"isabelle_op": request.op, "isabelle_warm_advisory_disabled": True},
            )

        session_dir = self._isabelle_session_dir()
        thy_path = session_dir / f"{theory}.thy"
        # The worker's in-flight `Tablet/<node>.thy` reaches the supervisor via
        # `sync_tablet_dir` (run pre-dispatch); the advisory projects it into the
        # session dir itself (the gate's `run_warm_advisory` does the project),
        # so an absent scaffold means the worker has not authored/synced the node
        # yet — advise fallback rather than drive a missing file.
        if not (session_dir / "Tablet").is_dir() and not thy_path.is_file():
            response = {
                "request_id": rid,
                "node": node_name,
                "ok": False,
                "seconds": 0.0,
                "errors": [f"Isabelle theory not synced yet: {thy_path.name}"],
                "advisory_unavailable": True,
            }
            return (
                response,
                None,
                {"isabelle_op": request.op, "isabelle_warm_advisory_unsynced": True},
            )

        try:
            gate = self._get_or_create_isabelle_warm_gate()
        except Exception:  # noqa: BLE001 — advisory must never raise into dispatch
            _LOGGER.exception(
                "isabelle warm gate construction failed for advisory node=%s — "
                "advising isabelle build fallback",
                node_name,
            )
            response = {
                "request_id": rid,
                "node": node_name,
                "ok": False,
                "seconds": 0.0,
                "errors": ["isabelle warm advisory could not start"],
                "advisory_unavailable": True,
            }
            return (
                response,
                None,
                {"isabelle_op": request.op, "isabelle_warm_advisory_unavailable": True},
            )

        advisory = gate.run_warm_advisory(
            node_name=node_name,
            theory=theory,
            timeout_secs=request.timeout_secs,
        )
        response = {"request_id": rid, "node": node_name, **advisory}
        # An advisory verdict carries no `returncode` (it is not a command
        # envelope): 0 when it ran and the node elaborated, 1 when it ran and the
        # node failed, None when the advisory itself was unavailable.
        if advisory.get("advisory_unavailable"):
            returncode: Optional[int] = None
        else:
            returncode = 0 if advisory.get("ok") else 1
        return (
            response,
            returncode,
            {
                "isabelle_op": request.op,
                "isabelle_warm_advisory": True,
                "isabelle_advisory_ok": bool(advisory.get("ok")),
                "isabelle_advisory_unavailable": bool(
                    advisory.get("advisory_unavailable")
                ),
            },
        )

    def _handle_isabelle_workspace_op(
        self, request: CheckerRequest
    ) -> Tuple[Mapping[str, Any], Any, Mapping[str, Any]]:
        """``isabelle_build_session`` / ``isabelle_sync_session`` (B2d).

        The ``sync-tablet-support`` / ``prepare-compiled-support`` analogues,
        made real by the per-tablet session scaffold generator:

        * ``isabelle_sync_session`` renders + writes the session scaffold
          (``ROOT`` + ``Tablet_Preamble.thy`` + a fail-closed stub for any
          registered node whose worker ``Tablet_<Node>.thy`` is absent) into
          the socket-derived ``<supervisor_repo>/isabelle/`` dir. Pure
          file-writing; the node set is the worker ``Tablet_<Node>.thy`` files
          present there (the ``sync_tablet_support`` render analogue).
        * ``isabelle_build_session`` materializes the session heap image via
          ``isabelle build -b -D <session_dir>`` (the ``materialize-tablet-
          oleans`` analogue) so the warm session is loadable; the prebuilt HOL
          heap means this is light.

        Both derive the session dir from the socket runtime root, never from a
        request field (the ``repo_path``-forbidden trust property).
        """
        op = request.op
        rid = request.request_id
        session_dir = self._isabelle_session_dir()

        if op == "isabelle_sync_session":
            payload = self._isabelle_sync_session(session_dir)
            return (
                {"request_id": rid, **payload},
                payload.get("returncode"),
                {"isabelle_op": op, "isabelle_session_dir": str(session_dir)},
            )

        # isabelle_build_session
        payload = self._isabelle_build_session(
            session_dir, timeout_secs=request.timeout_secs
        )
        return (
            {"request_id": rid, **payload},
            payload.get("returncode"),
            {"isabelle_op": op, "isabelle_session_dir": str(session_dir)},
        )

    def _isabelle_sync_session(self, session_dir: Path) -> Mapping[str, Any]:
        """Render + write the session scaffold; synthesize the command envelope.

        The ``ExternalCommandObservation`` shape the kernel reads: ``returncode``
        0 on a successful render, with the scaffold summary in ``stdout``; a
        write failure fails closed with ``returncode: null`` + ``spawn_error``
        so the kernel's precondition path treats it as a hard failure.
        """
        from trellis.checker import isabelle_scaffold

        try:
            summary = isabelle_scaffold.sync_session(session_dir)
            # Keep the WORKER-facing seed preamble in step with the session's.
            # `<worker_repo>/isabelle/` is written once at setup and nothing
            # refreshed it, so a `Tablet/Preamble.thy` edit reached the checker
            # but not the worker — the worker then compiled against a narrower
            # import root than the gate used. Content-conditional and
            # preamble-only (the seed's ROOT is deliberately node-less).
            try:
                isabelle_scaffold.refresh_worker_seed_preamble(self.worker_repo)
            except Exception:  # noqa: BLE001 — advisory; never fail the sync
                _LOGGER.exception("worker seed preamble refresh failed")
        except Exception as exc:  # noqa: BLE001 — fail closed, surface the cause
            message = f"isabelle session scaffold sync failed: {exc}"
            _LOGGER.exception("isabelle_sync_session failed")
            return isabelle_observations.external_command_envelope(
                returncode=None, stderr=message, spawn_error=message
            )
        return isabelle_observations.external_command_envelope(
            returncode=0,
            stdout=json.dumps(summary, separators=(",", ":")),
            stderr="",
        )

    def _isabelle_build_session(
        self, session_dir: Path, *, timeout_secs: float
    ) -> Mapping[str, Any]:
        """Build the per-tablet session heap (``isabelle build -b -D <dir>``).

        Synthesizes the ``ExternalCommandObservation`` the kernel reads:
        ``returncode`` is the build exit code (0 = the session image built and
        every theory checked; non-zero = a proof/parse failure — a stray
        ``sorry`` fails because ``quick_and_dirty = false``); a spawn failure or
        timeout fails closed.

        No-op-success when no scaffold ``ROOT`` exists yet (the production state
        — no IsabelleHol tablet live): there is nothing to build, and the
        prebuilt HOL heap needs no work, so the kernel's precondition path is
        satisfied without driving ``isabelle build`` against a missing ROOT.
        """
        import subprocess

        from trellis.checker.isabelle_session import isabelle_bin

        if not (session_dir / "ROOT").is_file():
            return isabelle_observations.external_command_envelope(
                returncode=0,
                stdout="",
                stderr="",
            )

        # Cap Isabelle parallelism so a session build cannot saturate a shared
        # host (Isabelle defaults to threads=0 = all cores). Tunable via env;
        # this is the in-code backstop to the install-level settings cap.
        # Defaults to "2" to match the scaffold ROOT's ``threads=2``.
        import os
        import shutil

        threads = os.environ.get("TRELLIS_ISABELLE_THREADS", "2")
        # ``system_heaps=true`` makes the build READ AND WRITE the SHARED system
        # heaps (``$ISABELLE_HOME/heaps``) where the prebuilt
        # HOL-Analysis→HOL-Probability→Tablet_Base→Tablet chain already lives, so
        # an up-to-date session is reused in seconds rather than rebuilt from
        # scratch into the per-user heaps (which would saturate the host). The
        # system heaps are writable; that is where Tablet_Base/Tablet already
        # build to. The warm ``session_start`` resolves the SAME base from these
        # system heaps, so cold-build and warm-session agree on one image set.
        # MATERIALIZATION, not the soundness verdict — hence `quick_and_dirty=true`.
        #
        # This op stands in for Lean's `materialize-tablet-oleans` /
        # `prepare-compiled-support`. Both are artifact-materialization steps, and
        # in Lean a `sorry` compiles with a WARNING: `lake build` succeeds and the
        # `sorry` is caught downstream by `#print axioms` -> `sorryAx`. The ROOT
        # pins `quick_and_dirty=false`, under which Isabelle makes `sorry` a hard
        # ERROR, so the same conceptual step became fatal here — and because this
        # build is whole-session, ONE open node made the entire tablet
        # unmaterializable.
        #
        # That contradicts the phase contract: nodes are not all expected to be
        # closed until the run ends (a node may legitimately be OPEN, stated with
        # its proof deferred), and `validate_node_shape` deliberately permits
        # `sorry` as the open-proof marker. A live run halted on exactly this.
        #
        # Soundness does not rest on this build. The per-node certificate loads
        # the node's import cone through PIDE with `quick_and_dirty=false` and
        # materializes an explicit `sorry` as the `skip_proof` oracle, which the
        # cut traversal then attributes to the proof that introduced it. A strict
        # whole-session `quick_and_dirty=false` build remains the COMPLETION gate,
        # run once no node is open — it is just not a per-certificate precondition.
        build_cmd = [
            isabelle_bin(),
            "build",
            "-b",
            "-o",
            "system_heaps=true",
            "-o",
            "quick_and_dirty=true",
            "-o",
            f"threads={threads}",
            "-D",
            str(session_dir),
        ]
        # Nice the spawn so an Isabelle build can never starve a load-sensitive
        # shared host: CPU priority via ``nice -n 15`` and (best-effort) idle I/O
        # priority via ``ionice -c3``. Both are optional on the host; missing
        # tools degrade to the next-best prefix rather than failing the build.
        prefix: list[str] = []
        ionice = shutil.which("ionice")
        if ionice:
            prefix += [ionice, "-c3"]
        nice = shutil.which("nice")
        if nice:
            prefix += [nice, "-n", "15"]
        cmd = [*prefix, *build_cmd]
        # Acceptance sub-progress heartbeat (the Lean `build-tablet` analogue):
        # the whole-session heap build is the longest single Isabelle step, so
        # emit start/finish through the shared progress channel. A no-op when
        # `TRELLIS_ACCEPTANCE_PROGRESS_LOG` is unset (the usual case for the
        # long-lived server process).
        build_started = time.time()
        observations._progress_emit(
            "[acceptance]   isabelle-build-session: starting (whole-session heap build)"
        )
        try:
            proc = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=max(1.0, float(timeout_secs)),
            )
        except subprocess.TimeoutExpired as exc:
            message = f"isabelle build timed out after {timeout_secs}s: {exc}"
            observations._progress_emit(
                f"[acceptance]   isabelle-build-session: timed out after "
                f"{time.time() - build_started:.1f}s"
            )
            return isabelle_observations.external_command_envelope(
                returncode=None, stderr=message, timed_out=True, spawn_error=message
            )
        except OSError as exc:
            message = f"could not spawn isabelle build: {exc}"
            _LOGGER.exception("isabelle_build_session spawn failed")
            observations._progress_emit(
                f"[acceptance]   isabelle-build-session: spawn failed in "
                f"{time.time() - build_started:.1f}s"
            )
            return isabelle_observations.external_command_envelope(
                returncode=None, stderr=message, spawn_error=message
            )
        observations._progress_emit(
            f"[acceptance]   isabelle-build-session: done "
            f"returncode={proc.returncode} in {time.time() - build_started:.1f}s"
        )
        return isabelle_observations.external_command_envelope(
            returncode=proc.returncode,
            stdout=proc.stdout or "",
            stderr=proc.stderr or "",
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
