"""Asynchronous kernel-side heartbeat measurement.

Lean heartbeats are an in-process elaborator counter, unreadable at the
``lake build`` boundary, and the instrumentation that CAN read them consumes
heartbeats from the same per-declaration budget it measures — a node near the
default ceiling builds clean uninstrumented and fails deterministically under
instrumentation. So the acceptance build is never instrumented. Instead this
module runs a SEPARATE measurement build, in the background, kernel-side
only:

  * the checker server enqueues a node here when a fresh elaboration-cost
    measurement arrives without a heartbeat count
    (``CheckerServer._memoize_elaboration_cost``);
  * one niced worker thread, at most one measurement at a time, re-elaborates
    the node by invoking ``lean`` DIRECTLY — heartbeat plugin loaded in its
    exact synchronous mode, cap raised to unlimited so the measurement can
    never abort on budget, ``LEAN_PATH`` pointing at the already-built
    dependency oleans, output discarded into a scratch dir — inside the same
    ``lake_compiler`` bwrap confinement the server's lake runs use. TWO such
    passes run per node: the headline one with kernel type-checking skipped
    (elaboration only) and a companion full build (total cost);
  * the completed count is spooled to
    ``<runtime_root>/checker-state/heartbeat-measurements/<node>.json``
    (atomic rename, last-wins per node), keyed by the source-closure hash it
    was ACTUALLY measured under;
  * the kernel runtime drains that spool onto the next worker response
    (``drain_pending_heartbeat_measurements`` in ``kernel/src/runtime.rs``)
    and the engine merges each count into the node's existing
    ``ElaborationCostRecord``.

FAIL-OPEN CONTRACT (the most important property): plugin missing, lean
missing, bwrap missing, memory too low, node changed before or during the
measurement, lean crashing or timing out, garbage side-file output, spool
unwritable — every one of these degrades to "no heartbeat data" and has zero
effect on anything else. Nothing here may ever influence acceptance,
scheduling, or dispatch; the output is pure telemetry. Work is DROPPED,
never queued without bound: a bounded queue plus a full-queue drop means
falling behind yields fewer measurements, never a backlog.

WHAT ``heartbeats`` MEANS, and why the kernel is excluded. The spooled
``heartbeats`` is ELABORATION ONLY: the quantity ``set_option maxHeartbeats``
actually enforces. Kernel type-checking is NOT separately budgeted, and it
contaminates the measured window only because we measure synchronously —
``Lean/AddDecl.lean`` runs ``doAddAndCommit`` inline exactly when
``Elab.async`` is false, whereas with async on it goes through
``Core.wrapAsyncAsSnapshot`` onto a thread with its own counter. A sync
bracket that left kernel checking enabled would therefore report a number
inflated by contamination its own measurement created (observed 1.02x-1.53x,
tracking kernel share 0.6%-34.0%), and comparing that to a node's budget
flags healthy nodes as over-limit. Skipping the kernel check removes it at
the source; the companion pass keeps the total available as an extra key.

Accuracy: validated against real-build bisection — the enforced limit for
``BigToSmallZeroRungNeighborCase`` sits between ``-DmaxHeartbeats=9000``
(fails) and ``9300`` (passes), and this measurement lands within a fraction
of a percent of the bisected value. Counts are deterministic: repeated
measurements of unchanged content are bit-identical.
"""

from __future__ import annotations

import json
import os
import queue
import re
import shutil
import subprocess
import tempfile
import threading
import time
import uuid
from pathlib import Path
from typing import Callable, Optional, Tuple

# Wire-format env var the plugin reads; keep in lockstep with
# ``observations._HB_OUT_ENV`` (imported lazily to stay fail-open).
_HB_OUT_ENV = "TRELLIS_HB_OUT"
# Plugin mode switches, both set only here.
#
# ``_HB_SYNC_ENV`` selects the EXACT measurement mode: the plugin forces
# ``Elab.async := false`` for the declaration and reports the plain
# main-thread heartbeat delta. The plugin's other mode recovers async costs
# from the profiler trace, whose own cost lands in the same budget and
# inflates the result by a per-node factor (measured 1.02x-1.44x across ten
# nodes — enough to REORDER them, which defeats the only purpose these
# numbers have). Sync mode is available to us precisely because this build
# discards its olean: byte-identical output is a constraint on instrumenting
# the ACCEPTANCE build, which we never do.
#
# ``_HB_UNCAP_ENV`` asks the plugin to set ``maxHeartbeats := 0`` inside the
# declaration's own scope, which is the only thing that beats a file-local
# ``set_option maxHeartbeats N``. Kept a separate switch from the mode, and
# deliberately not implied by ``TRELLIS_HB_OUT``: disabling a heartbeat
# ceiling must never happen by accident on a build whose outcome matters.
_HB_SYNC_ENV = "TRELLIS_HB_SYNC"
_HB_UNCAP_ENV = "TRELLIS_HB_UNCAP"
# Tier-2 probe ceiling: asks the plugin to IMPOSE `maxHeartbeats := n` from
# inside the declaration's scope. The only mechanism that overrides a
# file-local `set_option maxHeartbeats` — a CLI `-DmaxHeartbeats=n` is a
# mere default and loses to it.
_HB_CAP_ENV = "TRELLIS_HB_CAP"

# Master switch. Any value other than "0"/"false"/"off" (case-insensitive)
# leaves the feature enabled; the plugin's absence is the ordinary way the
# feature stays dormant on hosts that never deployed it.
_ENABLE_ENV = "TRELLIS_HB_MEASURE"
# Override for the plugin path; default is scripts/hbprof/HbProf.so in the
# trellis source tree (mounted read-only inside the lake_compiler bwrap).
_PLUGIN_ENV = "TRELLIS_HB_PLUGIN"
# MemAvailable floor in GiB before a measurement may start (default 12,
# matching the empirical matrix runs on this host class).
_MEM_FLOOR_ENV = "TRELLIS_HB_MEM_FLOOR_GIB"
_DEFAULT_MEM_FLOOR_GIB = 12
# Hard wall-clock cap on one measurement. 30 minutes is ~7x the slowest
# real node measured (262 s, a 1.05M-heartbeat proof), and it bounds the
# cost of a pathological node: `#count_heartbeats in` in a source hangs the
# instrumented elaboration (a plugin/Mathlib-diagnostic interaction; that
# command requires an import no tablet source carries). Such a node burns
# ONE timeout — the (node, hash) dedupe set means it is never retried for
# the same content — on a background thread nothing waits on.
_TIMEOUT_ENV = "TRELLIS_HB_MEASURE_TIMEOUT_SECS"
_DEFAULT_TIMEOUT_SECS = 1800.0
# Bounded work queue: beyond this, new work is dropped (never blocks).
_QUEUE_MAX = 8
# How long to wait for memory headroom before dropping the job.
_MEM_WAIT_MAX_SECS = 1800.0
_MEM_WAIT_POLL_SECS = 60.0
# Bound on the measured-already dedupe set; cleared wholesale when exceeded
# (re-measuring a few nodes beats unbounded growth).
_DONE_SET_MAX = 4096

# ---------------------------------------------------------------------------
# Tier 2: exact enforced value by bisection, for borderline nodes only.
#
# Tier 1 (the two-pass measurement above) is elaboration-only but still spans
# `addDecl`'s post-elaboration environment construction, which the
# elaborator's own heartbeat check never sees. That makes it a slight
# OVERSTATEMENT of what Lean actually enforces — measured >= enforced,
# observed at +4.9% on a 9.3k node and +0.8% on a 199k one.
#
# THE RESIDUAL IS ONE-SIDED, AND THE TRIGGER DEPENDS ON THAT. Because
# measured >= enforced always, `measured < 95% of limit` implies
# `enforced < 95% of limit`. So this trigger can never MISS a node that is
# genuinely close to its ceiling; it can only bisect a safe node needlessly.
# Do NOT turn this into a two-sided band or an "only if measured is within
# 5% either way" test: skipping nodes whose measured value looks comfortably
# low is only sound in the direction stated here, and a symmetric rule would
# silently drop fragile nodes.
_TIER2_TRIGGER_RATIO = 0.95
# Lean's default when a node declares no `set_option maxHeartbeats`.
_DEFAULT_HEARTBEAT_LIMIT = 200_000
# Probe budget for one bisection. Each probe is a full production-like
# build, so this is the knob that bounds Tier 2's cost.
_BISECT_MAX_PROBES = 16
# Stop when the bracket is this tight relative to the upper bound; finer
# resolution than this cannot change any decision.
_BISECT_REL_TOLERANCE = 0.001
# Mirror of ``observations._HEARTBEATS_ABSURD_MAX``.
_HEARTBEATS_ABSURD_MAX = 10**12


def measurement_spool_dir(runtime_root: Path) -> Path:
    """Spool directory the kernel runtime drains; must match
    ``heartbeat_measurements_dir`` in ``kernel/src/runtime.rs``."""
    return runtime_root / "checker-state" / "heartbeat-measurements"


def default_plugin_path() -> Path:
    """`scripts/hbprof/HbProf.so` in the trellis source tree — the one
    directory the ``lake_compiler`` bwrap already mounts read-only."""
    return (
        Path(__file__).resolve().parents[2] / "scripts" / "hbprof" / "HbProf.so"
    )


def _enabled() -> bool:
    raw = os.environ.get(_ENABLE_ENV, "").strip().lower()
    return raw not in ("0", "false", "off")


def _resolve_plugin_path() -> Optional[Path]:
    raw = os.environ.get(_PLUGIN_ENV, "").strip()
    path = Path(raw) if raw else default_plugin_path()
    try:
        return path if path.is_file() else None
    except OSError:
        return None


def _mem_available_gib() -> Optional[float]:
    try:
        with open("/proc/meminfo") as handle:
            for line in handle:
                if line.startswith("MemAvailable:"):
                    return int(line.split()[1]) / (1024 * 1024)
    except (OSError, ValueError, IndexError):
        pass
    return None


def _float_env(name: str, default: float) -> float:
    try:
        raw = os.environ.get(name, "").strip()
        return float(raw) if raw else default
    except ValueError:
        return default


class HeartbeatMeasurer:
    """Single-threaded background measurement runner. Every public entry
    point is non-blocking and swallows its own failures."""

    def __init__(
        self,
        supervisor_repo: Path,
        runtime_root: Path,
        *,
        log: Optional[Callable[[str], None]] = None,
    ) -> None:
        self.supervisor_repo = Path(supervisor_repo)
        self.runtime_root = Path(runtime_root)
        self._log = log or (lambda message: None)
        self._queue: "queue.Queue[Tuple[str, str]]" = queue.Queue(maxsize=_QUEUE_MAX)
        self._pending_or_done: set[Tuple[str, str]] = set()
        self._pending_lock = threading.Lock()
        self._stop = threading.Event()
        self._thread: Optional[threading.Thread] = None

    # ------------------------------ public ------------------------------

    def enqueue(self, node: str, source_closure_hash: str) -> None:
        """Queue one measurement; drop silently on any obstacle."""
        try:
            if self._stop.is_set() or not node or not source_closure_hash:
                return
            if not self._valid_node_name(node):
                return
            job = (node, source_closure_hash)
            with self._pending_lock:
                if job in self._pending_or_done:
                    return
                if len(self._pending_or_done) > _DONE_SET_MAX:
                    self._pending_or_done.clear()
                self._pending_or_done.add(job)
            try:
                self._queue.put_nowait(job)
            except queue.Full:
                # Droppable by design: forget the job entirely so a later
                # re-elaboration of the same content may retry it.
                with self._pending_lock:
                    self._pending_or_done.discard(job)
                self._log(f"heartbeat-measure: queue full, dropped {node}")
                return
            self._ensure_thread()
        except Exception:
            pass

    def shutdown(self) -> None:
        """Signal the worker to stop; never blocks on an in-flight build."""
        try:
            self._stop.set()
        except Exception:
            pass

    # ----------------------------- internals ----------------------------

    @staticmethod
    def _valid_node_name(node: str) -> bool:
        try:
            from trellis.checker.protocol import NODE_NAME_REGEX

            return NODE_NAME_REGEX.fullmatch(node) is not None
        except Exception:
            # Conservative fallback: never let a pathological name reach a
            # command line or a file path.
            return node.isidentifier()

    def _ensure_thread(self) -> None:
        if self._thread is not None and self._thread.is_alive():
            return
        thread = threading.Thread(
            target=self._run, name="trellis-hb-measure", daemon=True
        )
        self._thread = thread
        thread.start()

    def _run(self) -> None:
        while not self._stop.is_set():
            try:
                job = self._queue.get(timeout=5.0)
            except queue.Empty:
                continue
            try:
                reason = self._measure_once(*job)
                if reason:
                    self._log(
                        f"heartbeat-measure: {job[0]} dropped ({reason})"
                    )
                else:
                    self._log(f"heartbeat-measure: {job[0]} spooled")
            except Exception as exc:  # fail-open, always
                try:
                    self._log(f"heartbeat-measure: {job[0]} error ({exc!r})")
                except Exception:
                    pass

    def _wait_for_memory_floor(self) -> bool:
        floor = _float_env(_MEM_FLOOR_ENV, float(_DEFAULT_MEM_FLOOR_GIB))
        deadline = time.monotonic() + _MEM_WAIT_MAX_SECS
        while not self._stop.is_set():
            available = _mem_available_gib()
            if available is None or available >= floor:
                # An unreadable meminfo must not disable the feature forever;
                # the niced, single-flight build is already the safety net.
                return True
            if time.monotonic() >= deadline:
                return False
            self._stop.wait(_MEM_WAIT_POLL_SECS)
        return False

    def _source_closure_hash(self, node: str) -> Optional[str]:
        try:
            from trellis.atomic_actions import observations

            return observations.tablet_source_closure_hash(
                self.supervisor_repo, node
            )
        except Exception:
            return None

    def _build_command(
        self,
        node: str,
        plugin: Path,
        scratch: Path,
        hb_out: Path,
        *,
        skip_kernel_tc: bool = False,
        bisect_cap: Optional[int] = None,
    ) -> Optional[Tuple[list, dict]]:
        """Compose the confined measurement command, or ``None``.

        Mirrors the bwrap branch of ``observations._run_lake_command``: the
        measurement elaborates worker-authored Lean, so it gets the same
        ``lake_compiler`` confinement as every supervisor-side lake run. No
        bwrap → no measurement (never fall back to an unconfined build).
        """
        try:
            from trellis.config import SandboxConfig
            from trellis.host_runtime import worker_elan_home
            from trellis.project_paths import supervisor_workspace_home_path
            from trellis.sandbox import bwrap_available, wrap_command

            if not bwrap_available():
                return None
            repo = self.supervisor_repo
            # Resolve `lean` the way the rest of the checker does: prefer the
            # elan-home proxy, fall back to PATH (hosts that install the elan
            # proxies system-wide, e.g. /usr/bin/lean, have no <elan>/bin).
            # Either way elan selects the toolchain from the workspace's
            # `lean-toolchain` file at exec time.
            lean_binary = worker_elan_home() / "bin" / "lean"
            if not lean_binary.is_file():
                which_lean = shutil.which("lean")
                if which_lean is None:
                    return None
                lean_binary = Path(which_lean)
            lean_path_dirs = [repo / ".lake" / "build" / "lib" / "lean"]
            packages_root = repo / ".lake" / "packages"
            if packages_root.is_dir():
                for package_dir in sorted(packages_root.iterdir()):
                    candidate = (
                        package_dir / ".lake" / "build" / "lib" / "lean"
                    )
                    if candidate.is_dir():
                        lean_path_dirs.append(candidate)
            lean_path = os.pathsep.join(str(d) for d in lean_path_dirs)

            if bisect_cap is not None:
                # A Tier-2 probe must elaborate exactly as PRODUCTION does —
                # async on, kernel checking on, no instrumentation — because
                # what it asks is "would the real build survive this
                # ceiling?". The plugin is loaded solely to impose the
                # ceiling from inside the declaration's scope, which is the
                # only way to override a file-local `set_option
                # maxHeartbeats` (verified: a node declaring 3,000,000
                # builds fine under `-DmaxHeartbeats=1000` and fails under
                # `TRELLIS_HB_CAP=1000`).
                probe_cmd = [
                    str(lean_binary),
                    f"--root={repo}",
                    f"--plugin={plugin}",
                    "-o",
                    str(scratch / "probe.olean"),
                    f"Tablet/{node}.lean",
                ]
                probe_setenv = [
                    "--setenv", "ELAN_HOME", str(worker_elan_home()),
                    "--setenv", "LEAN_NUM_THREADS",
                    os.environ.get("TRELLIS_LEAN_PARALLELISM", "6").strip() or "6",
                    "--setenv", "LEAN_PATH", lean_path,
                    "--setenv", _HB_CAP_ENV, str(int(bisect_cap)),
                ]
            inner_cmd = [
                str(lean_binary),
                f"--root={repo}",
                f"--plugin={plugin}",
                # Raised cap so the measurement can never abort on budget —
                # it is measuring, not gating. Note this is only a DEFAULT:
                # a file-local ``set_option maxHeartbeats N`` overrides it
                # (verified), which is why the plugin ALSO raises the cap
                # from inside the declaration's scope under
                # ``TRELLIS_HB_UNCAP`` below. Without that, the ~10% of nodes
                # carrying an override — the expensive ones — could not be
                # measured at all.
                "-DmaxHeartbeats=0",
                # Synchronous elaboration. LOAD-BEARING FOR SOUNDNESS, not
                # merely accuracy: with `Elab.async` on, work is dispatched
                # onto other threads, and heartbeats are a THREAD-LOCAL
                # allocation counter — so a parent-side delta bounds nothing
                # at all. The plugin re-asserts this inside the declaration's
                # own scope; the flag is here so module-level work outside
                # declarations is synchronous too.
                "-DElab.async=false",
                "-o",
                str(scratch / "measured.olean"),
                f"Tablet/{node}.lean",
            ]
            if skip_kernel_tc:
                # The headline pass. Synchronous elaboration pulls kernel
                # type-checking inline onto the measured thread (see
                # `Lean/AddDecl.lean`: `doAddAndCommit` runs inline exactly
                # when `Elab.async` is false), so a sync bracket that leaves
                # it enabled reports elaboration PLUS kernel — a quantity
                # `maxHeartbeats` never governed, and one this measurement
                # would itself have created. Skipping the kernel check
                # removes it at the source. The olean is discarded, so
                # producing an unchecked one costs nothing.
                inner_cmd.insert(-3, "-Ddebug.skipKernelTC=true")
            if bisect_cap is not None:
                inner_cmd = probe_cmd
            sibling_home = (
                repo.parent / "home" if repo.name == "repo" else None
            )
            if sibling_home is not None and sibling_home.exists():
                supervisor_home = sibling_home
            else:
                supervisor_home = supervisor_workspace_home_path(repo)
                supervisor_home.mkdir(parents=True, exist_ok=True)
            wrapped = wrap_command(
                inner_cmd,
                sandbox=SandboxConfig(enabled=True, backend="bwrap"),
                work_dir=repo,
                burst_home=supervisor_home,
                role="lake_compiler",
            )
            lean_threads = (
                os.environ.get("TRELLIS_LEAN_PARALLELISM", "6").strip() or "6"
            )
            extra_setenv = (
                probe_setenv
                if bisect_cap is not None
                else [
                    "--setenv", "ELAN_HOME", str(worker_elan_home()),
                    "--setenv", "LEAN_NUM_THREADS", lean_threads,
                    "--setenv", "LEAN_PATH", lean_path,
                    "--setenv", _HB_OUT_ENV, str(hb_out),
                    "--setenv", _HB_SYNC_ENV, "1",
                    "--setenv", _HB_UNCAP_ENV, "1",
                ]
            )
            wrapped = wrapped[:1] + extra_setenv + wrapped[1:]
            # Nice the whole tree: a production run shares this machine and
            # the measurement must always lose the contention.
            nice = shutil.which("nice")
            if nice:
                wrapped = [nice, "-n", "19"] + wrapped

            env = os.environ.copy()
            env["LEAN_NUM_THREADS"] = lean_threads
            env["LEAN_PATH"] = lean_path
            if bisect_cap is not None:
                # A probe takes no measurement: it must carry the ceiling and
                # NOTHING else, or an inherited sync/uncap would stop it
                # elaborating the way production does.
                env[_HB_CAP_ENV] = str(int(bisect_cap))
                for stale in (_HB_OUT_ENV, _HB_SYNC_ENV, _HB_UNCAP_ENV):
                    env.pop(stale, None)
            else:
                env[_HB_OUT_ENV] = str(hb_out)
                env[_HB_SYNC_ENV] = "1"
                env[_HB_UNCAP_ENV] = "1"
                env.pop(_HB_CAP_ENV, None)
            elan_bin = str(worker_elan_home() / "bin")
            existing_path = env.get("PATH", "")
            if elan_bin not in existing_path.split(os.pathsep):
                env["PATH"] = (
                    f"{elan_bin}{os.pathsep}{existing_path}"
                    if existing_path
                    else elan_bin
                )
            return wrapped, env
        except Exception:
            return None

    def _declared_limit(self, node: str) -> int:
        """The node's own heartbeat grant, or Lean's default.

        Takes the MAXIMUM when a file carries several `set_option
        maxHeartbeats` lines: the governing grant for the node's main
        declaration is the largest one, and using the smallest (e.g. a tiny
        scoped raise on a helper) would fire Tier 2 constantly for no gain.
        """
        try:
            text = (self.supervisor_repo / "Tablet" / f"{node}.lean").read_text(
                encoding="utf-8", errors="replace"
            )
        except OSError:
            return _DEFAULT_HEARTBEAT_LIMIT
        values = [
            int(m)
            for m in re.findall(r"set_option\s+maxHeartbeats\s+(\d+)", text)
        ]
        values = [v for v in values if v > 0]
        return max(values) if values else _DEFAULT_HEARTBEAT_LIMIT

    def _probe_builds_under_cap(
        self, node: str, plugin: Path, scratch: Path, cap: int
    ) -> Optional[bool]:
        """Does the node build with its heartbeat ceiling set to ``cap``?

        ``True``/``False`` is the verdict; ``None`` means the probe itself
        failed and the caller must abandon the bisection rather than read a
        spawn error as a heartbeat verdict.
        """
        try:
            out_dir = scratch / f"probe{cap}"
            out_dir.mkdir(parents=True, exist_ok=True)
            built = self._build_command(
                node, plugin, out_dir, out_dir / "unused.jsonl", bisect_cap=cap
            )
            if built is None:
                return None
            command, env = built
            try:
                completed = subprocess.run(
                    command,
                    cwd=str(self.supervisor_repo),
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=_float_env(_TIMEOUT_ENV, _DEFAULT_TIMEOUT_SECS),
                )
            except (subprocess.TimeoutExpired, OSError):
                return None
            if completed.returncode == 0:
                return True
            # A non-zero exit is only a heartbeat verdict when Lean says so.
            # Anything else (a genuinely broken node, a missing dependency)
            # must abandon the bisection, never read as "too slow".
            blob = f"{completed.stdout}\n{completed.stderr}".lower()
            if "maximum number of heartbeats" in blob:
                return False
            return None
        except Exception:
            return None

    def _bisect_enforced(
        self, node: str, plugin: Path, scratch: Path, upper: int
    ) -> Optional[int]:
        """Smallest heartbeat ceiling under which the node still builds.

        That is exactly what Lean enforces, so it is directly comparable to
        the node's declared limit. ``upper`` is the Tier-1 measurement,
        which is >= the enforced value and therefore a valid starting
        upper bound. Fail-open: any probe failure abandons and returns
        ``None``, leaving the Tier-1 number as the only answer.
        """
        probes = 0
        hi = max(1, int(upper))
        verdict = self._probe_builds_under_cap(node, plugin, scratch, hi)
        probes += 1
        if verdict is None:
            return None
        if verdict is False:
            # The measurement should bound the enforced value from above; if
            # it does not, the assumption behind the whole tier is wrong for
            # this node. Say nothing rather than guess.
            self._log(
                f"heartbeat-measure: {node} bisect abandoned "
                f"(node fails at its own measured value {hi})"
            )
            return None
        # Walk down for a failing lower bound. The enforced value is
        # normally within a few percent of the measurement, so this
        # terminates in one or two probes for a typical node.
        lo = 0
        for factor in (0.9, 0.75, 0.5, 0.25, 0.1):
            if probes >= _BISECT_MAX_PROBES:
                return None
            candidate = max(1, int(hi * factor))
            if candidate >= hi:
                continue
            verdict = self._probe_builds_under_cap(node, plugin, scratch, candidate)
            probes += 1
            if verdict is None:
                return None
            if verdict is False:
                lo = candidate
                break
            hi = candidate
        # Binary search the bracket (lo fails, hi passes).
        while probes < _BISECT_MAX_PROBES:
            if hi - lo <= max(1, int(hi * _BISECT_REL_TOLERANCE)):
                return hi
            mid = lo + (hi - lo) // 2
            if mid <= lo or mid >= hi:
                return hi
            verdict = self._probe_builds_under_cap(node, plugin, scratch, mid)
            probes += 1
            if verdict is None:
                return None
            if verdict:
                hi = mid
            else:
                lo = mid
        # Budget exhausted: `hi` is still a value the node demonstrably
        # builds under, so it remains a sound (if slightly loose) answer.
        return hi

    def _read_side_file(self, path: Path, node: str) -> Optional[int]:
        try:
            from trellis.atomic_actions import observations

            counts = observations._read_heartbeat_side_file(path)
        except Exception:
            return None
        heartbeats = counts.get(f"Tablet.{node}")
        if not isinstance(heartbeats, int) or isinstance(heartbeats, bool):
            return None
        if heartbeats < 0 or heartbeats > _HEARTBEATS_ABSURD_MAX:
            return None
        return heartbeats

    def _write_spool_entry(
        self,
        node: str,
        heartbeats: int,
        key: str,
        *,
        total_heartbeats: Optional[int] = None,
        bisected_heartbeats: Optional[int] = None,
        declared_limit: Optional[int] = None,
    ) -> bool:
        """Atomic last-wins spool write the kernel runtime will drain.

        ``heartbeats`` is the ELABORATION-ONLY count — the quantity
        ``set_option maxHeartbeats`` actually enforces. When the companion
        full build also succeeded, ``total_heartbeats`` and the derived
        ``kernel_heartbeats`` ride along as extra keys: the kernel's drain
        reads only the three fields it knows and ignores these, so total
        cost stays available to anything inspecting the spool without any
        schema change.
        """
        try:
            spool = measurement_spool_dir(self.runtime_root)
            spool.mkdir(parents=True, exist_ok=True)
            payload = {
                "node": node,
                "heartbeats": int(heartbeats),
                # The hash the measurement ACTUALLY ran under — never the
                # node's current hash, so a node edited since measurement
                # start installs as explicitly stale kernel-side.
                "heartbeats_key": key,
                "measured_at_unix": int(time.time()),
            }
            if total_heartbeats is not None:
                payload["total_heartbeats"] = int(total_heartbeats)
                payload["kernel_heartbeats"] = max(
                    0, int(total_heartbeats) - int(heartbeats)
                )
            if declared_limit is not None:
                payload["declared_limit"] = int(declared_limit)
            # `heartbeats` above always stays the Tier-1 measurement so
            # nothing downstream changes. When a bisection ran, its exact
            # value lands under its own key and `heartbeats_method` names
            # which figure is authoritative — a consumer must be able to
            # tell "measured, may overstate slightly" from "bisected,
            # exact".
            if bisected_heartbeats is not None:
                payload["bisected_heartbeats"] = int(bisected_heartbeats)
            payload["heartbeats_method"] = (
                "bisected" if bisected_heartbeats is not None else "measured"
            )
            fd, tmp_name = tempfile.mkstemp(
                prefix=f".{node}.", suffix=".tmp", dir=str(spool)
            )
            try:
                with os.fdopen(fd, "w", encoding="utf-8") as handle:
                    json.dump(payload, handle)
                os.replace(tmp_name, str(spool / f"{node}.json"))
            except BaseException:
                try:
                    os.unlink(tmp_name)
                except OSError:
                    pass
                raise
            return True
        except Exception:
            return False

    def _measure_once(self, node: str, expected_hash: str) -> Optional[str]:
        """Run one measurement; return a drop reason, or ``None`` on spool.

        Every early return is a silent degradation to "no data" — by
        contract none of them may have any other effect.
        """
        if not _enabled():
            return "disabled"
        plugin = _resolve_plugin_path()
        if plugin is None:
            return "plugin missing"
        current = self._source_closure_hash(node)
        if current != expected_hash:
            return "content moved before measurement"
        if not self._wait_for_memory_floor():
            return "memory floor not met"
        scratch: Optional[Path] = None
        try:
            # Scratch lives under `.lake/build` — writable inside the
            # lake_compiler bwrap at the same absolute path, and never
            # collides with real build artifacts (own subdirectory, fresh
            # uuid). The discarded olean goes here too, so the real build
            # tree is never touched.
            scratch_root = (
                self.supervisor_repo / ".lake" / "build" / "trellis-hb-measure"
            )
            scratch_root.mkdir(parents=True, exist_ok=True)
            scratch = scratch_root / uuid.uuid4().hex
            scratch.mkdir()
            # PASS 1 — the headline: elaboration only. Run FIRST so that a
            # failure of the second (context-only) pass still leaves the
            # number that matters.
            elaboration, reason = self._run_pass(
                node, plugin, scratch, "elab", skip_kernel_tc=True
            )
            if elaboration is None:
                return reason
            if self._source_closure_hash(node) != expected_hash:
                # The content moved DURING the build: neither the old nor
                # the new hash honestly describes what was elaborated.
                return "content moved during measurement"

            # PASS 2 — context: the full build, kernel checking included,
            # for total cost and the auditable kernel share. Best-effort by
            # design: any failure here degrades to "elaboration only", never
            # to losing the measurement.
            total, total_reason = self._run_pass(
                node, plugin, scratch, "total", skip_kernel_tc=False
            )
            if total is not None and self._source_closure_hash(node) != expected_hash:
                total, total_reason = None, "content moved during total pass"
            if total is not None and total < elaboration:
                # Kernel work cannot make a build cheaper; a total below the
                # elaboration figure means the two passes did not describe
                # the same work. Drop the context rather than publish a
                # negative kernel share.
                total, total_reason = None, "total below elaboration"
            if total is None:
                self._log(
                    f"heartbeat-measure: {node} total pass unavailable "
                    f"({total_reason}); spooling elaboration only"
                )

            # TIER 2 — an exact enforced value, for borderline nodes only.
            # Tier 1 overstates slightly and one-sidedly, so a node reading
            # close to its declared limit needs a definitive answer before
            # anyone calls it fragile. See `_TIER2_TRIGGER_RATIO`.
            limit = self._declared_limit(node)
            bisected: Optional[int] = None
            if elaboration >= _TIER2_TRIGGER_RATIO * limit:
                self._log(
                    f"heartbeat-measure: {node} at {100.0 * elaboration / max(1, limit):.1f}% "
                    f"of its {limit} limit; bisecting for the exact enforced value"
                )
                bisected = self._bisect_enforced(node, plugin, scratch, elaboration)
                if bisected is not None and self._source_closure_hash(node) != expected_hash:
                    bisected = None
                self._log(
                    f"heartbeat-measure: {node} bisected="
                    f"{bisected if bisected is not None else 'unavailable'} "
                    f"(measured {elaboration}, limit {limit})"
                )

            if not self._write_spool_entry(
                node,
                elaboration,
                expected_hash,
                total_heartbeats=total,
                bisected_heartbeats=bisected,
                declared_limit=limit,
            ):
                return "spool write failed"
            return None
        finally:
            if scratch is not None:
                shutil.rmtree(scratch, ignore_errors=True)

    def _run_pass(
        self,
        node: str,
        plugin: Path,
        scratch: Path,
        tag: str,
        *,
        skip_kernel_tc: bool,
    ) -> Tuple[Optional[int], Optional[str]]:
        """Run one instrumented `lean` invocation; return (count, reason).

        Each pass gets its own side file and its own output path, so the two
        cannot read each other's results. Fail-open: every failure returns
        ``(None, reason)`` and raises nothing.
        """
        try:
            hb_out = scratch / f"hb.{tag}.jsonl"
            out_dir = scratch / tag
            out_dir.mkdir(parents=True, exist_ok=True)
            built = self._build_command(
                node, plugin, out_dir, hb_out, skip_kernel_tc=skip_kernel_tc
            )
            if built is None:
                return None, "no confined command (bwrap or lean unavailable)"
            command, env = built
            timeout_secs = _float_env(_TIMEOUT_ENV, _DEFAULT_TIMEOUT_SECS)
            try:
                completed = subprocess.run(
                    command,
                    cwd=str(self.supervisor_repo),
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=timeout_secs,
                )
            except subprocess.TimeoutExpired:
                return None, "timeout"
            except OSError as exc:
                return None, f"spawn failed ({exc})"
            if completed.returncode != 0:
                return None, f"lean exited {completed.returncode}"
            heartbeats = self._read_side_file(hb_out, node)
            if heartbeats is None:
                return None, "no usable side-file entry"
            return heartbeats, None
        except Exception as exc:  # fail-open, always
            return None, f"unexpected ({type(exc).__name__})"
