"""Atomic external-tool and fingerprint actions for the checker boundary.

These helpers only gather raw facts for the Rust kernel. They do not decide
validity, classify failures, or synthesize acceptance results.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
import tempfile
import threading
import time
import uuid
from pathlib import Path
from typing import Any, Dict, Mapping, Optional, Sequence, Tuple

from trellis.atomic_actions.checker_client import (
    _resolve_socket_path,
    client_build_tablet,
    client_compile_node,
    client_lean_semantic_payloads,
    client_materialize_tablet_oleans,
    client_prepare_compiled_support,
    client_print_axioms,
)
from trellis.checker.protocol import MAX_NODES_PER_REQUEST
from trellis.project_paths import repo_tmp_subdir
from trellis.supervisor_workspace import authoritative_env_for_repo

_BARE_MATHLIB_IMPORT_RE = re.compile(r"^\s*import\s+Mathlib\s*$")
LEAN_SUPPORT_TIMEOUT_SECS = 3600.0

# Sub-progress channel for the worker-side acceptance checker.
#
# The kernel binary emits top-level `[acceptance] phase k/6: ...` lines to
# its own stderr, which `trellis.runtime.kernel_cli._run_kernel_cli_once`
# forwards line-by-line to the calling agent. Inside phase 6/6 ("hydrate
# response and legality check") the kernel shells back out to Python
# observations (this module) via `python3 .../check.py materialize-tablet-oleans`
# and `python3 .../check.py lean-semantic-payloads`, each of which loops
# over nodes for many seconds per node. The kernel's spawn captures the
# child Python's stderr into a buffer (Stdio::piped + cmd.output), so a
# plain `print(..., file=sys.stderr)` from this module is INVISIBLE to the
# calling agent.
#
# To surface per-node sub-progress without rebuilding the kernel binary,
# `_run_kernel_cli_once` creates a tail-and-forward log file before
# spawning the kernel and exports its path via the
# `TRELLIS_ACCEPTANCE_PROGRESS_LOG` env var. The kernel inherits this env
# (no env_clear in run_repo_command_json), so the python3 child here also
# sees it. `_progress_emit` appends one line per call to that file; the
# outer Python tail thread forwards each new line to the agent's stderr
# in real time. When the env var is unset (direct CLI use, tests,
# fast-mode), `_progress_emit` is a no-op.
_PROGRESS_LOG_ENV = "TRELLIS_ACCEPTANCE_PROGRESS_LOG"


def _require_socket_path() -> str:
    """Resolve the checker socket or raise if it is unset.

    Acceptance observations (the six public fns below, when called with
    ``bwrap_role is None``) are socket-mandatory: the unified-checker
    UNIX-socket server is the only supported way to run authoritative
    acceptance lake checks. The legacy direct-host-lake fallback has been
    removed. ``restart_configured_run.sh`` launches the server and exports
    ``TRELLIS_CHECKER_SOCKET``, so in any real run the socket resolves. If it is unset, that is an
    operator misconfiguration — surface it loudly rather than silently
    running lake on the host (which bypasses the server's authority and
    confinement). Mirrors the no-fallback precedent for
    ``local-closure-axioms`` in :mod:`trellis.atomic_actions.cli`.
    """
    socket_path = _resolve_socket_path()
    if socket_path is None:
        raise RuntimeError(
            "checker socket required: TRELLIS_CHECKER_SOCKET unset. "
            "Acceptance lake checks must route through the supervisor-side "
            "checker server (no host-lake fallback). Launch the run via "
            "scripts/restart_configured_run.sh, "
            "which starts the server and exports TRELLIS_CHECKER_SOCKET."
        )
    return socket_path


# Byte budget for the ``nodes`` array of one checker request.
#
# The acceptance path materializes every present Tablet node in a single RPC,
# so the request line grows with the tablet. Two wire caps bound that line:
# ``MAX_NODES_PER_REQUEST`` (1024) and ``MAX_LINE_BYTES`` (64 KiB). A
# live run hit the first one at cycle 1037 — 1023 present nodes, and a
# six-node decomposition asked for 1029 — which halted the run because no node
# could be added. That request line was already 25,699 bytes, 39.2% of
# ``MAX_LINE_BYTES`` at 25.0 bytes/node, so raising the node cap would only
# have moved the halt to the line cap around 2,567 nodes.
#
# Chunking the node list instead keeps both wire constants fixed (no checker
# restart) and holds under growth: the split is driven by encoded bytes, so a
# tablet with longer node names splits earlier. ``NODE_NAME_MAX_LEN`` is 128,
# which puts the worst case at ~187 names per chunk. The 24 KiB budget is
# 37.5% of ``MAX_LINE_BYTES``, leaving the rest of the envelope ample room.
CHECKER_NODE_CHUNK_MAX_BYTES = 24 * 1024

# Per-node wire cost: the UTF-8 name plus its two JSON quotes and separating
# comma.
_NODE_WIRE_OVERHEAD_BYTES = 3


def _chunk_node_names(node_names: Sequence[str]) -> list[list[str]]:
    """Partition ``node_names`` into wire-legal chunks, order preserved.

    A chunk is flushed when adding the next name would exceed either
    ``CHECKER_NODE_CHUNK_MAX_BYTES`` of encoded node-name bytes or
    ``MAX_NODES_PER_REQUEST`` names. An empty input yields one empty chunk:
    an empty ``nodes`` list means "every present node" to the server, so it
    must still be sent. Names are neither deduplicated nor reordered, so the
    concatenation of the chunks is the caller's list.
    """
    chunks: list[list[str]] = []
    current: list[str] = []
    current_bytes = 0
    for name in node_names:
        cost = len(str(name).encode("utf-8")) + _NODE_WIRE_OVERHEAD_BYTES
        would_exceed_bytes = current_bytes + cost > CHECKER_NODE_CHUNK_MAX_BYTES
        would_exceed_count = len(current) + 1 > MAX_NODES_PER_REQUEST
        if current and (would_exceed_bytes or would_exceed_count):
            chunks.append(current)
            current = []
            current_bytes = 0
        current.append(name)
        current_bytes += cost
    chunks.append(current)
    return chunks


def _merge_materialize_chunks(
    chunk_payloads: Sequence[Dict[str, Any]],
) -> Dict[str, Any]:
    """Fold per-chunk ``materialize_oleans`` responses into one payload.

    The kernel inspects this surface via ``tablet_support::ensure_external_command_ok``,
    so a chunked run must present exactly what a single call would:
    ``requested_nodes`` concatenated, ``materialized_nodes`` unioned in
    first-seen order, the first non-zero ``returncode``, concatenated output
    streams, ``timed_out`` if any chunk timed out, and the first non-empty
    ``spawn_error``.
    """
    merged: Dict[str, Any] = dict(chunk_payloads[0])
    # Elaboration cost is per-lake-invocation, and a chunked call is several.
    # Drop the base chunk's entry and re-add one only when exactly one chunk
    # measured anything; two measurements cannot both ride a single payload,
    # and keeping an arbitrary one would look like a whole-request number.
    merged.pop("cost", None)
    chunk_costs = [
        payload["cost"]
        for payload in chunk_payloads
        if isinstance(payload.get("cost"), dict)
    ]
    if len(chunk_costs) == 1:
        merged["cost"] = dict(chunk_costs[0])
    # A floor-violation marker (see `_ELABORATION_PEAK_RSS_FLOOR_KIB`) is a
    # per-invocation diagnostic; keep the first so a chunked request still
    # surfaces that a measurement was dropped rather than never taken.
    merged.pop("cost_floor_violation", None)
    for payload in chunk_payloads:
        if isinstance(payload.get("cost_floor_violation"), dict):
            merged["cost_floor_violation"] = dict(payload["cost_floor_violation"])
            break
    # Summed, so the logged count still describes everything the request
    # elaborated. A chunked request that measured one node reports the total
    # stale count across chunks, which is the honest reading: the cost is
    # attributable, the request as a whole built more than one node. Absent
    # when no chunk reported one — this merge presents exactly what a single
    # call would, so it must not invent a zero the responder never gave.
    chunk_stale_counts = [
        payload["stale_node_count"]
        for payload in chunk_payloads
        if isinstance(payload.get("stale_node_count"), int)
    ]
    if chunk_stale_counts:
        merged["stale_node_count"] = sum(chunk_stale_counts)
    else:
        merged.pop("stale_node_count", None)
    requested: list[str] = []
    materialized: list[str] = []
    seen: set[str] = set()
    returncode = 0
    stdout_parts: list[str] = []
    stderr_parts: list[str] = []
    timed_out = False
    spawn_error = ""
    for payload in chunk_payloads:
        requested.extend(payload.get("requested_nodes") or [])
        for name in payload.get("materialized_nodes") or []:
            if name not in seen:
                seen.add(name)
                materialized.append(name)
        if returncode == 0:
            returncode = payload.get("returncode") or 0
        stdout_parts.append(str(payload.get("stdout") or ""))
        stderr_parts.append(str(payload.get("stderr") or ""))
        timed_out = timed_out or bool(payload.get("timed_out"))
        if not spawn_error:
            spawn_error = str(payload.get("spawn_error") or "")
    merged["requested_nodes"] = requested
    merged["materialized_nodes"] = materialized
    merged["returncode"] = returncode
    merged["stdout"] = "".join(stdout_parts)
    merged["stderr"] = "".join(stderr_parts)
    merged["timed_out"] = timed_out
    merged["spawn_error"] = spawn_error
    return merged


def _progress_emit(message: str) -> None:
    """Best-effort append `message` to the acceptance progress log file.

    No-op when `TRELLIS_ACCEPTANCE_PROGRESS_LOG` is unset (which is the
    case for direct CLI invocations, unit tests, and fast-mode runs).
    Errors are swallowed: progress logging must never break the underlying
    observation. The file is line-buffered so the parent's tail thread
    sees each line as soon as we flush.
    """
    path = os.environ.get(_PROGRESS_LOG_ENV, "").strip()
    if not path:
        return
    try:
        with open(path, "a", encoding="utf-8", buffering=1) as handle:
            handle.write(message.rstrip("\n") + "\n")
    except OSError:
        # The log file may have been cleaned up by the parent already; in
        # that case we just drop the line — the parent has already moved
        # on, so there is nobody to read it.
        pass


def _completed_process_payload(
    proc: subprocess.CompletedProcess[str],
) -> Dict[str, Any]:
    return {
        "returncode": proc.returncode,
        "stdout": proc.stdout,
        "stderr": proc.stderr,
        "timed_out": False,
        "spawn_error": "",
    }


def _timeout_payload(timeout_secs: float, *, command: str) -> Dict[str, Any]:
    message = f"command timed out after {timeout_secs}s: {command}"
    return {
        "returncode": None,
        "stdout": "",
        "stderr": message,
        "timed_out": True,
        "spawn_error": message,
    }


def _spawn_error_payload(exc: Exception, *, command: str) -> Dict[str, Any]:
    return {
        "returncode": None,
        "stdout": "",
        "stderr": f"failed to start command {command}: {exc}",
        "timed_out": False,
        "spawn_error": str(exc),
    }


class _Wait4Popen(subprocess.Popen):
    """``Popen`` that reaps its child via ``os.wait4`` to capture rusage.

    HISTORY, and why this rusage is now DIAGNOSTIC-ONLY: this class was
    introduced to make ``ru_maxrss`` the elaboration-cost peak-RSS
    measurement, on the theory that Linux ``wait4`` folds a child's reaped
    descendants into the returned ``ru_maxrss`` (it takes the max of the
    child's own peak and its ``cmaxrss``). That theory fails under the
    production sandbox — see ``_HostProcPeakSampler`` for the mechanism —
    so ``peak_rss_kib`` now comes from the sampler and ``child_rusage``
    survives only as the ``wait4_maxrss_kib`` diagnostic, kept precisely
    because its divergence from the sampled value is standing evidence
    that it must not be promoted back to the measurement.

    Why not ``resource.getrusage(resource.RUSAGE_CHILDREN)`` either: that
    counter is a high-water mark accumulated over *every* child the
    process has ever reaped, so in the long-lived checker server it is
    monotone non-decreasing and would report the largest build ever seen
    for every subsequent node. ``os.wait4`` at least scopes the rusage to
    the single child we just waited for. ``ru_maxrss`` is kibibytes on
    Linux.

    The override is confined to ``_try_wait``, the one CPython hook that
    calls ``os.waitpid``, so ``communicate`` / ``wait`` / timeout / kill
    semantics remain exactly the stdlib's. If a future CPython drops that
    hook, this method is simply never called: ``child_rusage`` stays
    ``None`` and the diagnostic is absent — the ordinary "not measured"
    state, never an error.
    """

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        # Set before `super().__init__` so a failed spawn still leaves the
        # attribute defined for `__del__` / error paths.
        self.child_rusage: Any = None
        super().__init__(*args, **kwargs)

    def _try_wait(self, wait_flags):  # type: ignore[override]
        """All callers to this function MUST hold ``self._waitpid_lock``."""
        try:
            (pid, sts, rusage) = os.wait4(self.pid, wait_flags)
        except ChildProcessError:
            # Mirrors the stdlib: SIGCLD ignored, or waiting for children
            # otherwise disabled. The child is dead and unmeasurable.
            return (self.pid, 0)
        if pid == self.pid:
            self.child_rusage = rusage
        return (pid, sts)


# Poll interval for `_HostProcPeakSampler`. `VmHWM` is a per-process monotone
# high-water mark, so one successful read near a process's end captures its
# whole-lifetime peak; only a process that lives shorter than one interval can
# be missed entirely, and a real elaboration lives for seconds. One poll over
# a production-shaped tree costs ~0.3 ms, so this is ~0.3% of one core.
_PEAK_RSS_POLL_SECS = 0.1

# Safety bound on one tree walk. The production tree is ~6 processes (bwrap
# monitor -> bwrap init -> lake -> lean -> git ...); the bound only matters if
# a runaway build forks without limit, in which case a truncated walk is the
# right degradation.
_PEAK_RSS_MAX_TREE_PIDS = 512


class _HostProcPeakSampler:
    """Samples the peak RSS of a spawned process TREE from the host side.

    WHY ``os.wait4`` CANNOT MEASURE A SANDBOXED BUILD — do not "simplify"
    this sampler back to rusage. ``wrap_command`` always passes
    ``--unshare-pid``, under which bubblewrap forks a small monitor whose
    child becomes pid-1 init of the new pid namespace, and *that* init
    spawns and reaps the app (`lake`). Linux folds a reaped child's peak
    RSS into the reaper's ``cmaxrss``, so the whole build tree's peak
    accumulates on the namespace init. But bubblewrap's monitor learns the
    app's exit status over an eventfd from init and returns WITHOUT
    reaping init, so init — and the folded build-tree peak it carries — is
    orphaned to the host reaper and never reaches our ``os.wait4``. What
    ``wait4`` returns instead is the monitor's own ``ru_maxrss``, which is
    dominated by copy-on-write pages inherited from THIS process at fork:
    an echo of the checker server's RSS. Confirmed live: the server's
    ``VmHWM`` was byte-identical to consecutive logged ``peak_rss_kib``
    values while `lean` actually peaked 10-90x higher.

    So the honest measurement is taken from outside the sandbox: a pid
    namespace hides nothing from the host's ``/proc``, where every inner
    process appears under its host pid. This thread walks the tree rooted
    at the spawned pid via ``/proc/<pid>/task/*/children`` and reads each
    descendant's ``VmHWM`` from ``/proc/<pid>/status`` (world-readable, so
    the sandbox uid does not matter). It observes only — nothing about the
    spawned command, its environment, or the sandbox is altered.

    Failure semantics: every ``/proc`` read races against process exit and
    degrades to skipping that pid; an unexpected error in the loop marks
    the whole measurement failed, because a thread that died mid-build
    holds only a prefix of the build's history and reporting that prefix
    as "the peak" would be exactly the kind of plausible-looking wrong
    number this sampler exists to eliminate. ``stop()`` then reports no
    measurement — absent is the honest answer.

    Known bounded imprecisions: growth inside the final poll interval
    before a process exits is missed (the prototype measured 0.01% low on
    a production-shaped tree), and a host pid recycled into the tree's
    parent chain in the instant between the app's death and ``stop()``
    could in principle be walked; both are noise against a 10-90x
    systematic error.
    """

    def __init__(self, root_pid: int) -> None:
        self._root_pid = root_pid
        self._stop_event = threading.Event()
        self._thread = threading.Thread(
            target=self._run, name="lake-peak-rss-sampler", daemon=True
        )
        self._best_kib = 0
        self._best_name = ""
        self._failed = False

    def start(self) -> None:
        self._thread.start()

    def stop(self) -> Tuple[Optional[int], str]:
        """Stop sampling and return ``(peak_kib, process_name)``.

        ``(None, "")`` when there is no trustworthy measurement: nothing
        was ever sampled, the thread failed, or it would not stop. The
        join timeout is a backstop only — the loop re-checks the stop
        event every poll interval, so a healthy thread exits within one.
        """
        self._stop_event.set()
        self._thread.join(timeout=5.0)
        if self._failed or self._thread.is_alive() or self._best_kib <= 0:
            return None, ""
        return self._best_kib, self._best_name

    def _run(self) -> None:
        try:
            while True:
                self._sample_once()
                if self._stop_event.wait(_PEAK_RSS_POLL_SECS):
                    break
        except Exception:
            self._failed = True

    def _descendants(self) -> list[int]:
        """Walk the process tree rooted at the spawned pid, root included.

        Reparenting keeps orphans visible: a process whose parent died is
        adopted by the nearest reaper, which inside the sandbox is the pid
        namespace's init — itself a descendant of the root.
        """
        out: list[int] = []
        todo = [self._root_pid]
        seen = {self._root_pid}
        while todo and len(out) < _PEAK_RSS_MAX_TREE_PIDS:
            pid = todo.pop()
            out.append(pid)
            try:
                tasks = os.listdir(f"/proc/{pid}/task")
            except OSError:
                continue  # exited mid-walk — ordinary, skip
            for task in tasks:
                try:
                    with open(f"/proc/{pid}/task/{task}/children") as handle:
                        children = [int(c) for c in handle.read().split()]
                except (OSError, ValueError):
                    continue  # exited mid-walk — ordinary, skip
                for child_pid in children:
                    if child_pid not in seen:
                        seen.add(child_pid)
                        todo.append(child_pid)
        return out

    def _sample_once(self) -> None:
        for pid in self._descendants():
            name = ""
            hwm_kib: Optional[int] = None
            try:
                with open(f"/proc/{pid}/status") as handle:
                    for line in handle:
                        if line.startswith("Name:"):
                            name = line.split(None, 1)[1].strip()
                        elif line.startswith("VmHWM:"):
                            # "VmHWM:  1234 kB"; absent for zombies and
                            # kernel threads, which then stay skipped.
                            hwm_kib = int(line.split()[1])
                            break
            except (OSError, ValueError, IndexError):
                continue  # exited mid-read — ordinary, skip
            if hwm_kib is not None and hwm_kib > self._best_kib:
                self._best_kib = hwm_kib
                self._best_name = name


def _read_self_vm_rss_kib() -> Optional[int]:
    """This process's current ``VmRSS`` in KiB, or ``None``.

    Logged beside each measurement as the anti-echo baseline: under the
    ``wait4`` bug (see ``_HostProcPeakSampler``) reported peaks tracked
    exactly this number, so an operator can grep the request log and
    confirm sampled peaks now decorrelate from the server's own RSS.
    """
    try:
        with open("/proc/self/status") as handle:
            for line in handle:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except (OSError, ValueError, IndexError):
        pass
    return None


# ---------------------------------------------------------------------------
# Heartbeat side-channel (elaboration-cost `heartbeats` field).
#
# Lean heartbeats are an in-process elaborator counter, unreadable at the
# lake-build boundary — which is why `ElaborationCostRecord.heartbeats` was
# always `None`. A Lean plugin loaded by the build closes the gap by
# appending one JSON object per line to the file named by the
# `TRELLIS_HB_OUT` env var. WIRE FORMAT, pinned on both sides:
#
#     {"module": "Tablet.<NodeName>", "decl": "<declName>", "heartbeats": <int>}
#
# `heartbeats` is in user-facing `maxHeartbeats` units (internal count /
# 1000). Extra keys may be present and are ignored; when several lines name
# the same module the LAST line wins. The whole channel is fail-open
# instrumentation: a missing/unreadable/malformed side file yields no
# heartbeat data and never changes a build's outcome.
_HB_OUT_ENV = "TRELLIS_HB_OUT"

# Sanity ceiling on one module's count, in user-facing maxHeartbeats units.
# `maxHeartbeats` defaults to 200_000 per declaration and pathological nodes
# run to a few million; 10**12 is unreachable by many lifetimes of real
# elaboration, so anything above it is corrupt output, not a big build.
_HEARTBEATS_ABSURD_MAX = 10**12


def _new_heartbeat_side_file(repo: Path) -> Optional[Path]:
    """Reserve a per-invocation heartbeat side-file path, or ``None``.

    The path lives directly under ``.lake/build/``, which is writable both
    unsandboxed and inside every bwrap role that runs lake —
    ``sandbox._repo_writable_paths`` binds ``<repo>/.lake/build`` read-write
    for ``lake_compiler`` and the worker roles alike, at the same absolute
    path inside and outside the sandbox — so no new mount is needed. The
    name embeds a fresh UUID so a stale line from an earlier build can never
    be misattributed to this invocation; the caller unlinks the file after
    parsing. Only the path is reserved here — the plugin creates the file,
    so a build without the plugin leaves nothing behind. Best-effort: any
    failure returns ``None`` and the build simply runs unmeasured.
    """
    try:
        parent = repo / ".lake" / "build"
        parent.mkdir(parents=True, exist_ok=True)
        return parent / f"trellis-hb.{uuid.uuid4().hex}.jsonl"
    except OSError:
        return None


def _read_heartbeat_side_file(path: Optional[Path]) -> Dict[str, int]:
    """Parse the plugin side file into ``{module: heartbeats}``. Fail-open.

    Every malformed input — missing file, unreadable file, partial or
    non-JSON line, non-dict line, missing/non-string module, missing,
    boolean, non-integer, negative, or absurdly large count — is skipped
    (or, for file-level errors, yields ``{}``) and never raises. Later
    lines override earlier ones for the same module (last line wins).
    """
    counts: Dict[str, int] = {}
    if path is None:
        return counts
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as handle:
            for line in handle:
                stripped = line.strip()
                if not stripped:
                    continue
                try:
                    entry = json.loads(stripped)
                except ValueError:
                    continue
                if not isinstance(entry, dict):
                    continue
                module = entry.get("module")
                heartbeats = entry.get("heartbeats")
                if not isinstance(module, str) or not module:
                    continue
                if isinstance(heartbeats, bool) or not isinstance(heartbeats, int):
                    continue
                if heartbeats < 0 or heartbeats > _HEARTBEATS_ABSURD_MAX:
                    continue
                counts[module] = heartbeats
    except OSError:
        return {}
    except Exception:
        # Instrumentation must never take down a build; an unexpected
        # parse-side error degrades to "no heartbeat data".
        return {}
    return counts


def _run_lake_command(
    repo: Path,
    args: Sequence[str],
    *,
    timeout_secs: float,
    bwrap_role: Optional[str] = None,
    metrics: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    """Spawn one confined ``lake`` process and return its raw payload.

    ``metrics``, when supplied, is an out-dict this function fills with
    ``walltime_ms`` (a ``time.monotonic`` bracket around the child) and,
    when the host-side sampler observed anything, ``peak_rss_kib`` (max
    ``VmHWM`` over the spawned process tree, kibibytes — see
    ``_HostProcPeakSampler`` for why ``ru_maxrss`` cannot supply this) plus
    ``peak_rss_process`` (the process name that held that maximum; `lean`
    for any real elaboration). Two diagnostics ride along for the
    anti-echo check: ``wait4_maxrss_kib`` (the old, known-wrong rusage
    reading) and ``checker_self_rss_kib`` (this process's own RSS, which
    the rusage reading used to echo). It is populated only for a process
    that ran to completion: a timeout or a spawn failure leaves it
    untouched, so callers record nothing for a build that never produced
    an artifact. The measurement is pure telemetry — no caller may branch
    on it.

    Heartbeat side-channel: a metrics-bearing invocation also exports
    ``TRELLIS_HB_OUT`` (a fresh per-invocation path under ``.lake/build``)
    to the child so the heartbeat plugin, when loaded, can report per-module
    counts; whatever parses cleanly lands in ``metrics`` as
    ``heartbeats_by_module`` (``{module: count}``). Metrics-less callers —
    the read-only olean consumers — get no env var and no side file, so
    they stay byte-for-byte untouched.
    """
    inner_cmd: list[str] = ["lake", *list(args)]
    # Reserved (not created) before the bwrap wrap so the same path can ride
    # both the `--setenv` and the env-dict branch below.
    hb_out_path = _new_heartbeat_side_file(repo) if metrics is not None else None
    # Threat-model mitigation 1: when invoked from the supervisor-side
    # checker server, route lake through wrap_command(role="lake_compiler")
    # so an elaborator RCE is bwrap-confined to the supervisor workspace
    # build outputs + Tablet/. The current direct-call path (worker burst's
    # outer bwrap'd lake; tests; supervisor pre-warm) is unchanged with
    # bwrap_role=None.
    if bwrap_role is not None:
        from trellis.config import SandboxConfig
        from trellis.sandbox import bwrap_available, wrap_command
        from trellis.project_paths import supervisor_workspace_home_path

        if not bwrap_available():
            return _spawn_error_payload(
                RuntimeError(
                    "bwrap is required for lake_compiler role but is not installed"
                ),
                command=" ".join(inner_cmd),
            )
        # supervisor_workspace.py lays out the workspace as
        #   <worker_repo>/.trellis/supervisor/{repo,home,cache}
        # so when the lake_compiler role is invoked, ``repo`` is conventionally
        # ``<worker>/.trellis/supervisor/repo`` and HOME lives at
        # ``<worker>/.trellis/supervisor/home``. Be tolerant: if the marker
        # path doesn't yet exist (e.g. tests with a synthetic repo) fall back
        # to the canonical helper which derives HOME from the worker repo.
        supervisor_home: Path
        sibling_home = repo.parent / "home" if repo.name == "repo" else None
        if sibling_home is not None and sibling_home.exists():
            supervisor_home = sibling_home
        else:
            supervisor_home = supervisor_workspace_home_path(repo)
            try:
                supervisor_home.mkdir(parents=True, exist_ok=True)
            except OSError:
                pass
        try:
            inner_cmd = wrap_command(
                inner_cmd,
                sandbox=SandboxConfig(enabled=True, backend="bwrap"),
                work_dir=repo,
                burst_home=supervisor_home,
                role=bwrap_role,
            )
            # `wrap_command` adds `--ro-bind <elan_home> <elan_home>` via
            # host_runtime_readonly_roots, but inside the bwrap HOME is
            # rebound to <supervisor_home> so `elan` can't find ~/.elan
            # by default. Make ELAN_HOME explicit so lake locates the
            # toolchain at the read-only mount instead of trying to
            # download it into <supervisor_home>/.elan.
            #
            # NOTE on multi-toolchain exposure: ELAN_HOME points at the
            # shared elan installation, which contains every toolchain
            # trellis can resolve. lake itself selects the toolchain
            # via the workspace's ``lean-toolchain`` file, so a confined
            # supervisor-side lake is still pinned to the workspace's
            # declared version even though the elan tree as a whole
            # holds others.
            from trellis.host_runtime import worker_elan_home

            elan_home = str(worker_elan_home())
            lean_threads = (
                os.environ.get("TRELLIS_LEAN_PARALLELISM", "6").strip() or "6"
            )
            extra_setenv = [
                "--setenv", "ELAN_HOME", elan_home,
                "--setenv", "LEAN_NUM_THREADS", lean_threads,
            ]
            if hb_out_path is not None:
                # Explicit `--setenv` mirrors the env-dict branch below so
                # the plugin sees the side-file path regardless of how
                # bwrap treats the inherited environment.
                extra_setenv += ["--setenv", _HB_OUT_ENV, str(hb_out_path)]
            inner_cmd = inner_cmd[:1] + extra_setenv + inner_cmd[1:]
        except Exception as exc:
            return _spawn_error_payload(exc, command=" ".join(["lake", *list(args)]))

    command = " ".join(["lake", *list(args)])
    try:
        env = os.environ.copy()
        env.update(authoritative_env_for_repo(repo))
        _inject_git_safe_directories(env, repo)
        # Cap lake's task-pool (and per-process Lean elaborator pool) at the
        # supervisor's configured parallelism. Lake schedules build jobs as
        # Lean tasks; LEAN_NUM_THREADS bounds that scheduler. Default 6.
        env["LEAN_NUM_THREADS"] = (
            os.environ.get("TRELLIS_LEAN_PARALLELISM", "6").strip() or "6"
        )
        if hb_out_path is not None:
            # Direct (non-bwrap) branch of the heartbeat side-channel; the
            # bwrap branch re-asserts the same var via `--setenv` above.
            env[_HB_OUT_ENV] = str(hb_out_path)
        if bwrap_role is not None:
            # `inner_cmd` is now `bwrap … lake …`; bwrap resolves the OUTER `lake`
            # via this env's PATH (wrap_command adds no `--setenv PATH`). The
            # checker server may be launched from a shell without elan on PATH,
            # which surfaces as a cryptic `bwrap: execvp lake: No such file or
            # directory`. Prepend the resolved elan bin (independent of the
            # launching shell's PATH) so lake always resolves — mirroring the
            # worker side, which already runs lake under `worker_path_env`.
            from trellis.host_runtime import worker_elan_home

            elan_bin = str(worker_elan_home() / "bin")
            existing_path = env.get("PATH", "")
            if elan_bin not in existing_path.split(os.pathsep):
                env["PATH"] = (
                    f"{elan_bin}{os.pathsep}{existing_path}" if existing_path else elan_bin
                )
        # Mirrors `subprocess.run(capture_output=True, text=True,
        # timeout=...)` exactly — same pipe setup, same timeout/kill
        # sequence, same `poll()` for the return code — with `Popen`
        # swapped for `_Wait4Popen` (diagnostic rusage) and a host-side
        # `_HostProcPeakSampler` observing the tree from outside. Timeout
        # semantics are unchanged: `communicate` raises `TimeoutExpired`,
        # we kill and reap, and the caller's `except` below returns the
        # same `_timeout_payload` as before.
        started_ns = time.monotonic_ns()
        with _Wait4Popen(
            inner_cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            cwd=str(repo),
            env=env,
        ) as process:
            # No sampler for the metrics-less callers (the read-only
            # olean-consuming ops): nothing would read the result, and the
            # structural exclusion documented at the attribution rule below
            # stays visible as "no thread was ever started".
            sampler = (
                _HostProcPeakSampler(process.pid) if metrics is not None else None
            )
            if sampler is not None:
                sampler.start()
            try:
                try:
                    stdout, stderr = process.communicate(timeout=timeout_secs)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                    raise
                except BaseException:
                    # Including KeyboardInterrupt; `communicate` handled the
                    # streams. `__exit__` does the wait for us.
                    process.kill()
                    raise
                returncode = process.poll()
            finally:
                # Every exit path — completion, timeout, KeyboardInterrupt —
                # comes through here, so the sampler thread never outlives
                # the build. Stopped before leaving the `with` (the child is
                # already reaped by `communicate`/`wait`) to close the pid-
                # reuse window as tightly as possible.
                if sampler is not None:
                    sampled_peak_kib, sampled_peak_name = sampler.stop()
        elapsed_ns = time.monotonic_ns() - started_ns
        if metrics is not None:
            metrics["walltime_ms"] = int(elapsed_ns / 1_000_000)
            if sampled_peak_kib is not None:
                metrics["peak_rss_kib"] = int(sampled_peak_kib)
                metrics["peak_rss_process"] = sampled_peak_name
            # Anti-echo diagnostics, never the measurement (see
            # `_HostProcPeakSampler`): the old rusage reading and the value
            # it used to echo, logged so their divergence stays observable.
            if process.child_rusage is not None:
                metrics["wait4_maxrss_kib"] = int(process.child_rusage.ru_maxrss)
            self_rss_kib = _read_self_vm_rss_kib()
            if self_rss_kib is not None:
                metrics["checker_self_rss_kib"] = self_rss_kib
            heartbeats_by_module = _read_heartbeat_side_file(hb_out_path)
            if heartbeats_by_module:
                metrics["heartbeats_by_module"] = heartbeats_by_module
        return _completed_process_payload(
            subprocess.CompletedProcess(process.args, returncode, stdout, stderr)
        )
    except subprocess.TimeoutExpired:
        return _timeout_payload(timeout_secs, command=command)
    except FileNotFoundError as exc:
        return _spawn_error_payload(exc, command=command)
    finally:
        # The side file is per-invocation by construction; remove it on
        # every exit path (completion, timeout, spawn failure) so nothing
        # accumulates under `.lake/build` and no later build can read it.
        if hb_out_path is not None:
            try:
                hb_out_path.unlink(missing_ok=True)
            except OSError:
                pass


def _shared_build_roots(repo: Path) -> list[Path]:
    roots: list[Path] = []
    main_root = repo / ".lake" / "build"
    if main_root.exists():
        roots.append(main_root)
    packages_root = repo / ".lake" / "packages"
    if packages_root.exists():
        for build_root in packages_root.glob("*/.lake/build"):
            if build_root.exists():
                roots.append(build_root)
    return roots


def _inject_git_safe_directories(env: dict[str, str], repo: Path) -> None:
    safe_dirs: list[str] = [str(repo)]
    packages_root = repo / ".lake" / "packages"
    if packages_root.exists():
        for package_dir in sorted(packages_root.iterdir()):
            if not package_dir.is_dir():
                continue
            if (package_dir / ".git").exists():
                safe_dirs.append(str(package_dir))
    existing = int(env.get("GIT_CONFIG_COUNT", "0") or "0")
    env["GIT_CONFIG_COUNT"] = str(existing + len(safe_dirs))
    for idx, path in enumerate(safe_dirs, start=existing):
        env[f"GIT_CONFIG_KEY_{idx}"] = "safe.directory"
        env[f"GIT_CONFIG_VALUE_{idx}"] = path


def _normalized_mode(path: Path, mode: int) -> int:
    execute_bits = mode & 0o111
    if path.is_dir():
        return 0o2775
    if execute_bits:
        return 0o775
    return 0o664


def _normalize_recent_shared_build_artifacts(repo: Path, *, since_ns: int) -> None:
    for root in _shared_build_roots(repo):
        for current_root, dirnames, filenames in os.walk(root):
            current_path = Path(current_root)
            try:
                current_stat = current_path.stat()
            except OSError:
                continue
            if current_stat.st_mtime_ns >= since_ns:
                try:
                    os.chmod(current_path, _normalized_mode(current_path, current_stat.st_mode & 0o777))
                except OSError:
                    pass
            for name in dirnames:
                path = current_path / name
                try:
                    st = path.stat()
                except OSError:
                    continue
                if st.st_mtime_ns < since_ns:
                    continue
                try:
                    os.chmod(path, _normalized_mode(path, st.st_mode & 0o777))
                except OSError:
                    pass
            for name in filenames:
                path = current_path / name
                try:
                    st = path.stat()
                except OSError:
                    continue
                if st.st_mtime_ns < since_ns:
                    continue
                try:
                    os.chmod(path, _normalized_mode(path, st.st_mode & 0o777))
                except OSError:
                    pass


# ---------------------------------------------------------------------------
# Elaboration-cost measurement (Stage 0). Pure telemetry: the numbers below
# gate nothing, and every consumer must treat their absence as ordinary.
# ---------------------------------------------------------------------------
#
# ATTRIBUTION RULE — record a measurement for node ``N`` if and only if this
# ``lake`` invocation's computed stale set is exactly ``{N}``.
#
# The stale set is the content-freshness gate's own verdict (see
# ``materialize_tablet_oleans``): the nodes whose oleans were purged, or were
# absent, and therefore had to be re-elaborated by the lake process we are
# about to time. When it holds exactly one node, every second of walltime and
# every byte of RSS that process spent elaborating is attributable to that
# node. When it holds zero or many, nothing is recorded — a whole-tablet batch
# build has no honest per-node attribution, and a missing record is an ordinary
# state, not an error.
#
# Two paths are excluded structurally rather than by a flag: the checker's
# cache-hit fast paths (``_maybe_synthesize_compile_hit``,
# ``_try_local_closure_axioms_cache_hit``) never reach this module's lake
# spawn, and the read-only olean-consuming ops (``print_axioms``,
# ``observe_lean_semantic_payloads``, ``local_closure_axioms``) call
# ``_run_lake_command`` without a ``metrics`` dict, so no measurement is taken
# at all. Neither can produce a record. A cache-hit RESPONSE may nonetheless
# carry a cost marked ``replayed: true``: that is the checker server
# re-serving a measurement this module took earlier for byte-identical
# content (``server._memoize_elaboration_cost``), never a new measurement —
# necessary because the process that measures (the worker self-check's miss)
# is not the process whose response reaches the engine (the supervisor's
# later warm-cache check).
#
# RESIDUAL IMPRECISION, stated rather than hidden: even with a stale set of
# ``{N}``, lake's process may rebuild a dependency olean that went missing
# without being judged content-stale. The freshness gate makes this rare, and
# the stale-set condition means those deps were judged content-current, so the
# residual is an over-attribution to ``N`` — never an attribution to a node
# that did not elaborate. The unbounded instance: this gate reasons only about
# ``Tablet/`` and never inspects ``.lake/packages``, so a mathlib rebuild
# triggered inside ``lake build Tablet.N`` is charged in full to ``N``.


# Floor assertion: below this, a "measurement" of a lake run that really
# elaborated a node is a failed sampling rather than a small build, so the
# record is dropped. This is the check that would have caught the `wait4`
# echo bug on its first record — echoed values were ~48 MiB, the checker
# server's own RSS.
#
# Calibration, both sides measured rather than assumed. Upper bound: a `lean`
# on an EMPTY zero-import module peaks at 418,820 KiB (409 MiB) on toolchain
# v4.30.0-rc1 — re-measured 2026-08-27 on the current pin v4.33.0 at 467,340
# KiB (456 MiB), i.e. the floor ROSE, so the margin below only widened —
# and the production tree is a `lake` besides (observed at ~800
# MiB live), so no real elaboration lands under 409 MiB. Lower bound: the
# echo class is bounded by the checker server's RSS at fork — 48 MiB live,
# growing slowly. 256 MiB sits with ~37% margin below the real floor (so a
# toolchain that got leaner would not start dropping VALID records, which
# would invert this check's purpose) and ~5x above the echo class it exists
# to catch.
#
# Diagnostic only: a violation drops the record — absent is the honest answer
# for a failed measurement — and surfaces a ``cost_floor_violation`` marker in
# the payload for the request log. Nothing gates on it and nothing may start
# to; it judges whether the SAMPLER worked, never whether the node is
# acceptable.
_ELABORATION_PEAK_RSS_FLOOR_KIB = 256 * 1024


def _elaboration_cost_payload(
    repo: Path,
    *,
    stale_nodes: Sequence[str],
    materialized_nodes: Sequence[str],
    post_closure_hashes: Dict[str, Optional[str]],
    metrics: Dict[str, Any],
    timed_out: bool,
    spawn_error: str,
) -> Optional[Dict[str, Any]]:
    """Build the per-node cost payload, or ``None`` when unattributable.

    Returns ``None`` — silently, this is the common case — unless all of:
      * the stale set was exactly one node (the attribution rule above);
      * the process ran to completion (no timeout, no spawn error) and the
        host-side sampler observed a peak, so the numbers describe a real
        elaboration;
      * that node ended up with a current olean whose source closure did not
        move during the build, which is what makes ``source_closure_hash``
        the exact key the checker's own build cache would serve.
    """
    if len(stale_nodes) != 1 or timed_out or spawn_error:
        return None
    node_name = stale_nodes[0]
    if node_name not in set(materialized_nodes):
        return None
    closure_hash = post_closure_hashes.get(node_name)
    if not closure_hash:
        return None
    walltime_ms = metrics.get("walltime_ms")
    peak_rss_kib = metrics.get("peak_rss_kib")
    if walltime_ms is None or peak_rss_kib is None:
        return None
    try:
        olean_size_bytes = _tablet_olean_path(repo, node_name).stat().st_size
    except (FileNotFoundError, OSError):
        return None
    cost: Dict[str, Any] = {
        "node": node_name,
        "source_closure_hash": closure_hash,
        "walltime_ms": int(walltime_ms),
        "peak_rss_kib": int(peak_rss_kib),
        # Which process held the maximum — `lean` for any real elaboration.
        # Persisted in the kernel record; a run of records naming anything
        # else is the sampler measuring the wrong thing.
        "peak_rss_process": str(metrics.get("peak_rss_process") or ""),
        "olean_size_bytes": int(olean_size_bytes),
    }
    # Heartbeats, when the plugin side-channel reported a count for the
    # attributed node's module (`Tablet.<NodeName>` on the wire, mapped back
    # to the bare node name here). Strictly optional: the channel only
    # produces data when the build ran with the heartbeat plugin loaded, so
    # an absent count is the ordinary state and never withholds the other
    # three fields. The kernel's `ElaborationCostObservation.heartbeats`
    # deserializes this key (serde default `None` when absent).
    heartbeats_by_module = metrics.get("heartbeats_by_module")
    if isinstance(heartbeats_by_module, dict):
        heartbeats = heartbeats_by_module.get(f"Tablet.{node_name}")
        if isinstance(heartbeats, int) and not isinstance(heartbeats, bool):
            if heartbeats >= 0:
                cost["heartbeats"] = int(heartbeats)
    # Anti-echo diagnostics. These ride the cost dict so the checker request
    # log (`server._cost_log_extra` copies the whole dict) shows, per
    # measurement, the old rusage reading and the server RSS it used to
    # echo; the kernel's serde deserializes into a fixed-field struct and
    # ignores them, so they are logged but never persisted.
    for diagnostic in ("wait4_maxrss_kib", "checker_self_rss_kib"):
        if metrics.get(diagnostic) is not None:
            cost[diagnostic] = int(metrics[diagnostic])
    return cost


def _tablet_lean_path(repo: Path, node_name: str) -> Path:
    return repo / "Tablet" / f"{node_name}.lean"


def _tablet_olean_path(repo: Path, node_name: str) -> Path:
    return repo / ".lake" / "build" / "lib" / "lean" / "Tablet" / f"{node_name}.olean"


# ---------------------------------------------------------------------------
# Source-closure content hashing + olean-provenance sidecars (olean-staleness
# fix). The freshness of a compiled ``.olean`` is decided by a SOURCE-CLOSURE
# CONTENT HASH, never by mtime. lake's mtime-based up-to-date heuristic (and
# the checker's former mtime gates) can be defeated by a ``git reset`` or an
# operator ``touch`` that makes a content-stale olean look "fresh"; a stale
# ``sorry``-bearing olean then gets served to a closure / ``#print axioms``
# probe and a genuine close is rejected. Replacing the decision with a content
# hash makes it tamper-proof against mtime.
#
# ``tablet_source_closure_hash`` is a faithful Python port of the kernel's
# ``cache_key::lean_closure_cache_key`` (``kernel/src/cache_key.rs``): same
# ``cache_v=2`` blob, same input order (lake state files → check.py script →
# Preamble → self → sorted deps), same per-file SHA-256. Equivalence is pinned
# by a Rust unit test (``cache_key.rs::python_equivalence_pin``) and a Python
# unit test against the same fixture. Keeping the two in lock-step means the
# provenance hash this module stores beside each olean is exactly the kernel's
# notion of "the source closure this olean was built from".
# ---------------------------------------------------------------------------

# Bare module name of the shared preamble; excluded from the per-node import
# closure (captured by its own ``preamble.lean=`` line), matching the kernel.
_CLOSURE_PREAMBLE_NAME = "Preamble"
# Lake state files hashed unconditionally (empty hash when absent), in the
# exact order the kernel emits them.
_CLOSURE_LAKE_FILES = (
    "lakefile.lean",
    "lakefile.toml",
    "lake-manifest.json",
    "lean-toolchain",
)
# Suffix of the per-olean provenance sidecar, written next to the olean so it
# shares the olean's lifecycle (purged alongside it by the kernel's
# stale-artifact sweep, which keys on the pre-first-dot stem).
_OLEAN_SRCCLOSURE_SUFFIX = ".srcclosure"
# The source-closure sidecar is also the replay-attestation store.  Keeping
# the two facts in one atomically replaced record is load-bearing: a second
# per-module database would have a different copy/purge lifecycle and could
# drift away from the olean.  Legacy sidecars containing only the bare source
# hash remain readable as source provenance, but deliberately carry no replay
# attestation and therefore trigger one cold replay during migration.
_OLEAN_PROVENANCE_SCHEMA_VERSION = 3
_KERNEL_REPLAY_ATTESTATION_VERSION = 4
_KERNEL_REPLAY_CHECKER = "leanchecker"
_KERNEL_REPLAY_MODE = "ordinary"
_DECLARATION_MANIFEST_VERSION = 2
_TRUSTED_IMPORT_POLICY_VERSION = 1
# Build artifacts deleted to force a lake rebuild when an olean is content-
# stale. Mirrors the kernel's ``purge_stale_tablet_build_artifacts`` set, plus
# our provenance sidecar.
_OLEAN_ARTIFACT_SUFFIXES = (
    ".olean",
    ".olean.server",
    ".olean.private",
    ".olean.server.hash",
    ".olean.private.hash",
    ".olean.hash",
    ".ilean",
    ".ilean.hash",
    ".trace",
    ".c",
    ".c.hash",
    ".ll",
    ".olean" + _OLEAN_SRCCLOSURE_SUFFIX,
)


def _sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _strip_lean_comments(content: str) -> str:
    """Remove Lean line and nested block comments, preserving newlines.

    Import commands are header syntax, but line-by-line prefix matching is
    not a parser: it accepts imports hidden in block comments and misses a
    command after a block comment.  This small lexer implements precisely the
    comment layer needed by the import walker.  Replacing comment bytes with
    spaces keeps command boundaries and line numbers stable.
    """
    out: list[str] = []
    i = 0
    block_depth = 0
    in_line_comment = False
    while i < len(content):
        pair = content[i : i + 2]
        ch = content[i]
        if in_line_comment:
            if ch == "\n":
                in_line_comment = False
                out.append(ch)
            else:
                out.append(" ")
            i += 1
            continue
        if block_depth:
            if pair == "/-":
                block_depth += 1
                out.extend((" ", " "))
                i += 2
            elif pair == "-/":
                block_depth -= 1
                out.extend((" ", " "))
                i += 2
            else:
                out.append("\n" if ch == "\n" else " ")
                i += 1
            continue
        if pair == "--":
            in_line_comment = True
            out.extend((" ", " "))
            i += 2
        elif pair == "/-":
            block_depth = 1
            out.extend((" ", " "))
            i += 2
        else:
            out.append(ch)
            i += 1
    return "".join(out)


def _lean_import_modules(content: str) -> list[str]:
    """Return module identifiers from legal one-line Lean import commands.

    Lean permits one import command to name several modules (``import A B``).
    Module identifiers are whitespace-delimited and may contain dotted or
    Unicode identifier components.  Comments are removed first, including
    nested block comments; blank lines and comment-only lines are immaterial.
    The generated Tablet surface uses one-line import commands (as Lean's
    formatter emits), so no source-line regex is used for module contents.
    This is cache/materialization bookkeeping, never certificate authority;
    certificate roots use replay-attested ``ModuleData.imports``.
    """
    modules: list[str] = []
    allowed_prefixes = {"public", "private", "meta"}
    for line in _strip_lean_comments(content).splitlines():
        tokens = line.split()
        try:
            import_index = tokens.index("import")
        except ValueError:
            continue
        if any(token not in allowed_prefixes for token in tokens[:import_index]):
            continue
        modules.extend(token for token in tokens[import_index + 1 :] if token != "all")
    return modules


def _repo_check_script_path(repo: Path) -> Path:
    """``.trellis/scripts/check.py`` — matches kernel ``repo_check_script_path``."""
    return repo / ".trellis" / "scripts" / "check.py"


def _kernel_extract_tablet_imports(content: str) -> set[str]:
    """Port of kernel ``cache_key::extract_tablet_imports``.

    Collects the suffix from every ``Tablet.<suffix>`` module in every Lean
    import command.  The generic import lexer covers multiple modules in one
    command, comments/blank lines, dotted identifiers, and Unicode names.
    The Rust port in ``kernel/src/cache_key.rs`` is pinned by equivalence
    tests so the import closure matches byte-for-byte.
    """
    out: set[str] = set()
    prefix = "Tablet."
    for module in _lean_import_modules(content):
        if module.startswith(prefix):
            suffix = module[len(prefix) :]
            if suffix:
                out.add(suffix)
    return out


def _kernel_direct_imports(repo: Path, node_name: str) -> set[str]:
    lean_path = repo / "Tablet" / f"{node_name}.lean"
    try:
        content = lean_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        content = ""
    return _kernel_extract_tablet_imports(content)


def _kernel_recursive_imports(
    repo: Path, node_name: str, visited: set[str]
) -> None:
    if (
        not node_name
        or node_name == _CLOSURE_PREAMBLE_NAME
        or node_name in visited
    ):
        return
    visited.add(node_name)
    for dep in _kernel_direct_imports(repo, node_name):
        if dep != _CLOSURE_PREAMBLE_NAME:
            _kernel_recursive_imports(repo, dep, visited)


def tablet_source_closure_hash(repo: Path, node_name: str) -> Optional[str]:
    """Python replica of kernel ``cache_key::lean_closure_cache_key``.

    Returns the SHA-256 of the ``cache_v=2`` blob over the node's source
    closure (lake state + check.py + Preamble + self + transitive deps), or
    ``None`` (fail-closed) when a required source — the check script, the
    preamble, the node, or any transitive dep — is unreadable. ``None`` means
    "cannot establish provenance", which every caller treats as "not current"
    (rebuild). Mtimes are never consulted.
    """
    closure: set[str] = set()
    _kernel_recursive_imports(repo, node_name, closure)
    closure.discard(node_name)
    closure.discard(_CLOSURE_PREAMBLE_NAME)

    tablet_dir = repo / "Tablet"
    parts: List[str] = ["cache_v=2\n"]
    for fname in _CLOSURE_LAKE_FILES:
        try:
            content = (repo / fname).read_bytes()
        except OSError:
            content = b""
        parts.append(f"lake:{fname}={_sha256_bytes(content)}\n")
    try:
        script_bytes = _repo_check_script_path(repo).read_bytes()
    except OSError:
        return None
    parts.append(f"script={_sha256_bytes(script_bytes)}\n")
    try:
        preamble_bytes = (tablet_dir / "Preamble.lean").read_bytes()
    except OSError:
        return None
    parts.append(f"preamble.lean={_sha256_bytes(preamble_bytes)}\n")
    try:
        own_bytes = (tablet_dir / f"{node_name}.lean").read_bytes()
    except OSError:
        return None
    parts.append(f"self={node_name}={_sha256_bytes(own_bytes)}\n")
    for dep in sorted(closure):
        try:
            dep_bytes = (tablet_dir / f"{dep}.lean").read_bytes()
        except OSError:
            return None
        parts.append(f"dep:{dep}={_sha256_bytes(dep_bytes)}\n")
    return _sha256_bytes("".join(parts).encode("utf-8"))


def _olean_srcclosure_sidecar_path(repo: Path, node_name: str) -> Path:
    olean = _tablet_olean_path(repo, node_name)
    return olean.parent / (olean.name + _OLEAN_SRCCLOSURE_SUFFIX)


def _read_olean_provenance(repo: Path, node_name: str) -> Dict[str, Any]:
    """Read the unified source/replay provenance record.

    A pre-attestation sidecar is a single bare source-closure hash.  It stays
    valid for build freshness (avoiding a deployment rebuild storm) but has no
    ``kernel_replay`` field, so it cannot skip replay.
    """
    try:
        text = _olean_srcclosure_sidecar_path(repo, node_name).read_text(
            encoding="utf-8"
        )
    except OSError:
        return {}
    text = text.strip()
    if not text:
        return {}
    try:
        decoded = json.loads(text)
    except (json.JSONDecodeError, TypeError):
        return {"source_closure_sha256": text}
    if not isinstance(decoded, dict):
        return {}
    return dict(decoded)


def read_olean_srcclosure(repo: Path, node_name: str) -> Optional[str]:
    """Return the source-closure hash recorded beside the node's olean."""
    value = _read_olean_provenance(repo, node_name).get("source_closure_sha256")
    if not isinstance(value, str):
        return None
    value = value.strip()
    return value or None


def _write_olean_provenance(
    repo: Path, node_name: str, record: Dict[str, Any]
) -> bool:
    """Atomically replace one unified provenance record, fail closed."""
    path = _olean_srcclosure_sidecar_path(repo, node_name)
    normalized = dict(record)
    normalized["schema_version"] = _OLEAN_PROVENANCE_SCHEMA_VERSION
    encoded = json.dumps(
        normalized, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ) + "\n"
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        fd, tmp_name = tempfile.mkstemp(
            prefix=path.name + ".tmp.", dir=str(path.parent)
        )
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                handle.write(encoded)
            os.replace(tmp_name, path)
            return True
        except Exception:
            try:
                os.unlink(tmp_name)
            except OSError:
                pass
            raise
    except OSError:
        return False


def write_olean_srcclosure(repo: Path, node_name: str, closure_hash: str) -> None:
    """Atomically persist the source-closure hash an olean was built from.

    Best-effort (temp + rename); an I/O failure leaves no sidecar, which the
    freshness check reads as "not current" (the fail-closed direction).
    """
    record = _read_olean_provenance(repo, node_name)
    record["source_closure_sha256"] = closure_hash
    _write_olean_provenance(repo, node_name, record)


def _kernel_replay_toolchain_sha256(repo: Path) -> str:
    """Identity of the toolchain selected for both build and replay.

    ``lean-toolchain`` is Elan's authoritative immutable toolchain pin and is
    already an olean source-closure input.  Hashing its bytes here makes the
    dependency explicit in the replay attestation as well.
    """
    try:
        toolchain = (repo / "lean-toolchain").read_bytes()
    except OSError:
        toolchain = b""
    return _sha256_bytes(toolchain)


def _olean_sha256(repo: Path, node_name: str) -> Optional[str]:
    try:
        with _tablet_olean_path(repo, node_name).open("rb") as handle:
            digest = hashlib.sha256()
            for chunk in iter(lambda: handle.read(1 << 20), b""):
                digest.update(chunk)
            return digest.hexdigest()
    except OSError:
        return None


def _artifact_bundle(repo: Path, node_name: str) -> Optional[list[dict[str, Any]]]:
    """Hash the exact ordered olean-part bundle used by ``leanchecker``.

    LeanChecker loads exported, then an optional server part, then an optional
    private part (the latter is only considered when the server part exists).
    A private-without-server layout is unrecognized and fails closed here.
    Paths are labels only; certificate authority comes from ordered levels,
    lengths, and SHA-256 digests of every byte.
    """
    exported = _tablet_olean_path(repo, node_name)
    server = Path(str(exported) + ".server")
    private = Path(str(exported) + ".private")
    if private.exists() and not server.exists():
        return None
    paths: list[tuple[str, Path]] = [("exported", exported)]
    if server.exists():
        paths.append(("server", server))
        if private.exists():
            paths.append(("private", private))
    result: list[dict[str, Any]] = []
    try:
        for level, path in paths:
            digest = hashlib.sha256()
            size = 0
            with path.open("rb") as handle:
                for chunk in iter(lambda: handle.read(1 << 20), b""):
                    digest.update(chunk)
                    size += len(chunk)
            result.append(
                {
                    "level": level,
                    "sha256": digest.hexdigest(),
                    "size_bytes": size,
                }
            )
    except OSError:
        return None
    return result


def _expected_kernel_replay_attestation(
    repo: Path,
    node_name: str,
    *,
    olean_sha256: Optional[str] = None,
    artifact_bundle: Optional[Sequence[Mapping[str, Any]]] = None,
) -> Optional[Dict[str, Any]]:
    # ``olean_sha256`` is accepted only to avoid breaking diagnostic callers;
    # v2 authority always re-reads and binds the complete ordered bundle.
    del olean_sha256
    bundle = [dict(part) for part in artifact_bundle] if artifact_bundle is not None else _artifact_bundle(repo, node_name)
    if not bundle:
        return None
    return {
        "attestation_version": _KERNEL_REPLAY_ATTESTATION_VERSION,
        "checker": _KERNEL_REPLAY_CHECKER,
        "mode": _KERNEL_REPLAY_MODE,
        "artifact_bundle": bundle,
        "toolchain_sha256": _kernel_replay_toolchain_sha256(repo),
    }


def olean_has_current_kernel_replay(
    repo: Path,
    node_name: str,
    *,
    olean_sha256: Optional[str] = None,
    artifact_bundle: Optional[Sequence[Mapping[str, Any]]] = None,
) -> bool:
    """Whether this exact olean has a current ordinary-replay attestation."""
    expected = _expected_kernel_replay_attestation(
        repo,
        node_name,
        olean_sha256=olean_sha256,
        artifact_bundle=artifact_bundle,
    )
    if expected is None:
        return False
    record = _read_olean_provenance(repo, node_name)
    if record.get("schema_version") != _OLEAN_PROVENANCE_SCHEMA_VERSION:
        return False
    stored = record.get("kernel_replay")
    if not isinstance(stored, dict):
        return False
    for key, value in expected.items():
        if stored.get(key) != value:
            return False
    manifest = stored.get("declaration_manifest")
    visibility = stored.get("visibility_manifests")
    return (
        stored.get("declaration_manifest_version") == _DECLARATION_MANIFEST_VERSION
        and stored.get("trusted_import_policy_version")
        == _TRUSTED_IMPORT_POLICY_VERSION
        and isinstance(stored.get("trusted_direct_imports"), list)
        and isinstance(manifest, list)
        and isinstance(visibility, list)
        and len(visibility) == len(stored.get("artifact_bundle", []))
        and all(
            isinstance(entry, dict)
            and isinstance(entry.get("name"), str)
            and bool(entry.get("name"))
            and isinstance(entry.get("kind"), str)
            and bool(entry.get("kind"))
            for entry in manifest
        )
        and all(
            isinstance(item, dict)
            and item.get("level") in {"exported", "server", "private"}
            and isinstance(item.get("declarations"), list)
            for item in visibility
        )
    )


def _write_kernel_replay_attestation(
    repo: Path,
    node_name: str,
    *,
    olean_sha256: str,
    declaration_manifest: Optional[Sequence[Mapping[str, str]]] = None,
    visibility_manifests: Optional[Sequence[Mapping[str, Any]]] = None,
    artifact_bundle: Optional[Sequence[Mapping[str, Any]]] = None,
    trusted_direct_imports: Optional[Sequence[Mapping[str, str]]] = None,
) -> bool:
    expected = _expected_kernel_replay_attestation(
        repo,
        node_name,
        olean_sha256=olean_sha256,
        artifact_bundle=artifact_bundle,
    )
    if expected is None:
        return False
    record = _read_olean_provenance(repo, node_name)
    expected["declaration_manifest_version"] = _DECLARATION_MANIFEST_VERSION
    expected["declaration_manifest"] = sorted(
        [
            {"name": str(entry.get("name", "")), "kind": str(entry.get("kind", ""))}
            for entry in (declaration_manifest or [])
        ],
        key=lambda entry: (entry["name"], entry["kind"]),
    )
    normalized_visibility = visibility_manifests
    if normalized_visibility is None:
        normalized_visibility = [
            {
                "level": str(part.get("level", "")),
                "declarations": list(declaration_manifest or []),
            }
            for part in expected.get("artifact_bundle", [])
            if isinstance(part, Mapping)
        ]
    expected["visibility_manifests"] = [
        {
            "level": str(item.get("level", "")),
            "declarations": sorted(
                [
                    {
                        "name": str(entry.get("name", "")),
                        "kind": str(entry.get("kind", "")),
                    }
                    for entry in item.get("declarations", [])
                    if isinstance(entry, Mapping)
                ],
                key=lambda entry: (entry["name"], entry["kind"]),
            ),
        }
        for item in normalized_visibility
    ]
    expected["trusted_import_policy_version"] = _TRUSTED_IMPORT_POLICY_VERSION
    expected["trusted_direct_imports"] = sorted(
        [
            {
                "module": str(entry.get("module", "")),
                "olean": str(entry.get("olean", "")),
            }
            for entry in (trusted_direct_imports or [])
        ],
        key=lambda entry: (entry["module"], entry["olean"]),
    )
    record["kernel_replay"] = expected
    if not _write_olean_provenance(repo, node_name, record):
        return False
    return olean_has_current_kernel_replay(
        repo, node_name, artifact_bundle=artifact_bundle
    )


def read_kernel_replay_declaration_manifest(
    repo: Path, node_name: str
) -> Optional[list[dict[str, str]]]:
    """Return the manifest bound to the exact current replay-attested olean."""
    if not olean_is_content_current(repo, node_name) or not olean_has_current_kernel_replay(
        repo, node_name
    ):
        return None
    replay = _read_olean_provenance(repo, node_name).get("kernel_replay")
    if not isinstance(replay, dict):
        return None
    manifest = replay.get("declaration_manifest")
    if not isinstance(manifest, list):
        return None
    return [dict(entry) for entry in manifest if isinstance(entry, dict)]


def read_kernel_replay_artifact_evidence(
    repo: Path, node_name: str
) -> Optional[dict[str, Any]]:
    """Return v2 manifest, imports, and bundle commitments for the artifact."""
    if not olean_is_content_current(repo, node_name) or not olean_has_current_kernel_replay(
        repo, node_name
    ):
        return None
    replay = _read_olean_provenance(repo, node_name).get("kernel_replay")
    if not isinstance(replay, dict):
        return None
    manifest = replay.get("declaration_manifest")
    visibility = replay.get("visibility_manifests")
    bundle = replay.get("artifact_bundle")
    trusted_imports = replay.get("trusted_direct_imports")
    if (
        not isinstance(manifest, list)
        or not isinstance(visibility, list)
        or not isinstance(bundle, list)
        or not isinstance(trusted_imports, list)
        or not all(
            isinstance(entry, dict)
            and isinstance(entry.get("module"), str)
            and bool(entry.get("module"))
            and isinstance(entry.get("olean"), str)
            and bool(entry.get("olean"))
            for entry in trusted_imports
        )
    ):
        return None
    return {
        "declaration_manifest": [dict(entry) for entry in manifest if isinstance(entry, dict)],
        "direct_imports": sorted({entry["module"] for entry in trusted_imports}),
        "visibility_manifests": [dict(item) for item in visibility if isinstance(item, dict)],
        "artifact_bundle": [dict(part) for part in bundle if isinstance(part, dict)],
    }


def _capture_replay_declaration_manifests(
    repo: Path,
    node_names: Sequence[str],
    *,
    timeout_secs: float,
    bwrap_role: Optional[str],
) -> tuple[dict[str, list[dict[str, str]]], Dict[str, Any]]:
    if not node_names:
        return {}, {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }
    script = Path(__file__).resolve().parents[2] / "scripts" / "lean_module_manifest.lean"
    payload = _run_lake_command(
        repo,
        ["env", "lean", "--run", str(script), *node_names],
        timeout_secs=timeout_secs,
        bwrap_role=bwrap_role,
    )
    manifests: dict[str, list[dict[str, str]]] = {}
    visibility_manifests: dict[str, list[dict[str, Any]]] = {}
    artifact_part_rows: dict[str, list[dict[str, Any]]] = {}
    import_rows: dict[str, list[dict[str, str]]] = {}
    if (
        payload.get("returncode") == 0
        and not payload.get("timed_out")
        and not payload.get("spawn_error")
    ):
        for line in str(payload.get("stdout", "") or "").splitlines():
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(row, dict):
                continue
            node = row.get("node")
            entries = row.get("declarations")
            ownership_entries = row.get("ownership_manifest")
            artifact_parts = row.get("artifact_parts")
            direct_imports = row.get("direct_imports")
            sysroot = row.get("sysroot")
            if (
                isinstance(node, str)
                and isinstance(entries, list)
                and isinstance(ownership_entries, list)
                and entries == ownership_entries
                and isinstance(artifact_parts, list)
                and bool(artifact_parts)
                and isinstance(direct_imports, list)
                and isinstance(sysroot, str)
                and sysroot
            ):
                manifests[node] = [dict(entry) for entry in entries if isinstance(entry, dict)]
                visibility_manifests[node] = [
                    {
                        "level": str(part.get("level", "")),
                        "declarations": [
                            dict(entry)
                            for entry in part.get("declarations", [])
                            if isinstance(entry, dict)
                        ],
                    }
                    for part in artifact_parts
                    if isinstance(part, dict)
                ]
                artifact_part_rows[node] = [
                    {
                        "level": str(part.get("level", "")),
                        "path": str(part.get("path", "")),
                    }
                    for part in artifact_parts
                    if isinstance(part, dict)
                ]
                import_rows[node] = [
                    {
                        "module": str(entry.get("module", "")),
                        "olean": str(entry.get("olean", "")),
                        "sysroot": sysroot,
                    }
                    for entry in direct_imports
                    if isinstance(entry, dict)
                ]
    missing = sorted(set(node_names) - set(manifests))
    if missing:
        prior = str(payload.get("stderr", "") or "")
        payload["stderr"] = (
            prior + "\nmissing replay declaration manifests for " + repr(missing)
        ).lstrip("\n")
        payload["returncode"] = 1
    if not missing:
        trusted_imports, import_errors = _validate_replay_import_boundaries(
            repo, node_names, import_rows
        )
        payload["trusted_direct_imports"] = trusted_imports
        payload["visibility_manifests"] = visibility_manifests
        payload["artifact_parts"] = artifact_part_rows
        if import_errors:
            prior = str(payload.get("stderr", "") or "")
            payload["stderr"] = (
                prior
                + "\ntrusted non-Tablet import policy violation(s):\n"
                + "\n".join(import_errors)
            ).lstrip("\n")
            payload["returncode"] = 1
    return manifests, payload


def _path_is_within(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
        return True
    except ValueError:
        return False


def _lake_managed_import_roots(repo: Path) -> list[Path]:
    """Cheap policy roots for imports not owned by a Tablet node.

    The project tree contains operator-owned support modules. Lake's pinned
    manifest additionally authorizes its materialized package directory and
    explicit local path dependencies. The pinned Lean executable contributes
    its sysroot separately from the manifest-script observation.
    """
    roots = [repo.resolve()]
    manifest_path = repo / "lake-manifest.json"
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return roots
    if not isinstance(manifest, dict):
        return roots
    packages_dir = manifest.get("packagesDir")
    if isinstance(packages_dir, str) and packages_dir:
        roots.append((repo / packages_dir).resolve())
    packages = manifest.get("packages")
    if isinstance(packages, list):
        for package in packages:
            if (
                isinstance(package, dict)
                and package.get("type") == "path"
                and isinstance(package.get("dir"), str)
                and package["dir"]
            ):
                roots.append((repo / package["dir"]).resolve())
    return list(dict.fromkeys(roots))


def _validate_replay_import_boundaries(
    repo: Path,
    node_names: Sequence[str],
    import_rows: Mapping[str, Sequence[Mapping[str, str]]],
) -> tuple[dict[str, list[dict[str, str]]], list[str]]:
    """Assert the explicit trusted-library boundary without replaying it.

    Non-Tablet imports are trusted by policy, not recursively certified. They
    must resolve inside the pinned Lean sysroot, a Lake-managed dependency,
    or the operator-owned project tree. Tablet imports must resolve to this
    project's own build output so exact owner certificates cannot be shadowed.
    """
    repo_root = repo.resolve()
    project_olean_roots = [
        (repo / ".lake/build/lib/lean").resolve(),
        (repo / ".lake/build/lib").resolve(),
    ]
    lake_roots = _lake_managed_import_roots(repo)
    trusted: dict[str, list[dict[str, str]]] = {}
    errors: list[str] = []
    for node in node_names:
        node_trusted: list[dict[str, str]] = []
        rows = import_rows.get(node)
        if rows is None:
            errors.append(f"Tablet.{node}: manifest capture omitted direct imports")
            continue
        for entry in rows:
            module = str(entry.get("module", "") or "")
            raw_olean = str(entry.get("olean", "") or "")
            raw_sysroot = str(entry.get("sysroot", "") or "")
            if not module or not raw_olean or not raw_sysroot:
                errors.append(f"Tablet.{node}: malformed direct-import resolution {entry!r}")
                continue
            unresolved = Path(raw_olean)
            olean = (
                unresolved if unresolved.is_absolute() else repo_root / unresolved
            ).resolve()
            sysroot = Path(raw_sysroot).resolve()
            if module == "Tablet" or module.startswith("Tablet."):
                allowed = any(_path_is_within(olean, root) for root in project_olean_roots)
                policy = "this project's .lake/build Tablet output"
            else:
                allowed_roots = [sysroot / "lib/lean", *lake_roots]
                allowed = any(_path_is_within(olean, root) for root in allowed_roots)
                policy = "the pinned Lean sysroot, Lake manifest, or project support tree"
            if not allowed:
                errors.append(
                    f"Tablet.{node}: import {module!r} resolved to {str(olean)!r}, "
                    f"outside {policy}; declare it as a pinned Lake dependency or "
                    "move the support module into the project"
                )
                continue
            node_trusted.append({"module": module, "olean": str(olean)})
        trusted[node] = sorted(
            node_trusted, key=lambda entry: (entry["module"], entry["olean"])
        )
    return trusted, errors


def _ensure_provenance_sidecars_exist(
    repo: Path, node_names: Sequence[str]
) -> None:
    """Pre-create provenance paths for lake_compiler's read-only overlays.

    The containing build directory must remain writable, but an elaborator
    must not be able to manufacture the attestation that lets its own output
    skip replay.  ``sandbox.wrap_command`` overlays every existing provenance
    path read-only after binding ``.lake/build`` read-write.  Missing paths
    are therefore created empty before the untrusted build starts; only this
    host process later replaces them.
    """
    for node_name in node_names:
        path = _olean_srcclosure_sidecar_path(repo, node_name)
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            # Do not let an untrusted pre-existing symlink redirect the
            # provenance overlay or the later atomic replacement outside the
            # Tablet build directory.
            if path.is_symlink():
                path.unlink()
            path.touch(exist_ok=True)
        except OSError:
            pass


def olean_is_content_current(repo: Path, node_name: str) -> bool:
    """Return whether the node's olean is current with its source CONTENT.

    True iff the olean exists, is non-empty, has a provenance sidecar, and
    that sidecar equals the current source-closure hash. Mtime is never
    consulted. Any inability to establish provenance (missing sidecar,
    unhashable closure) returns False — fail closed so a possibly-stale olean
    is rebuilt rather than served.
    """
    olean = _tablet_olean_path(repo, node_name)
    try:
        st = olean.stat()
    except (FileNotFoundError, OSError):
        return False
    if st.st_size == 0:
        return False
    stored = read_olean_srcclosure(repo, node_name)
    if not stored:
        return False
    current = tablet_source_closure_hash(repo, node_name)
    if current is None:
        return False
    return stored == current


def _purge_olean_artifacts(repo: Path, node_name: str) -> None:
    """Delete a node's build artifacts so lake is forced to rebuild it.

    Defeats lake's mtime up-to-date heuristic for a content-stale olean
    (e.g. a ``sorry`` olean ``touch``ed mtime-newer than a now-closed
    source): with the ``.olean``/``.trace``/``.hash`` gone, lake has no
    choice but to recompile from current source. Best-effort per file.
    """
    build_dir = _tablet_olean_path(repo, node_name).parent
    for suffix in _OLEAN_ARTIFACT_SUFFIXES:
        path = build_dir / f"{node_name}{suffix}"
        try:
            path.unlink()
        except FileNotFoundError:
            pass
        except OSError:
            pass


def _refresh_olean_artifact_mtimes(repo: Path, node_name: str) -> None:
    """Bump every build artifact for a CONTENT-VERIFIED-current node to now.

    This is the no-rebuild-storm lever: after a ``git reset`` (or operator
    ``touch``) leaves sources mtime-newer than their oleans while the content
    is unchanged, refreshing the oleans newer than the sources stops lake's
    mtime heuristic from triggering a needless rebuild. Safe because the
    caller has already confirmed the olean's content matches the source
    closure, so skipping the rebuild cannot serve stale bytes.
    """
    build_dir = _tablet_olean_path(repo, node_name).parent
    try:
        entries = list(build_dir.glob(f"{node_name}.*"))
    except OSError:
        return
    now = time.time()
    for path in entries:
        try:
            os.utime(path, (now, now))
        except OSError:
            pass


def _direct_tablet_imports(repo: Path, node_name: str) -> list[str]:
    lean_path = _tablet_lean_path(repo, node_name)
    if not lean_path.exists():
        return []
    content = lean_path.read_text(encoding="utf-8", errors="replace")
    return sorted(_kernel_extract_tablet_imports(content))


def materialization_order(
    repo: Path,
    requested_nodes: Sequence[str],
) -> list[str]:
    order: list[str] = []
    visited: set[str] = set()

    def visit(node_name: str) -> None:
        cleaned = str(node_name).strip()
        if not cleaned or cleaned in visited:
            return
        visited.add(cleaned)
        for dep in _direct_tablet_imports(repo, cleaned):
            visit(dep)
        order.append(cleaned)

    for node_name in requested_nodes:
        visit(node_name)
    return order


def _kernel_replay_tablet_modules(
    repo: Path,
    node_names: Sequence[str],
    *,
    timeout_secs: float,
    bwrap_role: Optional[str],
) -> Dict[str, Any]:
    """Replay the named Tablet modules through Lean's real kernel.

    ``lake build`` is not a kernel-validity capability: a source-local
    ``set_option debug.skipKernelTC true`` makes ``Lean.addDecl`` install an
    unchecked declaration and still emit a successful olean. ``leanchecker``
    reconstructs each target module from its imports and re-adds every local
    declaration with kernel checking enabled. The caller supplies the full
    transitive Tablet import closure, so every Tablet declaration trusted by
    the requested nodes is replayed, including declarations synthesized by
    elaborators or deriving handlers.

    This is deliberately ordinary (per-module) replay rather than ``--fresh``:
    P0 establishes that every *Tablet* declaration passed the kernel. Full
    fresh replay of external imports belongs to the recursive certificate
    layer and is far more expensive. The command runs in the same confined
    ``lake_compiler`` role as the build.
    """
    nodes: list[str] = []
    seen: set[str] = set()
    for raw_name in node_names:
        node_name = str(raw_name).strip()
        if not node_name or node_name in seen:
            continue
        seen.add(node_name)
        nodes.append(node_name)
    if not nodes:
        return {
            "checked_nodes": [],
            "reused_nodes": [],
            "attested_nodes": [],
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }
    # Hash before invoking leanchecker.  A matching persisted attestation is
    # enough to settle an unchanged module; a missing/legacy/mismatched record
    # puts only that module on this invocation's replay list.
    pre_bundles: Dict[str, list[dict[str, Any]]] = {}
    replay_nodes: list[str] = []
    reused_nodes: list[str] = []
    for node_name in nodes:
        bundle = _artifact_bundle(repo, node_name)
        if bundle is None:
            replay_nodes.append(node_name)
            continue
        pre_bundles[node_name] = bundle
        if olean_has_current_kernel_replay(
            repo, node_name, artifact_bundle=bundle
        ):
            reused_nodes.append(node_name)
        else:
            replay_nodes.append(node_name)

    if not replay_nodes:
        return {
            "checked_nodes": [],
            "reused_nodes": reused_nodes,
            "attested_nodes": list(nodes),
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    modules = [f"Tablet.{node_name}" for node_name in replay_nodes]
    replay = _run_lake_command(
        repo,
        ["env", "leanchecker", *modules],
        timeout_secs=timeout_secs,
        bwrap_role=bwrap_role,
    )
    replay["checked_nodes"] = replay_nodes
    replay["reused_nodes"] = reused_nodes
    replay["attested_nodes"] = list(reused_nodes)

    replay_ok = (
        replay.get("returncode") == 0
        and not bool(replay.get("timed_out"))
        and not bool(str(replay.get("spawn_error", "") or ""))
    )
    if not replay_ok:
        return replay

    declaration_manifests, manifest_capture = _capture_replay_declaration_manifests(
        repo,
        replay_nodes,
        timeout_secs=timeout_secs,
        bwrap_role=bwrap_role,
    )
    if manifest_capture.get("returncode") != 0:
        replay["returncode"] = manifest_capture.get("returncode", 1)
        replay["stderr"] = (
            str(replay.get("stderr", "") or "")
            + "\n[declaration_manifest]\n"
            + str(manifest_capture.get("stderr", "") or "")
        ).lstrip("\n")
        replay["manifest_capture"] = manifest_capture
        return replay

    # The olean replayed by leanchecker must be the exact bytes we attest.
    # A changed/missing file or an unwritable sidecar turns the whole replay
    # into a failure; callers purge the newly checked artifacts and report no
    # materialized closure.
    attestation_errors: list[str] = []
    newly_attested: list[str] = []
    for node_name in replay_nodes:
        before_bundle = pre_bundles.get(node_name)
        after_bundle = _artifact_bundle(repo, node_name)
        captured_parts = manifest_capture.get("artifact_parts", {}).get(node_name, [])
        captured_levels = [part.get("level") for part in captured_parts]
        after_levels = [part.get("level") for part in (after_bundle or [])]
        if (
            before_bundle is None
            or after_bundle != before_bundle
            or captured_levels != after_levels
        ):
            attestation_errors.append(
                f"Tablet.{node_name}: ordered artifact bundle changed, disappeared, or differed from the ModuleData reader during kernel replay"
            )
            continue
        if not _write_kernel_replay_attestation(
            repo,
            node_name,
            olean_sha256=str(after_bundle[0]["sha256"]),
            artifact_bundle=after_bundle,
            declaration_manifest=declaration_manifests.get(node_name, []),
            visibility_manifests=(
                manifest_capture.get("visibility_manifests", {}).get(node_name, [])
            ),
            trusted_direct_imports=(
                manifest_capture.get("trusted_direct_imports", {}).get(node_name, [])
            ),
        ):
            attestation_errors.append(
                f"Tablet.{node_name}: could not persist kernel replay attestation"
            )
            continue
        newly_attested.append(node_name)

    if attestation_errors:
        prior = str(replay.get("stderr", "") or "")
        detail = "\n".join(attestation_errors)
        replay["stderr"] = f"{prior}\n{detail}".lstrip("\n")
        replay["returncode"] = 1
        replay["attested_nodes"] = list(reused_nodes)
        return replay

    replay["attested_nodes"] = [*reused_nodes, *newly_attested]
    return replay


def _nonempty_olean_nodes(repo: Path, node_names: Sequence[str]) -> list[str]:
    """Return names whose build left a non-empty olean, preserving order."""
    result: list[str] = []
    for node_name in node_names:
        try:
            if _tablet_olean_path(repo, node_name).stat().st_size > 0:
                result.append(node_name)
        except (FileNotFoundError, OSError):
            continue
    return result


def _merge_kernel_replay_failure(
    build: Dict[str, Any], replay: Dict[str, Any]
) -> Dict[str, Any]:
    """Turn a replay failure into the ordinary external-command surface."""
    replay_stdout = str(replay.get("stdout", "") or "")
    replay_stderr = str(replay.get("stderr", "") or "")
    if replay_stdout:
        prior = str(build.get("stdout", "") or "")
        build["stdout"] = f"{prior}\n[kernel_replay]\n{replay_stdout}".lstrip("\n")
    if replay_stderr:
        prior = str(build.get("stderr", "") or "")
        build["stderr"] = f"{prior}\n[kernel_replay]\n{replay_stderr}".lstrip("\n")
    replay_rc = replay.get("returncode")
    build["returncode"] = replay_rc if replay_rc not in (0, None) else 1
    build["timed_out"] = bool(build.get("timed_out")) or bool(
        replay.get("timed_out")
    )
    replay_spawn_error = str(replay.get("spawn_error", "") or "")
    if replay_spawn_error:
        prior_spawn_error = str(build.get("spawn_error", "") or "")
        build["spawn_error"] = (
            f"{prior_spawn_error}; kernel replay: {replay_spawn_error}"
            if prior_spawn_error
            else f"kernel replay: {replay_spawn_error}"
        )
    return build


def _find_bare_mathlib_imports(
    repo: Path,
    node_names: Sequence[str],
) -> list[tuple[str, int]]:
    """Walk the transitive Tablet import closure of ``node_names`` and return
    every (node_name, line_number) where a bare ``import Mathlib`` appears.
    A bare Mathlib import pulls the entire library into the import graph and
    makes every dependent compile pay ~80–150s in olean loading."""
    closure = materialization_order(repo, node_names)
    findings: list[tuple[str, int]] = []
    for node_name in closure:
        lean_path = _tablet_lean_path(repo, node_name)
        if not lean_path.exists():
            continue
        try:
            text = lean_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            if _BARE_MATHLIB_IMPORT_RE.match(line):
                findings.append((node_name, lineno))
    return findings


def _bare_mathlib_error_message(findings: list[tuple[str, int]]) -> str:
    locs = ", ".join(f"Tablet/{name}.lean:{lineno}" for name, lineno in findings)
    return (
        f"FAIL: bare `import Mathlib` is not allowed. Replace it with the specific "
        f"`Mathlib.*` modules you actually use (e.g. `import Mathlib.Data.Real.Basic`, "
        f"`import Mathlib.Topology.MetricSpace.Pseudo.Lemmas`). Bare Mathlib pulls in "
        f"the entire library and inflates every dependent compile to ~80–150s. "
        f"Found in: {locs}"
    )


def materialize_tablet_oleans(
    repo: Path,
    node_names: Sequence[str],
    *,
    timeout_secs: float = LEAN_SUPPORT_TIMEOUT_SECS,
    bwrap_role: Optional[str] = None,
) -> Dict[str, Any]:
    # Acceptance routing: when called with ``bwrap_role is None`` (the
    # kernel-invoked acceptance path), the checker socket is mandatory —
    # route through the supervisor-side checker server. See ``compile_node``
    # for the design rationale (recursion guard via ``bwrap_role``,
    # no host-lake fallback). The direct ``_run_lake_command`` path below is
    # reachable only for explicit ``bwrap_role`` values (e.g. the server's
    # own ``"lake_compiler"`` endpoint).
    if bwrap_role is None:
        socket_path = _require_socket_path()
        chunk_payloads: list[Dict[str, Any]] = []
        for chunk in _chunk_node_names(list(node_names)):
            response = client_materialize_tablet_oleans(
                socket_path,
                repo,
                chunk,
                timeout_secs=timeout_secs,
            )
            payload = dict(response)
            payload.pop("request_id", None)
            chunk_payloads.append(payload)
        return _merge_materialize_chunks(chunk_payloads)

    requested = [str(name).strip() for name in node_names if str(name).strip()]
    if not requested:
        requested = sorted(
            path.stem
            for path in (repo / "Tablet").glob("*.lean")
            if path.stem != "Axioms"
        )
    bare_mathlib = _find_bare_mathlib_imports(repo, requested)
    if bare_mathlib:
        return {
            "requested_nodes": requested,
            "materialized_nodes": [],
            "returncode": 2,
            "stdout": "",
            "stderr": _bare_mathlib_error_message(bare_mathlib),
            "timed_out": False,
            "spawn_error": "",
        }
    order = materialization_order(repo, requested)
    output_dir = repo / ".lake" / "build" / "lib" / "lean" / "Tablet"
    output_dir.mkdir(parents=True, exist_ok=True)

    total = len(order)
    materialize_started = time.time()
    _progress_emit(
        f"[acceptance]   materialize-tablet-oleans: starting batched build on {total} node(s)"
    )

    if not order:
        # Degenerate case: nothing to build. Surface a successful no-op so
        # the caller's contract (returncode==0 means "everything you asked
        # for is current") still holds.
        _progress_emit(
            "[acceptance]   materialize-tablet-oleans: no nodes to build"
        )
        return {
            "requested_nodes": requested,
            "materialized_nodes": [],
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    # Content-freshness gate (olean-staleness fix). Decide each node's
    # currency by SOURCE-CLOSURE CONTENT HASH, not mtime, BEFORE handing the
    # tree to lake:
    #   * content-current (sidecar == current source-closure hash): refresh
    #     the olean's mtime so lake's mtime heuristic skips a needless
    #     rebuild — this is what removes the post-`git reset` rebuild storm
    #     that drove operators to ``touch`` oleans in the first place.
    #   * otherwise (stale, missing sidecar, or unhashable closure): fail
    #     closed — delete the stale ``.olean``/``.ilean``/``.trace``/``.hash``
    #     artifacts so lake MUST rebuild from current source. This is what
    #     stops a mtime-frozen-newer ``sorry`` olean from surviving into a
    #     downstream closure / ``#print axioms`` probe.
    # The pre-build hash is snapshotted per node so the post-build sidecar
    # write can detect a source that moved during the build (parallel sync)
    # and decline to vouch for the resulting olean.
    # ``stale_nodes`` is this gate's verdict, reused verbatim as the
    # elaboration-cost attribution set (see `_elaboration_cost_payload`): the
    # nodes lake will actually have to re-elaborate. It is derived, never
    # decided — the branch structure below is unchanged.
    pre_closure_hashes: Dict[str, Optional[str]] = {}
    stale_nodes: list[str] = []
    for node_name in order:
        node_hash = tablet_source_closure_hash(repo, node_name)
        pre_closure_hashes[node_name] = node_hash
        olean = _tablet_olean_path(repo, node_name)
        try:
            olean_size = olean.stat().st_size
        except (FileNotFoundError, OSError):
            # No olean on disk: the node must be built, so it is stale.
            # There is nothing to purge or refresh — keep the existing skip.
            stale_nodes.append(node_name)
            continue
        stored = read_olean_srcclosure(repo, node_name)
        if node_hash is not None and stored == node_hash and olean_size > 0:
            _refresh_olean_artifact_mtimes(repo, node_name)
        else:
            stale_nodes.append(node_name)
            _purge_olean_artifacts(repo, node_name)

    # The build directory is writable to Lean, but provenance is authority.
    # Pre-create every path so the lake_compiler sandbox can overlay these
    # files read-only for the duration of the untrusted build.
    _ensure_provenance_sidecars_exist(repo, order)

    # Single batched lake build: lake itself parallelises across targets
    # using its own job graph, so a single invocation is strictly faster
    # than the per-node loop (paid one lake startup, not N). The per-node
    # `[Foo]\n...` stdout/stderr tagging is dropped since it's no longer
    # meaningful for a batched build; nothing parses it (kernel reads
    # the structured `materialized_nodes` field, not stdout text).
    targets = [f"Tablet.{name}" for name in order]
    build_started_ns = time.time_ns()
    cost_metrics: Dict[str, Any] = {}
    result = _run_lake_command(
        repo,
        ["build", *targets],
        timeout_secs=timeout_secs,
        bwrap_role=bwrap_role,
        metrics=cost_metrics,
    )

    last_returncode = result.get("returncode")
    timed_out = bool(result.get("timed_out"))
    spawn_error = str(result.get("spawn_error") or "")
    stdout = str(result.get("stdout", "") or "")
    stderr = str(result.get("stderr", "") or "")

    if not timed_out and not spawn_error:
        _normalize_recent_shared_build_artifacts(repo, since_ns=build_started_ns)

    # P0 kernel-validity capability. A successful elaboration is insufficient:
    # `debug.skipKernelTC` can make it emit an olean containing declarations
    # the kernel never saw. Replay every module whose olean exists, including
    # partial-build successes, before writing any provenance sidecar or
    # reporting any module as materialized.
    replay_nodes = _nonempty_olean_nodes(repo, order)
    kernel_replay = _kernel_replay_tablet_modules(
        repo,
        replay_nodes,
        timeout_secs=timeout_secs,
        bwrap_role=bwrap_role,
    )
    replay_failed = (
        kernel_replay.get("returncode") != 0
        or bool(kernel_replay.get("timed_out"))
        or bool(str(kernel_replay.get("spawn_error", "") or ""))
    )
    if replay_failed:
        # Preserve settled modules: only artifacts placed on this invocation's
        # replay list lack a current attestation.  The reused set remains
        # independently vouched for and need not become cold because a newly
        # changed neighbor failed replay.
        for node_name in kernel_replay.get("checked_nodes", []) or []:
            _purge_olean_artifacts(repo, node_name)
        failed_payload = _merge_kernel_replay_failure(
            {
                "requested_nodes": requested,
                "materialized_nodes": [],
                "returncode": last_returncode,
                "stdout": stdout,
                "stderr": stderr,
                "timed_out": timed_out,
                "spawn_error": spawn_error,
                "stale_node_count": len(stale_nodes),
            },
            kernel_replay,
        )
        failed_payload["kernel_replay"] = kernel_replay
        _progress_emit(
            f"[acceptance]   materialize-tablet-oleans: kernel replay rejected "
            f"{len(replay_nodes)} module(s)"
        )
        return failed_payload

    # Content-walk to determine which oleans are current and record each
    # one's provenance. Even on a nonzero returncode or timeout some nodes
    # may have built before the failure point; the structured set must
    # reflect on-disk truth. Walk the full closure (``order``) rather than
    # just the originally requested set: lake builds the entire dependency
    # graph, and callers (e.g. the kernel's
    # ``ensure_*_tablet_support_available``) rely on ``materialized_nodes``
    # listing every node that ended up with a current olean — not just the
    # ones the caller named.
    #
    # Currency is decided by SOURCE-CLOSURE CONTENT HASH, not mtime. For each
    # node whose olean now exists non-empty AND whose source closure did not
    # move during the build (pre-build hash == post-build hash), we persist
    # that hash beside the olean and report it materialized. The pre/post
    # comparison closes the parallel-sync race: if another thread rewrote a
    # source mid-build, the olean lake produced is for the pre-sync source,
    # so we decline to vouch for it (no sidecar written) and it is rebuilt
    # on the next call. A node we purged above whose build failed has no
    # olean and is correctly omitted.
    materialized_nodes: list[str] = []
    post_closure_hashes: Dict[str, Optional[str]] = {}
    for node_name in order:
        olean = _tablet_olean_path(repo, node_name)
        try:
            ost = olean.stat()
        except (FileNotFoundError, OSError):
            continue
        if ost.st_size == 0:
            continue
        post_hash = tablet_source_closure_hash(repo, node_name)
        post_closure_hashes[node_name] = post_hash
        pre_hash = pre_closure_hashes.get(node_name)
        if post_hash is None or pre_hash is None or pre_hash != post_hash:
            # Unhashable closure, or source moved during the build — fail
            # closed: do not record provenance, so this olean is treated as
            # not-current (rebuilt) on the next materialize.
            continue
        write_olean_srcclosure(repo, node_name, post_hash)
        materialized_nodes.append(node_name)

    _progress_emit(
        f"[acceptance]   materialize-tablet-oleans: done "
        f"{len(materialized_nodes)}/{total} in {time.time() - materialize_started:.1f}s "
        f"(returncode={last_returncode}, timed_out={timed_out})"
    )
    payload: Dict[str, Any] = {
        "requested_nodes": requested,
        "materialized_nodes": materialized_nodes,
        "returncode": last_returncode,
        "stdout": stdout,
        "stderr": stderr,
        "timed_out": timed_out,
        "spawn_error": spawn_error,
        "kernel_replay": kernel_replay,
        # How many nodes this lake invocation actually had to elaborate. Not
        # consumed by anything — it exists so the attribution rule is
        # falsifiable from the checker request log alone. Cost is emitted iff
        # this is 1, so a logged line pairing a cost with a count above 1 is a
        # broken gate, and a count of 1 with no cost is a measurement that
        # dropped (nothing sampled, a floor violation, or the node did not
        # end up materialized). Without
        # it an operator cannot tell a legitimate single-stale materialize cost
        # from a fabricated one.
        "stale_node_count": len(stale_nodes),
    }
    cost = _elaboration_cost_payload(
        repo,
        stale_nodes=stale_nodes,
        materialized_nodes=materialized_nodes,
        post_closure_hashes=post_closure_hashes,
        metrics=cost_metrics,
        timed_out=timed_out,
        spawn_error=spawn_error,
    )
    if cost is not None and int(cost["peak_rss_kib"]) < _ELABORATION_PEAK_RSS_FLOOR_KIB:
        # Floor assertion (see `_ELABORATION_PEAK_RSS_FLOOR_KIB`): a real
        # elaboration cannot peak below the zero-import `lean` floor, so
        # this measurement is wrong and recording it would repeat the bug
        # this sampler replaced. Drop it, and leave a marker the request
        # log surfaces so the drop is observable rather than silent.
        payload["cost_floor_violation"] = {
            "node": cost["node"],
            "peak_rss_kib": cost["peak_rss_kib"],
            "peak_rss_process": cost.get("peak_rss_process", ""),
            "floor_kib": _ELABORATION_PEAK_RSS_FLOOR_KIB,
        }
        _progress_emit(
            f"[acceptance]   elaboration-cost floor violation: node="
            f"{cost['node']} peak_rss_kib={cost['peak_rss_kib']} "
            f"(< {_ELABORATION_PEAK_RSS_FLOOR_KIB}); measurement dropped"
        )
        cost = None
    if cost is not None:
        payload["cost"] = cost
    return payload


def _lean_semantic_fingerprint_script_path() -> Path:
    return Path(__file__).resolve().parents[2] / "scripts" / "lean_semantic_fingerprint.lean"


def compile_node(
    repo: Path,
    node_name: str,
    *,
    timeout_secs: float = LEAN_SUPPORT_TIMEOUT_SECS,
    bwrap_role: Optional[str] = None,
) -> Dict[str, Any]:
    # Acceptance routing: when called with ``bwrap_role is None`` (the
    # kernel-invoked acceptance path), the checker socket is MANDATORY.
    # Route this observation through the supervisor-side checker server via
    # the RPC client. The server runs lake under bwrap_role="lake_compiler"
    # on the supervisor's authoritative repo and returns a payload identical
    # in shape to the direct-lake path. The unified-checker UNIX-socket
    # server is the only supported way to run acceptance lake checks; the
    # legacy direct-host-lake fallback has been removed.
    #
    # Recursion guard: the supervisor's checker server calls this
    # function with ``bwrap_role="lake_compiler"`` to invoke confined
    # lake on its own side. That call must NOT route through RPC again
    # — it's already on the authoritative side. The worker burst's
    # acceptance callers in cli.py / check.py omit the kwarg, so a missing
    # bwrap_role means "route through the socket" and the resolved-but-None
    # socket case is an operator misconfiguration that raises loudly (no
    # silent host-lake fallback that would bypass the server's authority
    # and confinement).
    if bwrap_role is None:
        socket_path = _require_socket_path()
        response = client_compile_node(
            socket_path,
            repo,
            node_name,
            timeout_secs=timeout_secs,
        )
        # The server includes ``request_id`` in its response
        # envelope; the direct-lake path does not. Strip it so the
        # dict shape matches exactly what callers see today.
        result = dict(response)
        result.pop("request_id", None)
        return result

    # Direct-lake path: reachable only for explicit ``bwrap_role`` values
    # (the server's own ``"lake_compiler"`` endpoint). Compile through the same
    # import-closure materialization path used by explicit supervisor
    # support hydration so newly introduced Tablet imports produce
    # reusable `.olean` artifacts before later policy checks inspect them.
    payload = materialize_tablet_oleans(
        repo,
        [node_name],
        timeout_secs=timeout_secs,
        bwrap_role=bwrap_role,
    )
    payload["node"] = node_name
    return payload


def build_tablet(
    repo: Path,
    *,
    timeout_secs: float = LEAN_SUPPORT_TIMEOUT_SECS,
    bwrap_role: Optional[str] = None,
) -> Dict[str, Any]:
    # Acceptance routing: socket-mandatory when ``bwrap_role is None``.
    # Route through the supervisor-side checker server. See ``compile_node``
    # for the design rationale (no host-lake fallback). The direct-lake path
    # below is reachable only for explicit ``bwrap_role`` values.
    if bwrap_role is None:
        socket_path = _require_socket_path()
        response = client_build_tablet(
            socket_path,
            repo,
            timeout_secs=timeout_secs,
        )
        result = dict(response)
        result.pop("request_id", None)
        return result

    started = time.time()
    _progress_emit("[acceptance]   build-tablet: starting")
    source_nodes = sorted(
        path.stem
        for path in (repo / "Tablet").glob("*.lean")
        if path.stem != "Axioms"
    )
    pre_closure_hashes = {
        node_name: tablet_source_closure_hash(repo, node_name)
        for node_name in source_nodes
    }
    # Full-Tablet builds need the same content gate as targeted
    # materialization.  Without it, lake's mtime heuristic could retain an
    # olean after its source closure changed; its byte-matching replay
    # attestation would then look reusable even though the olean was stale.
    # Purging the olean and its unified provenance record couples source
    # invalidation and replay invalidation on this call site too.  Legacy
    # source-current sidecars take the refresh branch, preserving migration
    # without a rebuild storm while remaining replay-cold.
    for node_name in source_nodes:
        olean = _tablet_olean_path(repo, node_name)
        try:
            olean_size = olean.stat().st_size
        except (FileNotFoundError, OSError):
            olean_size = 0
        stored = read_olean_srcclosure(repo, node_name)
        current = pre_closure_hashes.get(node_name)
        if current is not None and stored == current and olean_size > 0:
            _refresh_olean_artifact_mtimes(repo, node_name)
        else:
            _purge_olean_artifacts(repo, node_name)
    _ensure_provenance_sidecars_exist(repo, source_nodes)
    started_ns = time.time_ns()
    payload = _run_lake_command(
        repo,
        ["build", "Tablet"],
        timeout_secs=timeout_secs,
        bwrap_role=bwrap_role,
    )
    if not payload.get("timed_out") and not payload.get("spawn_error"):
        _normalize_recent_shared_build_artifacts(repo, since_ns=started_ns)
    replay_nodes = _nonempty_olean_nodes(repo, source_nodes)
    kernel_replay = _kernel_replay_tablet_modules(
        repo,
        replay_nodes,
        timeout_secs=timeout_secs,
        bwrap_role=bwrap_role,
    )
    if (
        kernel_replay.get("returncode") != 0
        or bool(kernel_replay.get("timed_out"))
        or bool(str(kernel_replay.get("spawn_error", "") or ""))
    ):
        for node_name in kernel_replay.get("checked_nodes", []) or []:
            _purge_olean_artifacts(repo, node_name)
        payload = _merge_kernel_replay_failure(payload, kernel_replay)
    else:
        # Full-Tablet builds share the same provenance lifecycle as targeted
        # materialization.  Record source provenance only when the closure was
        # stable across the build; replay provenance was already written by
        # `_kernel_replay_tablet_modules` for the exact olean bytes.
        for node_name in kernel_replay.get("attested_nodes", []) or []:
            post_hash = tablet_source_closure_hash(repo, node_name)
            if post_hash and pre_closure_hashes.get(node_name) == post_hash:
                write_olean_srcclosure(repo, node_name, post_hash)
    payload["kernel_replay"] = kernel_replay
    _progress_emit(
        f"[acceptance]   build-tablet: done returncode={payload.get('returncode')} in {time.time() - started:.1f}s"
    )
    return payload


def prepare_compiled_support(
    repo: Path,
    *,
    timeout_secs: float = LEAN_SUPPORT_TIMEOUT_SECS,
    bwrap_role: Optional[str] = None,
) -> Dict[str, Any]:
    # Acceptance routing: socket-mandatory when ``bwrap_role is None``.
    # Route through the supervisor-side checker server. See ``compile_node``
    # for the design rationale (no host-lake fallback). The direct-lake path
    # below is reachable only for explicit ``bwrap_role`` values.
    if bwrap_role is None:
        socket_path = _require_socket_path()
        response = client_prepare_compiled_support(
            socket_path,
            repo,
            timeout_secs=timeout_secs,
        )
        result = dict(response)
        result.pop("request_id", None)
        return result

    stdout_parts: list[str] = []
    stderr_parts: list[str] = []
    steps_completed: list[str] = []
    last_returncode: Optional[int] = 0
    timed_out = False
    spawn_error = ""

    for step_name, args in (("cache_get", ["exe", "cache", "get"]),):
        started_ns = time.time_ns()
        result = _run_lake_command(
            repo,
            args,
            timeout_secs=timeout_secs,
            bwrap_role=bwrap_role,
        )
        if result.get("stdout"):
            stdout_parts.append(f"[{step_name}]\n{result['stdout']}")
        if result.get("stderr"):
            stderr_parts.append(f"[{step_name}]\n{result['stderr']}")
        last_returncode = result.get("returncode")
        if result.get("timed_out"):
            timed_out = True
            break
        if result.get("spawn_error"):
            spawn_error = str(result["spawn_error"])
            break
        _normalize_recent_shared_build_artifacts(repo, since_ns=started_ns)
        if result.get("returncode") not in (0, None):
            break
        steps_completed.append(step_name)

    return {
        "steps_completed": steps_completed,
        "returncode": last_returncode,
        "stdout": "\n".join(part for part in stdout_parts if part),
        "stderr": "\n".join(part for part in stderr_parts if part),
        "timed_out": timed_out,
        "spawn_error": spawn_error,
    }


def _axiom_probe_temp_dir(repo: Path) -> Path:
    for temp_dir in (
        repo_tmp_subdir(repo, "check"),
        repo / ".trellis" / "staging",
    ):
        try:
            temp_dir.mkdir(parents=True, exist_ok=True)
        except OSError:
            continue
        try:
            os.chmod(temp_dir, 0o2775)
        except OSError:
            pass
        if temp_dir.is_dir() and os.access(temp_dir, os.W_OK | os.X_OK):
            return temp_dir
    raise OSError(f"could not create repo-local axiom audit scratch dir under {repo}")


# Note on ``local_closure_axioms`` (LOCAL_CLOSURE_IMPL_PLAN.md Patch A):
# Unlike every other observation in this module, ``local_closure_axioms``
# is **server-only** (plan §5.7) and has **no host-lake fallback**. There
# is intentionally no Python wrapper for it here — Patch A's Python
# integration ends at the dispatch layer in
# :mod:`trellis.atomic_actions.cli` (subcommand
# ``local-closure-axioms``), which routes via
# :func:`trellis.atomic_actions.checker_client.client_local_closure_axioms`
# when ``TRELLIS_CHECKER_SOCKET`` is set and errors loudly otherwise.
# The Rust kernel (Patch B / Patch C) calls the subcommand directly via
# ``run_repo_command_json``; it does not import this module's Python
# surface for this op. Trust model rationale: per plan §2.3, the server
# derives ``repo_path`` from the socket's runtime root and rejects any
# worker-supplied ``repo_path`` (protocol.py:212-217), so a host-lake
# fallback would silently bypass that authority. Surface the missing
# socket as an error instead, matching the ``compile_node`` precedent
# above of refusing to mask operator misconfiguration with graceful
# fallback.


def print_axioms(
    repo: Path,
    node_name: str,
    *,
    timeout_secs: float = LEAN_SUPPORT_TIMEOUT_SECS,
    bwrap_role: Optional[str] = None,
) -> Dict[str, Any]:
    # Acceptance routing: socket-mandatory when ``bwrap_role is None``.
    # Route through the supervisor-side checker server. See ``compile_node``
    # for the design rationale (no host-lake fallback). The direct-lake path
    # below is reachable only for explicit ``bwrap_role`` values.
    if bwrap_role is None:
        socket_path = _require_socket_path()
        response = client_print_axioms(
            socket_path,
            repo,
            node_name,
            timeout_secs=timeout_secs,
        )
        result = dict(response)
        result.pop("request_id", None)
        return result

    temp_path: Optional[Path] = None
    try:
        temp_dir = _axiom_probe_temp_dir(repo)
        with tempfile.NamedTemporaryFile(
            mode="w",
            suffix=".lean",
            dir=str(temp_dir),
            prefix=f"axioms_{node_name}_",
            delete=False,
            encoding="utf-8",
        ) as handle:
            # FILESPEC ordinary nodes declare their principal constant at the
            # root under exactly the node/file-stem name.  Use Lean's absolute
            # root qualifier so namespace opens and exported short names (for
            # example core's ``Singleton.singleton`` versus a tablet theorem
            # named ``singleton``) cannot make the command elaborate more than
            # one declaration.  A non-conforming node has no such constant and
            # the Lean command fails closed instead of auditing a namesake.
            handle.write(
                f"import Tablet.{node_name}\n"
                f"#print axioms _root_.{node_name}\n"
            )
            temp_path = Path(handle.name)
        try:
            os.chmod(temp_path, 0o664)
        except OSError:
            pass
        lean_arg = (
            str(temp_path.relative_to(repo))
            if temp_path.is_relative_to(repo)
            else str(temp_path)
        )
        payload = _run_lake_command(
            repo,
            ["env", "lean", lean_arg],
            timeout_secs=timeout_secs,
            bwrap_role=bwrap_role,
        )
        payload["node"] = node_name
        return payload
    finally:
        if temp_path is not None:
            try:
                temp_path.unlink()
            except OSError:
                pass


def observe_lean_semantic_payloads(
    repo: Path,
    node_names: Sequence[str],
    *,
    timeout_secs: float = LEAN_SUPPORT_TIMEOUT_SECS,
    bwrap_role: Optional[str] = None,
    script_path: Optional[Path] = None,
    principal_names: Optional[Mapping[str, str]] = None,
) -> Dict[str, Dict[str, Any]]:
    """Observe Lean semantic payloads for a set of Tablet nodes.

    ``script_path`` lets the caller pin which copy of
    ``lean_semantic_fingerprint.lean`` lake invokes. When ``None`` (default,
    direct-call path), the source-root copy is used. The supervisor-side
    checker server passes an explicit path so the script resolves inside
    the ``lake_compiler`` bwrap (sandbox.py mounts the trellis source
    ``scripts/`` directory read-only for that role).
    """
    # Acceptance routing: socket-mandatory when ``bwrap_role is None``.
    # Route through the supervisor-side checker server (no host-lake
    # fallback). The wire response is ``{"request_id", "nodes": {...}}``;
    # the client already unwraps the ``nodes`` envelope so it returns the
    # per-node mapping the direct-lake path returns. ``script_path`` is
    # honoured by the supervisor side at its own copy of this function — the
    # worker passes node_names only. The direct-lake path below is reachable
    # only for explicit ``bwrap_role`` values. See ``compile_node`` for the
    # rationale.
    if bwrap_role is None:
        socket_path = _require_socket_path()
        # ``client_lean_semantic_payloads`` already unwraps ``nodes``
        # and returns the per-node mapping directly; the response has
        # no ``request_id`` to strip. Coerce to a plain dict-of-dicts
        # so the return type matches the direct-lake path exactly, and
        # merge the chunks by node key.
        merged: Dict[str, Dict[str, Any]] = {}
        for chunk in _chunk_node_names(list(node_names)):
            client_kwargs: Dict[str, Any] = {"timeout_secs": timeout_secs}
            if principal_names is not None:
                client_kwargs["principal_names"] = {
                    node: principal_names[node]
                    for node in chunk
                    if node in principal_names
                }
            response = client_lean_semantic_payloads(
                socket_path, repo, chunk, **client_kwargs
            )
            for node_name, entry in response.items():
                merged[node_name] = dict(entry)
        return merged

    requested_nodes: list[str] = []
    seen: set[str] = set()
    for raw_name in node_names:
        node_name = str(raw_name).strip()
        if not node_name or node_name in seen:
            continue
        seen.add(node_name)
        requested_nodes.append(node_name)

    result: Dict[str, Dict[str, Any]] = {
        node_name: {"ok": False, "payload": "", "error": ""}
        for node_name in requested_nodes
    }
    if not requested_nodes:
        return result

    exact_principals = {
        str(node): str(principal).strip()
        for node, principal in (principal_names or {}).items()
        if str(node) in seen and str(principal).strip()
    }
    missing_principals = [
        node for node in requested_nodes if node not in exact_principals
    ]
    # Explicit sandboxed/direct calls are a test and maintenance surface, not
    # the acceptance trust path. Preserve their historical top-level-node
    # shorthand; the checker server above requires the exact registration and
    # production Rust callers always provide it.
    if missing_principals and bwrap_role is not None:
        for node in missing_principals:
            exact_principals[node] = node
        missing_principals = []
    if missing_principals:
        message = (
            "exact principal registration missing for semantic fingerprint node(s): "
            + ", ".join(missing_principals)
        )
        for node_name in requested_nodes:
            result[node_name]["error"] = message
        return result

    bare_mathlib = _find_bare_mathlib_imports(repo, requested_nodes)
    if bare_mathlib:
        message = _bare_mathlib_error_message(bare_mathlib)
        for node_name in requested_nodes:
            result[node_name]["ok"] = False
            result[node_name]["payload"] = ""
            result[node_name]["error"] = message
        return result

    resolved_script_path = (
        Path(script_path) if script_path is not None
        else _lean_semantic_fingerprint_script_path()
    )
    if not resolved_script_path.exists():
        message = f"semantic fingerprint script not found: {resolved_script_path}"
        for node_name in requested_nodes:
            result[node_name]["error"] = message
        return result

    # Run one node per Lean process so memory from large transitive closures can
    # be reclaimed between nodes instead of accumulating inside one long-lived
    # `lean --run` process.
    total = len(requested_nodes)
    payloads_started = time.time()
    _progress_emit(
        f"[acceptance]   lean-semantic-payloads: starting on {total} node(s)"
    )
    for idx, node_name in enumerate(requested_nodes, start=1):
        node_started = time.time()
        _progress_emit(
            f"[acceptance]   lean-semantic-payloads ({idx}/{total}) {node_name}"
        )
        raw = _run_lake_command(
            repo,
            [
                "env",
                "lean",
                "--run",
                str(resolved_script_path),
                node_name,
                exact_principals[node_name],
            ],
            timeout_secs=timeout_secs,
            bwrap_role=bwrap_role,
        )
        stdout = str(raw.get("stdout", "") or "")
        stderr = str(raw.get("stderr", "") or "")
        entry = result[node_name]

        for line in (stdout + "\n" + stderr).splitlines():
            if line.startswith("FP\t"):
                try:
                    _, parsed_node_name, payload = line.split("\t", 2)
                except ValueError:
                    continue
                if parsed_node_name == node_name:
                    entry["ok"] = True
                    entry["payload"] = payload
                    entry["error"] = ""
            elif line.startswith("ERR\t"):
                try:
                    _, parsed_node_name, error = line.split("\t", 2)
                except ValueError:
                    continue
                if parsed_node_name == node_name:
                    entry["ok"] = False
                    entry["payload"] = ""
                    entry["error"] = error

        generic_error = ""
        if raw.get("spawn_error"):
            generic_error = str(raw["spawn_error"])
        elif raw.get("timed_out"):
            generic_error = str(raw.get("stderr") or f"timed out after {timeout_secs}s")
        elif raw.get("returncode") not in (0, None):
            generic_error = (
                f"lean semantic payload extraction failed with exit code {raw['returncode']}"
            )

        if not entry["ok"] and not entry["error"]:
            entry["error"] = generic_error or "no semantic payload emitted"

        node_duration = time.time() - node_started
        if entry["ok"]:
            _progress_emit(
                f"[acceptance]   lean-semantic-payloads ({idx}/{total}) {node_name}: ok in {node_duration:.1f}s"
            )
        else:
            _progress_emit(
                f"[acceptance]   lean-semantic-payloads ({idx}/{total}) {node_name}: fail in {node_duration:.1f}s"
            )

    _progress_emit(
        f"[acceptance]   lean-semantic-payloads: done {total}/{total} in {time.time() - payloads_started:.1f}s"
    )
    return result


__all__ = [
    "build_tablet",
    "compile_node",
    "materialization_order",
    "materialize_tablet_oleans",
    "observe_lean_semantic_payloads",
    "olean_has_current_kernel_replay",
    "olean_is_content_current",
    "print_axioms",
    "read_kernel_replay_artifact_evidence",
    "read_kernel_replay_declaration_manifest",
    "read_olean_srcclosure",
    "tablet_source_closure_hash",
    "write_olean_srcclosure",
]
