"""Sidecar grunt manager (queue redesign §2.4).

The reviewer's queue order IS the schedule: the manager holds N grunt
slots (config ``daemon.grunts``, default 2), each pinned to its own
isolated workspace (``<runtime>/sidecar/grunts/<k>/repo``), assigns
READY export queue rows front-to-back, cancels in-flight attempts
whose entry left the export (owner decision 3: remove-in-flight
cancels + rolls back), and reaps results into the ledger /
attempted-set / status surfaces.

Pass ordering (audit A2): REAP and CANCEL-WATCH run unconditionally on
every pass — a STALE export additionally suppresses cancels (never act
on a frozen view) but still reaps; the suspension / staleness /
window / budget / api-key gates apply ONLY to new-assignment, so
removing an entry under a tripped budget cap still cancels its attempt
(that is exactly when the reviewer wants to stop spend).

Rewind guard (audit A1): an export ``cycle`` strictly below the
last-seen cursor means the kernel rewound — entry generations restart
while the daemon's files persist, so the manager wipes
``attempted.json``, cancels ALL in-flight attempts, and resets every
grunt workspace; without this the (node, seq) dedupe wedges nodes
silently forever and cancellation keys collide.

Dedupe keys (audit A3): the ATTEMPTED-set keys by ``(node,
entry_seq)`` (one attempt per generation, Q5); the IN-FLIGHT dedupe
keys by NODE alone, so a remove+re-add observed mid-flight can never
double-spawn — the old attempt is cancelled first, and its published
record (if it raced the cancel) dies at the kernel's
``stale_generation`` gate.

All heavy work happens in ``trellis.sidecar.attempt`` child processes
(one per attempt, own session ⇒ ``killpg`` takes the whole attempt
down, warm lean server included); this module owns sequencing,
suspension, and bookkeeping only, so ``run_once`` is unit-testable
with a fake ``spawn_fn``.

Restart across a live pool (§2.2-§2.4): attempts OUTLIVE the manager.
``slots.json`` journals every in-flight assignment (identity + pid), so
the next daemon ADOPTS the children still running — they keep their
grunt slots, their original ``started_at_ms`` and their bookkeeping,
and the ones that finished during the gap are reaped at startup, BEFORE
the first assignment decision. Adoption is what makes the drain
sentinel safe, and it also closes a standing hole: today a ``kill -9``
(or a tmux kill-session, or an exception out of the loop) orphans its
children, whose results are never read and whose generations are then
re-attempted — a real double attempt.

An adopted slot is an ORDINARY busy slot (``AdoptedProc`` supplies the
Popen surface), so cancel-watch, rewind, reap and status need no new
code paths.
"""

from __future__ import annotations

import fcntl
import json
import os
import signal
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Dict, List, Mapping, Optional, Set

from trellis.sidecar import ledger as ledger_mod
from trellis.sidecar import spool as spool_mod
from trellis.sidecar.adopt import (
    AdoptedProc,
    attempt_is_alive,
    proc_started_at_ms,
    scan_attempt_processes,
)
from trellis.sidecar.config import SidecarConfig, read_api_key
from trellis.sidecar.spool import export_is_stale


TRANSPORT_SUSPEND_THRESHOLD = 3
TRANSPORT_SUSPEND_SECONDS = 30 * 60.0

# Outcome statuses that mean the ATTEMPT INFRASTRUCTURE failed (API
# transport, missing key at request time, post-loop daemon errors) —
# the model never got a fair shot at the node. These count toward the
# suspension counter exactly like raised exceptions and NEVER enter
# the attempted-set (the entry stays assignable once the suspension
# lifts; recording it would burn the reviewer's generation on a
# failure that had nothing to do with the node).
TRANSPORT_CLASS_STATUSES = frozenset({"error"})

# A runner that dies WITHOUT a result file is transport-class (above),
# so it never consumes the generation — a deterministically-crashing
# node would respawn every pass forever, gated only by the suspension.
# After this many CONSECUTIVE no-result crashes on the same (node,
# entry_seq), the manager records a ``crashed`` attempt in
# attempted.json (reviewer-visible; consumes the generation).
NO_RESULT_CRASH_THRESHOLD = 3

# The same livelock one level up: ANY transport-class outcome skips the
# attempted-set, so a DETERMINISTIC per-node `error` (a malformed request
# the endpoint always 400s, a node whose payload always trips the same
# server-side rejection) makes the manager respawn that (node, entry_seq)
# every pass forever — invisible to the reviewer, who sees no attempt
# row at all. After this many CONSECUTIVE `error` outcomes on the same
# (node, entry_seq), record an `error` attempt in attempted.json: the
# generation is consumed, the entry stops being re-assigned, and the
# reviewer sees the failure in the status table's attempt digest.
# Genuine transport weather is unaffected — it is not per-entry
# consecutive (it hits whatever entries are in flight) and the
# suspension counter still governs it.
#
# The two counters are disjoint by KIND and share the threshold: a reap
# WITHOUT a result file feeds NO_RESULT_CRASH_THRESHOLD (reported
# `crashed`), a reap WITH one feeds this counter (reported `error`).
# Neither kind resets the other — only a genuine non-error outcome does
# — so an entry alternating the two kinds still reaches a threshold
# instead of looping forever.
ERROR_STREAK_THRESHOLD = 3

ATTEMPTED_FILE = "attempted.json"
CURSOR_FILE = "manager_cursor.json"
STATUS_FILE = "status.json"
SLOTS_FILE = "slots.json"
SLOTS_SCHEMA = 1
STATUS_SCHEMA = 1
STATUS_ATTEMPTS_PER_NODE = 5

# ``status.json`` phase marker. ``bootstrapping`` is written BEFORE
# ``startup()`` does its work, because that work (a git clone plus an
# olean copy per grunt) can run for minutes during which the file would
# otherwise be absent or carry the previous daemon's timestamp — i.e.
# every cold start would look like an outage to anything that checks
# freshness.
PHASE_BOOTSTRAPPING = "bootstrapping"
PHASE_RUNNING = "running"


class SingletonError(RuntimeError):
    def __init__(self, pid_path: Path, existing_pid: Optional[int]) -> None:
        detail = f" (held by pid {existing_pid})" if existing_pid else ""
        super().__init__(f"sidecar daemon already running{detail}: {pid_path}")
        self.pid_path = pid_path
        self.existing_pid = existing_pid


def acquire_pid_lock(runtime_root: Path) -> int:
    """flock-based singleton (checker precedent). Returns the held fd
    (kept open for the daemon's lifetime)."""
    pid_path = Path(runtime_root) / "sidecar" / "daemon.pid"
    pid_path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(str(pid_path), os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        existing: Optional[int] = None
        try:
            data = os.read(fd, 64).decode("utf-8", "replace").strip()
            existing = int(data) if data else None
        except (OSError, ValueError):
            existing = None
        os.close(fd)
        raise SingletonError(pid_path, existing)
    os.ftruncate(fd, 0)
    os.pwrite(fd, str(os.getpid()).encode(), 0)
    return fd


class ProcRootUnavailableError(RuntimeError):
    """No readable ``/proc`` with a non-empty slot journal on disk.

    Fail-closed: without process liveness the manager cannot tell an
    adoptable child from a dead row, and freeing those slots would hand
    the same generations to a second grunt. Refuse to start (exit 2,
    like ``SingletonError``)."""

    def __init__(self, proc_root: Path, rows: int) -> None:
        super().__init__(
            f"sidecar daemon cannot verify {rows} journalled attempt(s): "
            f"{proc_root} is not a readable proc filesystem"
        )
        self.proc_root = proc_root
        self.rows = rows


def stop_sentinel_path(runtime_root: Path) -> Path:
    return Path(runtime_root) / "sidecar" / "stop"


def drain_sentinel_path(runtime_root: Path) -> Path:
    """DRAIN sentinel — "stop ASSIGNING and exit NOW, leaving the
    children to finish".

    NOT the usual pool-drain meaning of "wait for everything to empty":
    the manager returns immediately, in-flight attempts keep running
    unsupervised (they enforce their own wall), and the NEXT daemon
    adopts them off ``slots.json``. That is the point — a kernel/daemon
    redeploy costs no in-flight grunt work.

    Nothing is recorded on the way out: the attempts have not ENDED, so
    a ledger row, an attempted-set row or a published outcome would all
    be lies about work that is still running (and publishing an outcome
    would expire the queue entry, making the child's own later closure
    die ``not_queued``).

    A ``stop`` sentinel present at the same time WINS: the hard stop is
    the operator's "kill the work too"."""
    return Path(runtime_root) / "sidecar" / "drain"


def clear_startup_sentinels(
    runtime_root: Path, log: Callable[[str], None] = print
) -> List[str]:
    """Remove any sentinel that was already on disk when this daemon
    started. Returns the names removed.

    A DESTRUCTIVE bug lives in the alternative. Sentinels are unlinked
    on the way OUT of ``run_forever``, so one written while no daemon is
    running simply persists — a hand-typed ``touch .../stop`` against a
    daemon that was already dead, or an exit that never reached the
    unlink. The next daemon then runs ``startup()`` FIRST, which adopts
    every live orphan into its slots, and only afterwards reaches the
    stop branch — which ``killpg``s all of them and calls
    ``record_attempted(publish=True)`` on each, spending and publishing
    generations for attempts that were minutes from closing a node. A
    forgotten file therefore destroys live proof work at the next
    restart.

    Clearing before ``startup()`` closes it: a sentinel means "stop the
    daemon that is running now", so one that predates the daemon means
    nothing. An operator who wants this daemon to stop writes the
    sentinel again — that is one keystroke, against a class of loss that
    is not recoverable."""
    cleared: List[str] = []
    for name, path in (
        ("stop", stop_sentinel_path(runtime_root)),
        ("drain", drain_sentinel_path(runtime_root)),
    ):
        try:
            path.unlink()
        except FileNotFoundError:
            # The ordinary case: no sentinel to clear.
            continue
        except OSError as exc:
            # NOT the ordinary case, and it must not look like it. A
            # sentinel this daemon failed to remove is still on disk and
            # still armed, so the destructive path above is live: the
            # first pass will read it and kill everything startup() just
            # adopted. Swallowing this as "nothing to clear" is what
            # makes that silent.
            log(
                f"sidecar: ERROR removing the pre-existing `{name}` sentinel "
                f"{path} ({type(exc).__name__}: {exc}); it is STILL ARMED and "
                "the next pass will act on it — remove it by hand now if the "
                "adopted attempts are to survive"
            )
            continue
        cleared.append(name)
        log(
            f"sidecar: cleared a `{name}` sentinel left over from before this "
            f"daemon started ({path}); it addressed an earlier daemon. Touch "
            "it again to stop this one."
        )
    return cleared


def new_attempt_id(node: str) -> str:
    stamp = time.strftime("%Y%m%d-%H%M%S", time.gmtime())
    return f"sc-{stamp}-{uuid.uuid4().hex[:6]}-{node}"


# ---------------------------------------------------------------------------
# Durable manager files: attempted.json (Q5 dedupe + reviewer-visible
# history), manager_cursor.json (A1 rewind cursor), status.json (§2.5).
# ---------------------------------------------------------------------------


def attempted_path(runtime_root: Path) -> Path:
    return Path(runtime_root) / "sidecar" / ATTEMPTED_FILE


def load_attempted(runtime_root: Path) -> Dict[str, List[Dict[str, Any]]]:
    try:
        data = json.loads(attempted_path(runtime_root).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return data if isinstance(data, dict) else {}


def _write_json_atomic(path: Path, data: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(data, indent=2, sort_keys=True), encoding="utf-8")
    os.replace(tmp, path)


SIDECAR_OUTCOME_SCHEMA = 1


def record_attempted(
    runtime_root: Path,
    node: str,
    entry: Dict[str, Any],
    *,
    publish: bool = False,
    export_cycle: int = 0,
) -> None:
    """Record one attempt against ``(node, entry_seq)`` — the SINGLE
    writer of "this generation is spent" — and, when ``publish`` is set,
    tell the kernel so it can retire the now-dead queue entry.

    Publication is a side effect of THIS choke point rather than a
    second copy of the spend policy: everything that consumes a
    generation passes through here, so "spent" and "published" cannot
    drift apart. Callers set ``publish`` only where the generation is
    PERMANENTLY spent and the node did NOT close:

      * ``publish=True``  — ``failed`` / ``budget_exhausted`` /
        ``skipped_giant``, the threshold-burn ``crashed`` / ``error``,
        and a cancel that leaves the attempted-set standing.
      * ``publish=False`` — ``success`` (its closure is in ``pending/``;
        expiring the entry would make the kernel reject that closure
        ``not_queued`` and silently destroy completed grunt work) and
        the rewind cancel (which wipes the attempted-set in the same
        pass, so the generation is not spent at all).

    Publication is best-effort: a spool write failure must never break
    the bookkeeping write above it. The visible symptom of a broken
    outcome pipeline is a queue entry that stays ``SPENT`` in the
    reviewer's status table instead of leaving the queue — which is
    exactly why a landed publication stamps ``outcome_published`` back
    onto the attempt row: without the marker, "spent but the kernel was
    never told" is indistinguishable from "spent and reported", and
    ``health.spent_without_outcome`` could not name the entries an
    operator has to remove by hand."""
    attempted = load_attempted(runtime_root)
    attempted.setdefault(node, []).append(entry)
    _write_json_atomic(attempted_path(runtime_root), attempted)
    if not publish:
        return
    try:
        spool_mod.publish_outcome(
            spool_mod.spool_dirs(runtime_root),
            {
                "schema": SIDECAR_OUTCOME_SCHEMA,
                "attempt_id": str(entry.get("attempt_id", "")),
                "node": node,
                "entry_seq": entry.get("entry_seq"),
                "status": str(entry.get("status", "")),
                "detail": str(entry.get("detail", ""))[:200],
                "export_cycle": int(export_cycle),
                "ts": entry.get("ts"),
            },
        )
    except Exception:  # noqa: BLE001 — telemetry-class: never fatal
        return
    # ONLY after the record is in ``outcomes/``. Dying before this
    # rewrite leaves the row unmarked, so the report over-counts — the
    # direction that sends an operator to look at a healthy entry rather
    # than past a broken one.
    try:
        entry["outcome_published"] = True
        _write_json_atomic(attempted_path(runtime_root), attempted)
    except Exception:  # noqa: BLE001 — telemetry-class: never fatal
        pass


def wipe_attempted(runtime_root: Path) -> None:
    try:
        attempted_path(runtime_root).unlink()
    except OSError:
        pass


def attempted_contains(
    attempted: Mapping[str, Any], node: str, entry_seq: int
) -> bool:
    rows = attempted.get(node)
    if not isinstance(rows, list):
        return False
    return any(
        isinstance(row, Mapping) and row.get("entry_seq") == entry_seq
        for row in rows
    )


def cursor_path(runtime_root: Path) -> Path:
    return Path(runtime_root) / "sidecar" / CURSOR_FILE


def load_cursor(runtime_root: Path) -> Optional[int]:
    try:
        data = json.loads(cursor_path(runtime_root).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    value = data.get("last_seen_cycle") if isinstance(data, dict) else None
    return value if isinstance(value, int) else None


def store_cursor(runtime_root: Path, cycle: int) -> None:
    _write_json_atomic(cursor_path(runtime_root), {"last_seen_cycle": cycle})


def status_path(runtime_root: Path) -> Path:
    return Path(runtime_root) / "sidecar" / STATUS_FILE


# ---------------------------------------------------------------------------
# Slot journal (§2.2): the durable in-flight assignment table.
#
# Named slots.json rather than inflight.json on purpose — ``inflight/``
# is the SPOOL's vocabulary for a record under construction, a different
# thing entirely.
#
# A journal is unavoidable for adoption. The only other durable trace of
# an assignment is ``grunts/<k>/result-<attempt_id>.json``, which carries
# neither the node nor the entry_seq — and without the entry_seq you
# cannot key the attempted-set, publish an outcome, or stop the
# generation from being attempted a second time.
#
# One writer only: the pid-lock holder.
# ---------------------------------------------------------------------------


def slots_path(runtime_root: Path) -> Path:
    return Path(runtime_root) / "sidecar" / SLOTS_FILE


def slot_record(slot: "GruntSlot") -> Dict[str, Any]:
    """A journal row: exactly ``GruntSlot`` minus ``proc`` plus ``pid``."""
    try:
        pid = int(getattr(slot.proc, "pid", -1))
    except (TypeError, ValueError):
        pid = -1
    return {
        "grunt": slot.grunt,
        "node": slot.node,
        "entry_seq": slot.entry_seq,
        "attempt_id": slot.attempt_id,
        "pid": pid,
        "started_at_ms": slot.started_at_ms,
        "result_path": str(slot.result_path),
        "assigned_at_cycle": slot.assigned_at_cycle,
    }


def slot_from_record(record: Mapping[str, Any], proc: Any) -> Optional["GruntSlot"]:
    """Rebuild a slot from a journal row (``proc`` supplied by the
    caller: an ``AdoptedProc`` for a live child, a dead stand-in for a
    row being reaped). None when the row is malformed."""
    try:
        grunt = int(record["grunt"])
        entry_seq = int(record["entry_seq"])
        node = str(record["node"])
        attempt_id = str(record["attempt_id"])
    except (KeyError, TypeError, ValueError):
        return None
    if not node or not attempt_id:
        return None
    try:
        started_at_ms = int(record.get("started_at_ms", 0) or 0)
    except (TypeError, ValueError):
        started_at_ms = 0
    try:
        assigned_at_cycle = int(record.get("assigned_at_cycle", 0) or 0)
    except (TypeError, ValueError):
        assigned_at_cycle = 0
    return GruntSlot(
        grunt=grunt,
        node=node,
        entry_seq=entry_seq,
        attempt_id=attempt_id,
        started_at_ms=started_at_ms,
        proc=proc,
        result_path=Path(str(record.get("result_path", ""))),
        assigned_at_cycle=assigned_at_cycle,
        adopted=True,
    )


def load_slots(runtime_root: Path) -> List[Dict[str, Any]]:
    try:
        data = json.loads(slots_path(runtime_root).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    rows = data.get("slots") if isinstance(data, dict) else None
    if not isinstance(rows, list):
        return []
    return [dict(row) for row in rows if isinstance(row, Mapping)]


def store_slots(
    runtime_root: Path,
    slots: List["GruntSlot"],
    *,
    drained_at: Optional[float] = None,
    now: Optional[float] = None,
) -> None:
    """Atomically rewrite the journal. Raises on failure — the caller
    treats a journal it cannot write as a reason to stop assigning."""
    _write_json_atomic(
        slots_path(runtime_root),
        {
            "schema": SLOTS_SCHEMA,
            "written_at": time.time() if now is None else now,
            "drained_at": drained_at,
            "slots": [slot_record(slot) for slot in slots],
        },
    )


# ---------------------------------------------------------------------------
# Manager
# ---------------------------------------------------------------------------


@dataclass
class GruntSlot:
    """One busy grunt: the attempt child + its assignment identity."""

    grunt: int
    node: str
    entry_seq: int
    attempt_id: str
    started_at_ms: int
    proc: Any  # Popen-like: .poll(), .wait(), .pid
    result_path: Path
    # Export ``cycle`` this assignment was made from. Rides along into
    # the published outcome so the kernel can drop a spent-generation
    # report that was minted AFTER a cycle it has since rewound past
    # (``export_cycle > state.cycle``). Defaults to 0, which no live
    # cycle is below, so a slot built without it is never dropped.
    assigned_at_cycle: int = 0
    # Inherited from a PREVIOUS daemon at startup (§2.4) rather than
    # spawned by this one. Surfaced in status.json; otherwise this slot
    # behaves exactly like any other busy slot.
    adopted: bool = False


@dataclass
class DaemonState:
    consecutive_transport_failures: int = 0
    suspended_until: float = 0.0
    # Consecutive runner-died-without-a-result-file crashes, keyed
    # "node:entry_seq" (see NO_RESULT_CRASH_THRESHOLD). Any reap of the
    # same key WITH a result file resets its streak; a kernel rewind
    # clears the map (generations restart).
    no_result_crashes: Dict[str, int] = field(default_factory=dict)
    # Consecutive transport-class (`error`) outcomes, keyed
    # "node:entry_seq" (see ERROR_STREAK_THRESHOLD). Any non-error
    # outcome for the key resets it; a kernel rewind clears the map.
    error_streaks: Dict[str, int] = field(default_factory=dict)
    # What the last completed pass DID, for status.json. ``run_once``
    # has always returned this string and ``run_forever`` has always
    # thrown it away, which left two states invisible from outside the
    # process: a `journal_error` daemon reaps and cancels forever while
    # refusing to assign anything (one log line, then silence), and a
    # 30-minute transport `suspended` window looks exactly like `idle`.
    last_pass_status: str = ""
    last_pass_at_ms: int = 0
    # Nodes the last pass declined to assign because their closure is
    # published and waiting on the kernel's ingest (P5). Surfaced in
    # status.json so the reviewer's block can say WHY the pool is idle:
    # the skip used to be a bare `continue`, which rendered as an idle
    # pool with ready entries — and the natural response to that reading
    # is to remove and re-add the entry, minting a fresh generation,
    # which is exactly what this skip exists to avoid.
    last_pass_awaiting_ingest: List[str] = field(default_factory=list)


@dataclass
class SidecarDaemon:
    """Manager shell. ``spawn_fn(grunt, entry_row, export, attempt_id)
    -> GruntSlot`` is the injected spawn (production wiring in
    ``__main__`` launches ``trellis.sidecar.attempt`` in its own
    session); ``reset_workspace_fn(grunt, snapshot_sha)`` is the
    injected cancel/rewind rollback."""

    runtime_root: Path
    config: SidecarConfig
    spawn_fn: Callable[[int, Mapping[str, Any], Mapping[str, Any], str], GruntSlot]
    reset_workspace_fn: Optional[Callable[[int, str], None]] = None
    log: Callable[[str], None] = print
    now: Callable[[], float] = time.time
    kill_grace_seconds: float = 5.0
    state: DaemonState = field(default_factory=DaemonState)
    slots: Dict[int, Optional[GruntSlot]] = field(default_factory=dict)
    # Per-grunt workspace bootstrap, injected so ``startup`` OWNS the
    # ordering: adoption first, then bootstrap ONLY the grunts with no
    # adopted attempt. Bootstrapping a workspace whose attempt is still
    # running makes the daemon a second writer into a tree that is
    # mid-``lake build`` (hazard b).
    bootstrap_fn: Optional[Callable[[int], None]] = None
    # Injectable for tests; the liveness/identity source for adoption.
    proc_root: Path = Path("/proc")

    def __post_init__(self) -> None:
        for k in range(max(1, int(self.config.grunts))):
            self.slots.setdefault(k, None)

    # -- lifecycle ---------------------------------------------------------

    def startup(self) -> None:
        """Ordering is load-bearing (§2.4): ensure dirs, ADOPT, sweep
        with the adopted attempts excluded, bootstrap only the free
        grunts.

        Adoption must run FIRST because both later steps are destructive
        to a live attempt: the inflight sweep would move a record the
        child is about to ``os.replace`` into ``pending/`` (turning a
        finished proof into an ``error``), and the bootstrap would write
        oleans into a workspace the child is compiling in.

        A status write BRACKETS the whole thing. Bootstrapping N grunt
        workspaces takes minutes, and until the first ``run_once`` there
        was no ``status.json`` at all (first ever start) or only the
        previous daemon's (every restart) — so anything that judges the
        daemon by that file's age reported an outage on EVERY cold
        start. The opening write says "up, and busy bootstrapping"."""
        dirs = spool_mod.spool_dirs(self.runtime_root)
        spool_mod.ensure_spool_dirs(dirs)
        self._write_status(self.now(), phase=PHASE_BOOTSTRAPPING)
        try:
            self._startup_inner(dirs)
        finally:
            self._write_status(self.now())

    def _startup_inner(self, dirs: "spool_mod.SpoolDirs") -> None:
        self.adopt_orphans()
        adopted = self._busy()
        abandoned = spool_mod.sweep_inflight_to_abandoned(
            dirs, keep_attempt_ids={slot.attempt_id for slot in adopted.values()}
        )
        for path in abandoned:
            self.log(f"sidecar: abandoned crashed inflight record {path.name}")
        if self.bootstrap_fn is None:
            return
        for k in sorted(self.slots):
            if k in adopted:
                self.log(
                    f"sidecar: grunt {k} bootstrap skipped — attempt "
                    f"{adopted[k].attempt_id} is still running in that workspace"
                )
                continue
            self.bootstrap_fn(k)

    # -- adoption (§2.4) ---------------------------------------------------

    def adopt_orphans(self) -> None:
        """Take over the attempts a previous daemon left running, and
        settle the ones that ended while no daemon was watching.

        Per journal row: ALIVE (pid still running THAT attempt id) →
        adopt into its slot, original ``started_at_ms`` intact; DEAD →
        reap now unless the ledger already carries the attempt (the
        exactly-once guard for a daemon that died between bookkeeping
        and the journal write). A dead row with no result file takes the
        ordinary ``no_result_file`` path: transport-class, generation
        NOT spent, entry assignable again — identical to today's crash
        behaviour.

        Live processes with NO journal row are found by scanning /proc;
        that is the belt for a daemon SIGKILLed between ``Popen`` and
        the journal write, and the migration path for orphans of a
        pre-journal daemon."""
        rows = load_slots(self.runtime_root)
        proc_root = Path(self.proc_root)
        if rows and not proc_root.is_dir():
            raise ProcRootUnavailableError(proc_root, len(rows))

        # Attempts the journal pass ACTUALLY resolved. A row that exists
        # but could not be acted on (malformed in a non-identity field)
        # deliberately stays OUT: it must fall through to the /proc scan,
        # which reconstructs the identity from the child's own argv.
        # Keying this on "every row present" instead would leave a LIVE
        # attempt neither adopted nor scanned — its grunt index would be
        # handed to a fresh attempt whose `refresh_to_snapshot` runs
        # `git reset --hard` over the running child's compile tree.
        handled_ids: Set[str] = set()
        # (grunt -> candidate slots) for adoption.
        candidates: Dict[int, List[GruntSlot]] = {}

        for row in rows:
            attempt_id = str(row.get("attempt_id", ""))
            pid = row.get("pid")
            if attempt_is_alive(pid, attempt_id, proc_root=proc_root):
                slot = slot_from_record(
                    row, AdoptedProc(int(pid), attempt_id, proc_root=proc_root)
                )
                if slot is None:
                    self.log(
                        f"sidecar: journal row for LIVE attempt {attempt_id} is "
                        "malformed; falling back to the /proc scan for it"
                    )
                    continue
                handled_ids.add(slot.attempt_id)
                candidates.setdefault(slot.grunt, []).append(slot)
                continue
            if self._settle_dead_journal_row(row):
                handled_ids.add(attempt_id)

        for pid, identity in scan_attempt_processes(
            self.runtime_root, proc_root=proc_root
        ):
            if identity.attempt_id in handled_ids:
                continue
            self.log(
                f"sidecar: found unjournaled attempt {identity.attempt_id} "
                f"(pid {pid}, grunt {identity.grunt}) by /proc scan"
            )
            candidates.setdefault(identity.grunt, []).append(
                GruntSlot(
                    grunt=identity.grunt,
                    node=identity.node,
                    entry_seq=identity.entry_seq,
                    attempt_id=identity.attempt_id,
                    started_at_ms=proc_started_at_ms(pid, proc_root=proc_root),
                    proc=AdoptedProc(
                        pid, identity.attempt_id, proc_root=proc_root
                    ),
                    result_path=Path(identity.result_path),
                    assigned_at_cycle=0,
                    adopted=True,
                )
            )

        for grunt, slots in sorted(candidates.items()):
            # Two live attempts on one grunt index share
            # ``grunts/<k>/repo`` and would corrupt each other's compile
            # tree: keep the OLDER (it has done more work) and hard-cancel
            # the rest. Cancel first, so the winner's slot assignment is
            # not clobbered by the cancel's slot release.
            slots.sort(key=lambda s: (s.started_at_ms, s.attempt_id))
            for loser in slots[1:]:
                self._cancel_slot(grunt, loser, "duplicate grunt occupant", "")
            winner = slots[0]
            self.slots[grunt] = winner
            over = " (beyond the configured pool)" if grunt >= self._pool_size() else ""
            self.log(
                f"sidecar: adopted attempt {winner.attempt_id} on {winner.node} "
                f"(seq {winner.entry_seq}) running in grunt {grunt}{over}"
            )
        # The journal now describes THIS daemon's reality.
        self._persist_slots()

    def _settle_dead_journal_row(self, row: Mapping[str, Any]) -> bool:
        """Bookkeep (or knowingly drop) one journal row whose attempt is
        gone. Returns False when the row is too malformed to act on, so
        the caller leaves its id out of the handled set.

        The exactly-once guard now consults the LEDGER for BOTH classes,
        and for a generation-spending outcome it demands the attempted
        row AS WELL. A row is dropped only when EVERY durable write this
        outcome owns is on disk; any half-written state re-reaps.

        AUDIT P1. Keying the generation-spending class on `attempted.json`
        alone reintroduced the strand the write ordering was chosen to
        prevent. `record_attempted` makes THREE durable writes —
        `attempted.json`, then `spool.publish_outcome`, then
        `attempted.json` again to stamp `outcome_published` — and
        `attempted_contains` sees only the FIRST. A crash between writes
        one and two left a row that reads "already bookkept" while the
        kernel had never been told the generation was spent: the next
        daemon dropped the row, no outcome record was ever published, and
        the queue entry sat SPENT forever with no automatic recovery.
        The ledger row goes LAST of all, so requiring it closes exactly
        that window.

        AUDIT F1 (still live, and why the ledger is not the SOLE key).
        The inverse half-write — a ledger row with no attempted row —
        would leave the generation unspent and assignable, so it must
        re-reap too. The current ordering cannot produce that state, but
        a row written by a pre-F1 daemon can, and a guard that reads
        `ledger` alone would silently reassign a spent generation. The
        conjunction is the only predicate that refuses both halves.

        Re-reaping an outcome whose writes DID all land is the benign
        direction, and is what the conjunction costs: the re-published
        outcome is dropped by the kernel (`not_queued` /
        `stale_generation`) and the duplicate attempted row is deduped by
        `attempted_contains` on entry_seq."""
        attempt_id = str(row.get("attempt_id", ""))
        slot = slot_from_record(
            row, AdoptedProc(-1, attempt_id, proc_root=Path(self.proc_root))
        )
        if slot is None:
            self.log(
                f"sidecar: journal row for dead attempt {attempt_id} is "
                "malformed; nothing to settle"
            )
            return False
        outcome = self._read_result(slot)
        status = str(outcome.get("status", "error"))
        spends_generation = (
            status != "workspace_idle" and status not in TRANSPORT_CLASS_STATUSES
        )
        already = ledger_mod.contains_attempt(self.runtime_root, attempt_id)
        if already and spends_generation:
            already = attempted_contains(
                load_attempted(self.runtime_root), slot.node, slot.entry_seq
            )
        if already:
            # The previous daemon bookkept it and died before rewriting
            # the journal. Re-reaping would double-count.
            self.log(
                f"sidecar: journalled attempt {attempt_id} is already "
                "bookkept; dropping the stale slot row"
            )
            return True
        self.log(
            f"sidecar: reaping attempt {attempt_id} on {slot.node} "
            f"(seq {slot.entry_seq}) that ended while no manager was running"
        )
        self._bookkeep_outcome(slot, outcome)
        return True

    def run_once(self) -> str:
        """One manager pass. Returns a status string (for tests and the
        loop's log). See the module docstring for the A2 ordering.

        A thin wrapper over ``_run_once_inner`` whose only job is to
        record the pass's verdict in ``status.json`` before returning
        it, so a daemon that is alive but refusing to work (see
        ``DaemonState.last_pass_status``) says so out loud. Splitting it
        this way keeps every ``run_once(...) == "…"`` contract — the
        callers', the loop's and the tests' — exactly as it was."""
        status = self._run_once_inner()
        self.state.last_pass_status = status
        self.state.last_pass_at_ms = int(self.now() * 1000)
        self._write_status(self.now())
        return status

    def _run_once_inner(self) -> str:
        now = self.now()
        # Reset up front: every early return below is a pass that reached
        # no assignment loop, and carrying the previous pass's held list
        # into status.json would report a stale reason.
        self.state.last_pass_awaiting_ingest = []
        export = spool_mod.read_candidates(self.runtime_root)
        stale = export_is_stale(
            self.runtime_root, self.config.export_stale_after_seconds, now=now
        )

        # REAP — unconditional (A2).
        self._reap()

        # A1 rewind guard — before the cancel-watch so a rewind's
        # wholesale cancel+wipe+reset is never preempted by a partial
        # per-entry cancel against colliding keys.
        if export is not None:
            self._observe_cycle(export)

        # Stop sentinel: cancel everything in flight, then exit. Checked
        # BEFORE drain so a hard stop wins when both sentinels exist.
        if stop_sentinel_path(self.runtime_root).exists():
            self._cancel_all("stop", self._snapshot_of(export))
            self._write_status(now)
            return "stop"

        # Drain sentinel: stop ASSIGNING and exit, leaving the children
        # running for the next daemon to adopt. Nothing is killed and
        # NOTHING is recorded — these attempts have not ended.
        if drain_sentinel_path(self.runtime_root).exists():
            for _, slot in sorted(self._busy().items()):
                pid = getattr(slot.proc, "pid", -1)
                self.log(
                    f"sidecar: draining — leaving attempt {slot.attempt_id} "
                    f"(pid {pid}) on {slot.node} (seq {slot.entry_seq}) "
                    f"running in grunt {slot.grunt}"
                )
            self._persist_slots(drained_at=now)
            self._write_status(now)
            return "drain"

        # CANCEL-WATCH — unconditional except on a STALE export (frozen
        # view: never cancel from it; still reaped above) (A2).
        if export is not None and not stale:
            self._cancel_removed(export)

        self._write_status(now)

        # New-assignment gates ONLY from here down (A2).
        if now < self.state.suspended_until:
            return "suspended"
        if export is None:
            return "no_export"
        if stale:
            return "stale_export"
        if not export.get("sidecar_window_open", False):
            return "window_shut"
        if read_api_key(self.config) is None:
            return "no_api_key"

        queue = export.get("queue")
        queue = queue if isinstance(queue, list) else []
        if not queue:
            # Owner decision 1: empty queue ⇒ grunts idle.
            return "queue_empty"

        attempted = load_attempted(self.runtime_root)
        busy_nodes = {slot.node for slot in self._busy().values()}
        # A node whose closure is published and awaiting kernel ingest is
        # already solved: re-attempting it burns a slot on a duplicate the
        # eligibility gate refuses.
        awaiting_ingest = spool_mod.pending_nodes(
            spool_mod.spool_dirs(self.runtime_root)
        )
        assigned = 0
        budget_blocked = False
        journal_error = False
        journal_probed = False
        # P5: ready entries held back ONLY because their closure is
        # awaiting kernel ingest. Named, not silent — see
        # `DaemonState.last_pass_awaiting_ingest`.
        held_for_ingest: List[str] = []
        for row in queue:
            if not isinstance(row, Mapping) or row.get("status") != "ready":
                continue
            node = str(row.get("node", ""))
            entry_seq = row.get("entry_seq")
            if not node or not isinstance(entry_seq, int):
                continue
            # A3: in-flight dedupe by NODE (a remove+re-add mid-flight
            # must cancel first, never double-spawn).
            if node in busy_nodes:
                continue
            if node in awaiting_ingest:
                if node not in held_for_ingest:
                    held_for_ingest.append(node)
                    self.log(
                        f"sidecar: not assigning {node} (seq {entry_seq}): its "
                        "closure is published and awaiting kernel ingest — the "
                        "entry stays ready until the kernel applies it"
                    )
                continue
            # Q5: one attempt per (node, entry_seq) generation.
            if attempted_contains(attempted, node, entry_seq):
                continue
            free = next(
                (k for k, slot in sorted(self.slots.items()) if slot is None), None
            )
            if free is None:
                break
            # F12: budget re-checked per SPAWN, not once per pass — an
            # earlier spawn's spend can trip the cap mid-pass.
            if not ledger_mod.budget_allows_new_attempt(
                self.runtime_root,
                self.config.daily_tokens,
                self.config.monthly_tokens,
                now=self.now(),
            ):
                budget_blocked = True
                break
            # NEVER SPAWN WHAT YOU CANNOT JOURNAL: an unjournaled child
            # is invisible to the next daemon's adoption (only the /proc
            # scan can still find it), so prove the journal is writable
            # BEFORE the first Popen of the pass.
            if not journal_probed:
                journal_probed = True
                if not self._persist_slots():
                    journal_error = True
                    break
            attempt_id = new_attempt_id(node)
            self.log(
                f"sidecar: assigning {node} (seq {entry_seq}) to grunt {free} "
                f"as {attempt_id}"
            )
            try:
                slot = self.spawn_fn(free, row, export, attempt_id)
            except Exception as exc:  # noqa: BLE001 — spawn is transport-class
                self._note_transport_failure(f"spawn failed: {type(exc).__name__}")
                break
            # ORDERING: journal AFTER Popen (the pid is only knowable
            # once the child exists) and before the next loop iteration.
            if not self._set_slot(free, slot):
                journal_error = True
                busy_nodes.add(node)
                assigned += 1
                break
            busy_nodes.add(node)
            assigned += 1
        self.state.last_pass_awaiting_ingest = held_for_ingest
        if assigned:
            self._write_status(self.now())
        if journal_error:
            return "journal_error"
        if assigned:
            return "assigned"
        if budget_blocked:
            return "budget_cap"
        if held_for_ingest:
            # A pass that assigned nothing BECAUSE every ready entry is
            # waiting on the kernel is not idle, and must not report as
            # idle: `idle` reads as spare capacity and invites the
            # remove-and-re-add that mints a fresh generation.
            return "awaiting_ingest"
        return "idle"

    def run_forever(self) -> None:
        # BEFORE startup(), which adopts live orphans that the stop
        # branch would then kill and spend. See clear_startup_sentinels.
        clear_startup_sentinels(self.runtime_root, self.log)
        self.startup()
        while True:
            status = self.run_once()
            if status == "stop":
                self.log("sidecar: stop sentinel present; exiting")
                try:
                    stop_sentinel_path(self.runtime_root).unlink()
                except OSError:
                    pass
                return
            if status == "drain":
                self.log(
                    f"sidecar: drain sentinel present; exiting with "
                    f"{len(self._busy())} attempt(s) still running "
                    "(the next daemon adopts them)"
                )
                try:
                    drain_sentinel_path(self.runtime_root).unlink()
                except OSError:
                    pass
                return
            time.sleep(self.config.poll_seconds)

    # -- slots ---------------------------------------------------------------

    def _busy(self) -> Dict[int, GruntSlot]:
        return {k: slot for k, slot in self.slots.items() if slot is not None}

    def _pool_size(self) -> int:
        return max(1, int(self.config.grunts))

    def _persist_slots(self, drained_at: Optional[float] = None) -> bool:
        """Rewrite ``slots.json``. False (logged at error level) means
        the journal is unwritable: the caller must refuse to assign,
        because an attempt this daemon cannot journal is one the next
        daemon cannot adopt. Reaping and cancelling continue regardless
        — they only ever REMOVE in-flight work."""
        try:
            store_slots(
                self.runtime_root,
                [slot for _, slot in sorted(self._busy().items())],
                drained_at=drained_at,
            )
            return True
        except Exception as exc:  # noqa: BLE001 — fail-closed, never fatal
            self.log(
                f"sidecar: ERROR writing the slot journal "
                f"{slots_path(self.runtime_root)} ({type(exc).__name__}: {exc}); "
                "refusing new assignments until it is writable"
            )
            return False

    def _set_slot(self, k: int, slot: Optional[GruntSlot]) -> bool:
        """Single mutation point for ``slots`` — every change is
        journalled. Returns the journal-write result."""
        if slot is None and k >= self._pool_size():
            # An adopted attempt from a LARGER pool: hold the index only
            # while it runs, then drop it entirely rather than leaving a
            # free slot behind (that would silently grow the pool).
            self.slots.pop(k, None)
        else:
            self.slots[k] = slot
        return self._persist_slots()

    def _snapshot_of(self, export: Optional[Mapping[str, Any]]) -> str:
        if isinstance(export, Mapping):
            return str(export.get("snapshot_sha", ""))
        return ""

    def _reap(self) -> None:
        for k, slot in list(self._busy().items()):
            if slot.proc.poll() is None:
                continue
            outcome = self._read_result(slot)
            # ORDERING: the journal row is dropped strictly AFTER
            # bookkeeping returns. A crash in between leaves a dead row
            # whose attempt is already in the ledger — the next daemon's
            # ``contains_attempt`` guard drops it instead of re-reaping.
            self._bookkeep_outcome(slot, outcome)
            self._set_slot(k, None)

    def _read_result(self, slot: GruntSlot) -> Dict[str, Any]:
        try:
            data = json.loads(slot.result_path.read_text(encoding="utf-8"))
            if isinstance(data, dict):
                return data
        except (OSError, ValueError):
            pass
        return {
            "status": "error",
            "attempt_id": slot.attempt_id,
            "detail": "attempt exited without a result file",
            "no_result_file": True,
        }

    def _bookkeep_outcome(self, slot: GruntSlot, outcome: Mapping[str, Any]) -> None:
        status = str(outcome.get("status", "error"))
        if status == "workspace_idle":
            # Not an attempt at all: neither bookkeeping nor a counter
            # move (neutral for the suspension counter).
            self.log(
                f"sidecar: grunt {slot.grunt} idle on {slot.node}: "
                f"{outcome.get('detail', '')}"
            )
            return
        # Ledger row for every real outcome — transport-class included:
        # tokens may have been spent before the transport broke, and the
        # ledger is the spend accounting of record.
        #
        # It is built here but APPENDED LAST, after every other durable
        # write this outcome makes. A crash inside `_bookkeep_outcome`
        # must never leave a ledger row standing for an attempted-set row
        # that was never written: startup adoption reads these files to
        # decide whether a dead attempt was already bookkept, and the
        # last write is the only one it can safely treat as "all of it
        # landed". The inverted risk is benign — a re-reap re-publishes
        # an outcome the kernel drops (`not_queued` / `stale_generation`)
        # and `attempted_contains` still dedupes by entry_seq.
        row: Dict[str, Any] = {
            "attempt_id": outcome.get("attempt_id", slot.attempt_id),
            "node": slot.node,
            "entry_seq": slot.entry_seq,
            "grunt": slot.grunt,
            "status": status,
            "detail": str(outcome.get("detail", "")),
            "iterations": outcome.get("iterations", 0),
            "prompt_tokens": outcome.get("prompt_tokens", 0),
            "completion_tokens": outcome.get("completion_tokens", 0),
            "wall_secs": outcome.get("wall_secs", 0.0),
            "snapshot_sha": outcome.get("snapshot_sha", ""),
        }
        # Phase telemetry (grunt-bench fair-cost instrumentation):
        # additive, present only when the runner reported it.
        timings = outcome.get("timings")
        if isinstance(timings, Mapping) and timings:
            row["timings"] = dict(timings)
        crash_key = f"{slot.node}:{slot.entry_seq}"
        if status in TRANSPORT_CLASS_STATUSES:
            # Transport/API-class outcome: count toward suspension and
            # keep the attempted-set clean — the entry stays assignable
            # once the suspension lifts (F1). Exception: a runner that
            # keeps dying WITHOUT a result file is likelier a
            # deterministic per-node crash than transport; after
            # NO_RESULT_CRASH_THRESHOLD consecutive such deaths on this
            # (node, entry_seq), record a ``crashed`` attempt so the
            # generation is consumed and the reviewer sees it.
            errors = self.state.error_streaks.get(crash_key, 0)
            if outcome.get("no_result_file"):
                crashes = self.state.no_result_crashes.get(crash_key, 0) + 1
                self.state.no_result_crashes[crash_key] = crashes
            else:
                # A result file arrived — the runner is alive; the
                # no-result streak (if any) is broken. This is the
                # population ERROR_STREAK_THRESHOLD counts: the runner
                # reported a transport-class error rather than dying.
                crashes = 0
                self.state.no_result_crashes.pop(crash_key, None)
                errors += 1
                self.state.error_streaks[crash_key] = errors
            # Either streak reaching its threshold consumes the
            # generation. The no-result form is reported specifically
            # (`crashed`); anything else is the generic `error` guard.
            burn_status = ""
            burn_detail = ""
            if crashes >= NO_RESULT_CRASH_THRESHOLD:
                burn_status = "crashed"
                burn_detail = (
                    f"attempt runner died without a result file "
                    f"{crashes} times consecutively"
                )
            elif errors >= ERROR_STREAK_THRESHOLD:
                burn_status = "error"
                burn_detail = (
                    f"attempt errored {errors} times consecutively; last: "
                    f"{str(outcome.get('detail', '')) or 'transport error'}"
                )
            if burn_status:
                self.state.no_result_crashes.pop(crash_key, None)
                self.state.error_streaks.pop(crash_key, None)
                self.log(
                    f"sidecar: {slot.node} (seq {slot.entry_seq}): "
                    f"{burn_detail}; recording a {burn_status} attempt "
                    "(consumes the generation)"
                )
                record_attempted(
                    self.runtime_root,
                    slot.node,
                    {
                        "entry_seq": slot.entry_seq,
                        "attempt_id": outcome.get("attempt_id", slot.attempt_id),
                        "status": burn_status,
                        "detail": burn_detail[:500],
                        "ts": self.now(),
                    },
                    publish=True,
                    export_cycle=slot.assigned_at_cycle,
                )
            ledger_mod.append_ledger_row(self.runtime_root, row)
            self._note_transport_failure(
                str(outcome.get("detail", "")) or "transport error"
            )
            return
        self.state.no_result_crashes.pop(crash_key, None)
        self.state.error_streaks.pop(crash_key, None)
        self.state.consecutive_transport_failures = 0
        record_attempted(
            self.runtime_root,
            slot.node,
            {
                "entry_seq": slot.entry_seq,
                "attempt_id": outcome.get("attempt_id", slot.attempt_id),
                "status": status,
                "detail": str(outcome.get("detail", ""))[:500],
                "iterations": outcome.get("iterations", 0),
                "prompt_tokens": outcome.get("prompt_tokens", 0),
                "completion_tokens": outcome.get("completion_tokens", 0),
                "wall_secs": outcome.get("wall_secs", 0.0),
                "ts": self.now(),
            },
            # Every non-transport outcome that is not a success spends
            # the generation for good: `failed`, `budget_exhausted`,
            # `skipped_giant`, and anything a future runner reports in
            # that class. `success` is the ONE exclusion — this branch
            # runs for it unconditionally, so the exclusion has to be
            # explicit here.
            publish=status != "success",
            export_cycle=slot.assigned_at_cycle,
        )
        # LAST: see the row comment above.
        ledger_mod.append_ledger_row(self.runtime_root, row)

    # -- cancellation (owner decision 3) --------------------------------------

    def _kill_group(self, proc: Any) -> None:
        """killpg(TERM) → grace → killpg(KILL). Falls back to
        terminate()/kill() for fakes without a real process group."""

        # Nothing to signal. Matters for ADOPTED procs: their ``.pid``
        # reads -1 once dead, and this early-out keeps the whole
        # signalling dance off a process the daemon no longer owns.
        if proc.poll() is not None:
            return

        def _signal(sig: int, hard: bool) -> None:
            try:
                os.killpg(os.getpgid(proc.pid), sig)
            except (OSError, AttributeError, TypeError):
                try:
                    (proc.kill if hard else proc.terminate)()
                except Exception:  # noqa: BLE001 — already dead
                    pass

        _signal(signal.SIGTERM, hard=False)
        deadline = time.monotonic() + self.kill_grace_seconds
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                break
            time.sleep(0.05)
        if proc.poll() is None:
            _signal(signal.SIGKILL, hard=True)
            try:
                proc.wait(timeout=self.kill_grace_seconds)
            except Exception:  # noqa: BLE001 — best effort
                pass

    def _sweep_inflight_for(self, attempt_id: str) -> None:
        """F12: a killed attempt may leave a half-written record in the
        shared spool ``inflight/`` — move it to ``abandoned/`` so it can
        never be published."""
        dirs = spool_mod.spool_dirs(self.runtime_root)
        src = dirs.inflight / spool_mod.attempt_file_name(attempt_id)
        if src.exists():
            dirs.abandoned.mkdir(parents=True, exist_ok=True)
            try:
                os.replace(src, dirs.abandoned / src.name)
                self.log(f"sidecar: swept cancelled inflight record {src.name}")
            except OSError:
                pass

    def _cancel_slot(
        self,
        k: int,
        slot: GruntSlot,
        reason: str,
        snapshot_sha: str,
        spend_generation: bool = True,
    ) -> None:
        self.log(
            f"sidecar: cancelling grunt {k} attempt {slot.attempt_id} "
            f"on {slot.node} (seq {slot.entry_seq}): {reason}"
        )
        self._kill_group(slot.proc)
        self._sweep_inflight_for(slot.attempt_id)
        # History surface: the reviewer sees the cancel in the status
        # table. Harmless for Q5 dedupe — the generation left the queue
        # with the remove, and a re-add mints a fresh one.
        #
        # `spend_generation=False` is the REWIND cancel: the caller wipes
        # the attempted-set in the same pass, so this generation is not
        # spent and must not be published as spent (the kernel has
        # rewound; its queue entries are the pre-rewind ones).
        record_attempted(
            self.runtime_root,
            slot.node,
            {
                "entry_seq": slot.entry_seq,
                "attempt_id": slot.attempt_id,
                "status": "cancelled",
                "detail": reason,
                "ts": self.now(),
            },
            publish=spend_generation,
            export_cycle=slot.assigned_at_cycle,
        )
        # LAST durable write of this cancel, for the same reason as in
        # `_bookkeep_outcome`: startup adoption may only conclude "already
        # bookkept" from a write that could not have preceded the others.
        ledger_mod.append_ledger_row(
            self.runtime_root,
            {
                "attempt_id": slot.attempt_id,
                "node": slot.node,
                "entry_seq": slot.entry_seq,
                "grunt": slot.grunt,
                "status": "cancelled",
                "detail": reason,
                "snapshot_sha": snapshot_sha,
            },
        )
        if self.reset_workspace_fn is not None:
            try:
                self.reset_workspace_fn(k, snapshot_sha)
            except Exception as exc:  # noqa: BLE001 — cache rebuild, non-fatal
                self.log(
                    f"sidecar: grunt {k} workspace reset failed "
                    f"({type(exc).__name__}); next refresh rebuilds it"
                )
        self._set_slot(k, None)

    def _cancel_removed(self, export: Mapping[str, Any]) -> None:
        queue = export.get("queue")
        queue = queue if isinstance(queue, list) else []
        live_keys = {
            (str(row.get("node", "")), row.get("entry_seq"))
            for row in queue
            if isinstance(row, Mapping)
        }
        snapshot = self._snapshot_of(export)
        for k, slot in list(self._busy().items()):
            if (slot.node, slot.entry_seq) not in live_keys:
                self._cancel_slot(k, slot, "entry left the queue", snapshot)

    def _cancel_all(
        self, reason: str, snapshot_sha: str, spend_generation: bool = True
    ) -> None:
        for k, slot in list(self._busy().items()):
            self._cancel_slot(
                k, slot, reason, snapshot_sha, spend_generation=spend_generation
            )

    # -- A1 rewind guard -------------------------------------------------------

    def _observe_cycle(self, export: Mapping[str, Any]) -> None:
        cycle = export.get("cycle")
        if not isinstance(cycle, int):
            return
        last = load_cursor(self.runtime_root)
        if last is not None and cycle < last:
            self.log(
                f"sidecar: kernel rewind detected (export cycle {cycle} < "
                f"last seen {last}); wiping attempted-set, cancelling all "
                "in-flight attempts, resetting all grunt workspaces"
            )
            snapshot = self._snapshot_of(export)
            # The wipe below un-spends every generation, so these
            # cancels publish NO spent-generation outcome: the kernel
            # rewound, and its (restored) queue entries are assignable
            # again the moment the export catches up.
            self._cancel_all("kernel rewind", snapshot, spend_generation=False)
            wipe_attempted(self.runtime_root)
            # Generations restart on rewind: stale no-result / error
            # streaks must not consume a fresh generation's key.
            self.state.no_result_crashes.clear()
            self.state.error_streaks.clear()
            if self.reset_workspace_fn is not None:
                for k in sorted(self.slots):
                    try:
                        self.reset_workspace_fn(k, snapshot)
                    except Exception as exc:  # noqa: BLE001
                        self.log(
                            f"sidecar: grunt {k} rewind reset failed "
                            f"({type(exc).__name__})"
                        )
        store_cursor(self.runtime_root, cycle)

    # -- surfaces ---------------------------------------------------------------

    def _write_status(self, now: float, phase: str = PHASE_RUNNING) -> None:
        """The daemon's one outward-facing heartbeat.

        Every field beyond ``generated_at_ms`` / ``grunts`` /
        ``in_flight`` / ``attempts`` is OPTIONAL to a reader: a daemon
        from before this version writes none of them, and every consumer
        degrades to what it can see rather than assuming a default.

        ``pid`` + ``poll_seconds`` exist so a reader on a hot path can
        answer "is it alive?" and "how stale is too stale?" from THIS
        file alone — without opening ``daemon.pid``, which means without
        an ``open()`` that can hang on a dead mount and without an
        ``flock`` that would make a concurrently starting daemon refuse
        to start."""
        attempted = load_attempted(self.runtime_root)
        attempts = {
            node: rows[-STATUS_ATTEMPTS_PER_NODE:]
            for node, rows in attempted.items()
            if isinstance(rows, list) and rows
        }
        last_pass: Dict[str, Any] = {}
        if self.state.last_pass_status:
            last_pass = {
                "status": self.state.last_pass_status,
                "at_ms": self.state.last_pass_at_ms,
            }
            # Additive: present only when the last pass actually held
            # something back for ingest, so an older reader is unaffected.
            if self.state.last_pass_awaiting_ingest:
                last_pass["awaiting_ingest"] = list(
                    self.state.last_pass_awaiting_ingest
                )
        _write_json_atomic(
            status_path(self.runtime_root),
            {
                "schema": STATUS_SCHEMA,
                "generated_at_ms": int(now * 1000),
                "pid": os.getpid(),
                "phase": phase,
                "poll_seconds": float(self.config.poll_seconds),
                # 0 == not suspended. A live suspension is otherwise
                # indistinguishable from an idle pool: same empty
                # in_flight, same free slots, no assignments either way.
                "suspended_until_ms": int(self.state.suspended_until * 1000),
                **({"last_pass": last_pass} if last_pass else {}),
                "grunts": len(self.slots),
                "in_flight": [
                    {
                        "node": slot.node,
                        "entry_seq": slot.entry_seq,
                        "grunt": slot.grunt,
                        "attempt_id": slot.attempt_id,
                        "started_at_ms": slot.started_at_ms,
                        # Only on inherited attempts (§2.5): consumers read
                        # in_flight rows by key and tolerate extra ones.
                        **({"adopted": True} if slot.adopted else {}),
                    }
                    for _, slot in sorted(self._busy().items())
                ],
                "attempts": attempts,
            },
        )

    def _note_transport_failure(self, detail: str) -> None:
        self.state.consecutive_transport_failures += 1
        self.log(
            f"sidecar: attempt error ({detail}); "
            f"{self.state.consecutive_transport_failures} consecutive"
        )
        if self.state.consecutive_transport_failures >= TRANSPORT_SUSPEND_THRESHOLD:
            self.state.suspended_until = self.now() + TRANSPORT_SUSPEND_SECONDS
            self.state.consecutive_transport_failures = 0
            self.log("sidecar: suspending new assignments for 30 minutes")
