"""One authoritative answer to "is the sidecar daemon alive, and is what
it last told us still true?".

Seven production incidents share one root cause: every consumer of the
sidecar's state files invented its own liveness rule, and every one of
those rules was wrong in the same direction — it read UP when the
manager was dead. The two rules that must never be used again:

  * ``pgrep -f trellis.sidecar`` — ERE, so the ``.`` matches any
    character (``trellis_sidecar.sh`` included) AND the pattern matches
    every ``trellis.sidecar.attempt`` CHILD. A dead manager with live
    children reads UP. That is exactly how a 15-minute outage went
    unnoticed.
  * "``status.json`` says two grunts are idle" — a frozen file from a
    daemon that died an hour ago says that too.

The rule here instead: the daemon's OWN pid lock is the liveness
oracle, ``/proc/<pid>/cmdline`` corroborates identity, and the status
file's age says whether its CONTENTS may still be quoted.

Two deliberate constraints:

  * Reading the pid file is ``O_RDONLY``, never ``O_CREAT``/``O_RDWR``.
    A probe that creates the lock file, or that truncates a running
    daemon's recorded pid, is a probe that changed what it measured.
  * ``probe_daemon`` is for the CLI (and any other out-of-band caller)
    ONLY. It is NOT for the review critical path: ``open()`` can block
    uninterruptibly on a dead mount, no ``try/except`` catches a hang,
    and even a momentary ``LOCK_SH`` makes a daemon that is starting
    CONCURRENTLY fail ``acquire_pid_lock`` and refuse to start with
    "already running". In-process consumers on a hot path read the pid
    out of ``status.json`` and corroborate it against ``/proc`` — see
    ``trellis/runtime/bridge_prompts.py``.

Stdlib only, and nothing here raises at a caller: an undeterminable
answer is the ``unknown`` state carrying its reason, never an
exception.
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Mapping, Optional, Sequence, Tuple

from trellis.sidecar import adopt as adopt_mod
from trellis.sidecar import spool as spool_mod


# The daemon's own ``python3 -m <this>`` module name. Matched EXACTLY —
# never as a prefix — because ``trellis.sidecar.attempt`` (the attempt
# children) starts with it, and counting a child as the manager is the
# 15-minute-outage bug in code form.
DAEMON_MODULE = "trellis.sidecar"

DEFAULT_PROC_ROOT = Path("/proc")

# How stale a ``status.json`` may be before its CONTENTS stop being
# quotable. The analogue of ``spool.export_is_stale`` for the other
# direction of the same handshake.
#
# The floor is 900s and NOT the poll interval's own order of magnitude:
# the daemon's pass can legitimately go quiet far longer than
# ``poll_seconds``. ``startup()`` bootstraps up to N grunt workspaces
# (a git clone plus an olean copy each) before the first pass, and a
# reap of a finished attempt does real work between writes. A floor
# near the poll interval fires on every cold start, and an operator
# alert that fires on every cold start is an alert nobody reads.
STATUS_STALE_FLOOR_SECONDS = 900.0

# ...and above the floor, staleness scales with the configured poll
# interval: four missed passes.
STATUS_STALE_POLL_MULTIPLIER = 4

STATE_RUNNING = "running"
STATE_NOT_RUNNING = "not_running"
STATE_UNKNOWN = "unknown"

# CLI exit codes (documented in SIDECAR_OPERATIONS.md; scripts key on
# them, so they are API).
EXIT_RUNNING = 0
EXIT_NOT_RUNNING = 1
EXIT_DEGRADED = 2
EXIT_NO_SIDECAR_DIR = 3
EXIT_UNDETERMINABLE = 4
EXIT_USAGE = 64


# ---------------------------------------------------------------------------
# Liveness
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class DaemonLiveness:
    """What the pid lock and ``/proc`` jointly say about the manager.

    ``state`` is the only field a caller should branch on. ``identity_ok``
    is corroboration, not liveness: a held lock means SOMETHING is
    running, and the argv check says whether that something is the
    daemon we meant."""

    state: str
    pid: Optional[int] = None
    argv: Optional[Tuple[str, ...]] = None
    pid_file_present: bool = False
    identity_ok: bool = False
    detail: str = ""

    @property
    def running(self) -> bool:
        return self.state == STATE_RUNNING

    def to_dict(self) -> Dict[str, Any]:
        return {
            "state": self.state,
            "pid": self.pid,
            "argv": list(self.argv) if self.argv else None,
            "pid_file_present": self.pid_file_present,
            "identity_ok": self.identity_ok,
            "detail": self.detail,
        }


def daemon_pid_path(runtime_root: Path) -> Path:
    return Path(runtime_root) / "sidecar" / "daemon.pid"


def parse_daemon_cmdline(argv: Sequence[str]) -> Optional[str]:
    """Recognise ``python3 -m trellis.sidecar <runtime_root> …`` and
    return the runtime root it names, or None.

    Pure (no filesystem, no process access), and EXACT on the module
    token: ``trellis.sidecar.attempt`` — the argv every attempt child
    carries — must never be recognised here. The same discipline as
    ``adopt.parse_attempt_cmdline``, whose comment explains why a
    substring scan is not an option."""
    argv = list(argv)
    for index, token in enumerate(argv):
        if token != DAEMON_MODULE:
            continue
        if index == 0 or argv[index - 1] != "-m":
            continue
        rest = argv[index + 1 :]
        if not rest or rest[0].startswith("-"):
            return None
        return rest[0]
    return None


def _same_path(left: str, right: str) -> bool:
    try:
        return os.path.realpath(left) == os.path.realpath(right)
    except (OSError, ValueError):  # pragma: no cover — realpath is total
        return False


def probe_daemon(
    runtime_root: Path, *, proc_root: Path = DEFAULT_PROC_ROOT
) -> DaemonLiveness:
    """CLI USE ONLY (see the module docstring): is the manager running?

    ``acquire_pid_lock`` holds ``LOCK_EX`` on ``sidecar/daemon.pid`` for
    the daemon's whole lifetime, and the kernel drops that lock when the
    process dies however it dies — ``kill -9``, an OOM, a tmux
    kill-session. So a ``LOCK_SH`` that cannot be taken is a LIVE
    daemon, and one that can be taken is a dead one, with no
    stale-pid-file failure mode in either direction.

    Never raises: any ``OSError`` becomes ``unknown`` carrying the
    reason."""
    pid_path = daemon_pid_path(runtime_root)
    if not pid_path.exists():
        return DaemonLiveness(
            state=STATE_NOT_RUNNING,
            pid_file_present=False,
            detail=f"no pid file at {pid_path}",
        )
    fd: Optional[int] = None
    try:
        # O_RDONLY: a probe must not CREATE the lock file (that would
        # invent state) and must not open O_RDWR (an accidental
        # ftruncate would erase a running daemon's recorded pid).
        fd = os.open(str(pid_path), os.O_RDONLY | os.O_CLOEXEC)
        try:
            fcntl.flock(fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
        except BlockingIOError:
            return _corroborate(
                runtime_root, _read_pid(fd), proc_root=proc_root
            )
        # The shared lock was ours to take, so no daemon holds the
        # exclusive one. Release it AT ONCE: a lock held here would make
        # a daemon starting concurrently refuse with "already running".
        try:
            fcntl.flock(fd, fcntl.LOCK_UN)
        except OSError:  # pragma: no cover — unlock of a held fd
            pass
        return DaemonLiveness(
            state=STATE_NOT_RUNNING,
            pid=_read_pid(fd),
            pid_file_present=True,
            detail=(
                "the pid-lock is free, so no daemon holds it "
                "(any pid recorded in the file is stale)"
            ),
        )
    except OSError as exc:
        return DaemonLiveness(
            state=STATE_UNKNOWN,
            pid_file_present=True,
            detail=f"cannot read {pid_path}: {type(exc).__name__}: {exc}",
        )
    finally:
        if fd is not None:
            try:
                os.close(fd)
            except OSError:  # pragma: no cover
                pass


def _read_pid(fd: int) -> Optional[int]:
    try:
        raw = os.pread(fd, 64, 0).decode("utf-8", "replace").strip()
    except OSError:
        return None
    try:
        return int(raw) if raw else None
    except ValueError:
        return None


def _corroborate(
    runtime_root: Path, pid: Optional[int], *, proc_root: Path
) -> DaemonLiveness:
    """The lock is held, so a daemon IS running; say what we can about
    which process it is."""
    if pid is None:
        return DaemonLiveness(
            state=STATE_RUNNING,
            pid_file_present=True,
            detail="the pid-lock is held, but the file records no pid",
        )
    argv = adopt_mod.read_cmdline(pid, proc_root=Path(proc_root))
    if argv is None:
        return DaemonLiveness(
            state=STATE_RUNNING,
            pid=pid,
            pid_file_present=True,
            detail=(
                f"the pid-lock is held; /proc/{pid}/cmdline is unreadable, "
                "so the identity could not be corroborated"
            ),
        )
    named_root = parse_daemon_cmdline(argv)
    if named_root is None:
        return DaemonLiveness(
            state=STATE_RUNNING,
            pid=pid,
            argv=tuple(argv),
            pid_file_present=True,
            detail=(
                f"the pid-lock is held by pid {pid}, whose argv is not a "
                "`-m trellis.sidecar <runtime_root>` daemon"
            ),
        )
    if not _same_path(named_root, str(runtime_root)):
        return DaemonLiveness(
            state=STATE_RUNNING,
            pid=pid,
            argv=tuple(argv),
            pid_file_present=True,
            detail=(
                f"the pid-lock is held by pid {pid}, a daemon over "
                f"{named_root} rather than {runtime_root}"
            ),
        )
    return DaemonLiveness(
        state=STATE_RUNNING,
        pid=pid,
        argv=tuple(argv),
        pid_file_present=True,
        identity_ok=True,
        detail=f"pid {pid} holds the pid-lock and its argv names this runtime root",
    )


# ---------------------------------------------------------------------------
# status.json freshness — the missing analogue of spool.export_is_stale
# ---------------------------------------------------------------------------


def status_file_path(runtime_root: Path) -> Path:
    return Path(runtime_root) / "sidecar" / "status.json"


def read_status(runtime_root: Path) -> Optional[Dict[str, Any]]:
    """``status.json`` as a dict, or None (absent / unreadable / not an
    object). Never raises."""
    try:
        data = json.loads(status_file_path(runtime_root).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    return data if isinstance(data, dict) else None


def status_age_seconds(
    status: Optional[Mapping[str, Any]], *, now: float
) -> Optional[float]:
    """Seconds since the daemon last wrote the file, or None when it
    does not say. Clamped at 0: a clock skew must not read as "the
    future", which every ``age > threshold`` test would call FRESH."""
    if not isinstance(status, Mapping):
        return None
    stamp = status.get("generated_at_ms")
    try:
        stamp_ms = float(stamp)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        return None
    return max(0.0, now - stamp_ms / 1000.0)


def status_stale_after_seconds(
    status: Optional[Mapping[str, Any]] = None,
    *,
    poll_seconds: Optional[float] = None,
) -> float:
    """The staleness threshold: four missed polls, floored."""
    if poll_seconds is None and isinstance(status, Mapping):
        try:
            candidate = float(status.get("poll_seconds"))  # type: ignore[arg-type]
        except (TypeError, ValueError):
            candidate = 0.0
        poll_seconds = candidate if candidate > 0 else None
    if poll_seconds is None or poll_seconds <= 0:
        return STATUS_STALE_FLOOR_SECONDS
    return max(
        STATUS_STALE_FLOOR_SECONDS, STATUS_STALE_POLL_MULTIPLIER * float(poll_seconds)
    )


def status_is_stale(
    status: Optional[Mapping[str, Any]],
    *,
    now: float,
    poll_seconds: Optional[float] = None,
) -> bool:
    """Is ``status.json`` too old for its contents to be quoted?

    Fail-safe like ``spool.export_is_stale``: an absent file, or one
    with no timestamp, is STALE — the direction that makes a consumer
    stop asserting rather than start guessing."""
    age = status_age_seconds(status, now=now)
    if age is None:
        return True
    return age > status_stale_after_seconds(status, poll_seconds=poll_seconds)


# ---------------------------------------------------------------------------
# Aggregate
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class PoolHealth:
    """Everything an operator asks in the first minute of an incident,
    gathered once so no two answers can disagree."""

    runtime_root: str
    sidecar_dir_present: bool
    liveness: DaemonLiveness = field(
        default_factory=lambda: DaemonLiveness(state=STATE_UNKNOWN)
    )
    status_present: bool = False
    status_age_seconds: Optional[float] = None
    status_stale: bool = True
    status_stale_after_seconds: float = STATUS_STALE_FLOOR_SECONDS
    poll_seconds: Optional[float] = None
    phase: str = ""
    # The `stopped` block a deliberate exit leaves in `status.json`
    # (reason / run_phase / what it did). Empty for a daemon that is
    # running, and for one that died without taking an exit path — which
    # is the distinction it exists to draw.
    stopped: Dict[str, Any] = field(default_factory=dict)
    pool_size: Optional[int] = None
    # Grunt workspace indices present on disk, per `status.json`. NOT
    # `pool_size`: a lowered `daemon.grunts` leaves the dropped indices'
    # checkouts behind, and they are invisible in every other surface.
    workspaces: Tuple[int, ...] = ()
    in_flight: Tuple[Dict[str, Any], ...] = ()
    attempt_processes: Tuple[Dict[str, Any], ...] = ()
    suspended_until_ms: Optional[int] = None
    suspended_for_seconds: Optional[float] = None
    last_pass_status: str = ""
    last_pass_age_seconds: Optional[float] = None
    stop_sentinel: bool = False
    drain_sentinel: bool = False
    spool_counts: Dict[str, int] = field(default_factory=dict)
    export_present: bool = False
    export_cycle: Optional[int] = None
    export_age_seconds: Optional[float] = None
    spent_without_outcome: Tuple[Dict[str, Any], ...] = ()

    @property
    def sentinel_pending(self) -> bool:
        return self.stop_sentinel or self.drain_sentinel

    def to_dict(self) -> Dict[str, Any]:
        return {
            "runtime_root": self.runtime_root,
            "sidecar_dir_present": self.sidecar_dir_present,
            "liveness": self.liveness.to_dict(),
            "status_present": self.status_present,
            "status_age_seconds": self.status_age_seconds,
            "status_stale": self.status_stale,
            "status_stale_after_seconds": self.status_stale_after_seconds,
            "poll_seconds": self.poll_seconds,
            "phase": self.phase,
            "stopped": dict(self.stopped),
            "pool_size": self.pool_size,
            "workspaces": list(self.workspaces),
            "in_flight": [dict(row) for row in self.in_flight],
            "attempt_processes": [dict(row) for row in self.attempt_processes],
            "suspended_until_ms": self.suspended_until_ms,
            "suspended_for_seconds": self.suspended_for_seconds,
            "last_pass_status": self.last_pass_status,
            "last_pass_age_seconds": self.last_pass_age_seconds,
            "stop_sentinel": self.stop_sentinel,
            "drain_sentinel": self.drain_sentinel,
            "spool_counts": dict(self.spool_counts),
            "export_present": self.export_present,
            "export_cycle": self.export_cycle,
            "export_age_seconds": self.export_age_seconds,
            "spent_without_outcome": [dict(row) for row in self.spent_without_outcome],
        }


SPOOL_LANES = (
    "pending",
    "claimed",
    "applied",
    "rejected",
    "outcomes",
    "claimed_outcomes",
    "outcomes_consumed",
    "inflight",
    "abandoned",
)


def _spool_counts(runtime_root: Path) -> Dict[str, int]:
    spool = Path(runtime_root) / "sidecar" / "spool"
    counts: Dict[str, int] = {}
    for lane in SPOOL_LANES:
        try:
            counts[lane] = sum(
                1 for entry in (spool / lane).iterdir() if entry.name.endswith(".json")
            )
        except OSError:
            counts[lane] = 0
    return counts


def spent_without_outcome(
    runtime_root: Path, export: Optional[Mapping[str, Any]] = None
) -> List[Dict[str, Any]]:
    """Queue entries whose generation the daemon SPENT and whose attempt
    row carries no proof that the kernel was told.

    A row is counted when the newest attempt against its exact
    ``(node, entry_seq)`` generation both (a) should have published —
    every spending status except ``success``, whose closure is on its
    way through ``pending/`` and whose entry must stay live — and (b)
    carries no ``outcome_published`` marker.

    This is a MISSING-MARKER count, not a fault count, and the caller
    must not present it as one. Three situations produce it:

      * the row predates the marker, which is written only from the
        version that introduced it. Such rows never acquire one and read
        this way until the entries cycle out of the queue;
      * the outcome IS published and simply has not been consumed yet —
        the record is sitting in ``outcomes/`` waiting for the kernel's
        next boundary;
      * the publication genuinely failed (the spool write is
        best-effort and swallows its errors), which is the one case
        needing a human.

    ``outcomes/`` being empty is what separates the third from the
    second. REPORT ONLY either way: republishing automatically was
    designed and then cut, because ``sidecar_queue_seq`` lives in
    ``ProtocolState``, so a rewind the daemon did not witness hands the
    same ``entry_seq`` to a DIFFERENT entry and a republished outcome
    would expire a live, never-attempted queue entry."""
    # local: avoid a cycle
    from trellis.sidecar.daemon import assignable_lanes, load_attempted

    if export is None:
        export = spool_mod.read_candidates(Path(runtime_root))
    if not isinstance(export, Mapping):
        return []
    # BOTH lanes: this is an operator diagnostic about the outcome
    # pipeline, and the pipeline is lane-blind.
    queue = assignable_lanes(export)
    attempted = load_attempted(Path(runtime_root))
    out: List[Dict[str, Any]] = []
    for row in queue:
        node = str(row.get("node", ""))
        entry_seq = row.get("entry_seq")
        if not node or not isinstance(entry_seq, int):
            continue
        rows = attempted.get(node)
        if not isinstance(rows, list):
            continue
        matching = [
            r
            for r in rows
            if isinstance(r, Mapping) and r.get("entry_seq") == entry_seq
        ]
        if not matching:
            continue
        newest = matching[-1]
        status = str(newest.get("status", ""))
        if status == "success":
            continue
        if newest.get("outcome_published"):
            continue
        out.append(
            {
                "node": node,
                "entry_seq": entry_seq,
                "status": status,
                "attempt_id": str(newest.get("attempt_id", "")),
            }
        )
    return out


def pool_health(
    runtime_root: Path,
    *,
    now: Optional[float] = None,
    proc_root: Path = DEFAULT_PROC_ROOT,
) -> PoolHealth:
    """Gather every liveness-adjacent fact about one runtime root.

    The ``attempt_processes`` scan is what makes a POST-DRAIN gap
    legible: a drained manager leaves its children running on purpose,
    so "no daemon, three live attempts" is a healthy redeploy window,
    while "no daemon, no attempts" is an outage. Nothing else on the box
    distinguishes those two, and reading them as the same thing is how a
    drain gets escalated and an outage does not."""
    now = time.time() if now is None else now
    root = Path(runtime_root)
    sidecar_dir = root / "sidecar"
    if not sidecar_dir.is_dir():
        return PoolHealth(
            runtime_root=str(root),
            sidecar_dir_present=False,
            liveness=DaemonLiveness(
                state=STATE_NOT_RUNNING,
                detail=f"no sidecar directory at {sidecar_dir}",
            ),
        )

    liveness = probe_daemon(root, proc_root=proc_root)
    status = read_status(root)
    poll_seconds: Optional[float] = None
    if isinstance(status, Mapping):
        try:
            candidate = float(status.get("poll_seconds"))  # type: ignore[arg-type]
            poll_seconds = candidate if candidate > 0 else None
        except (TypeError, ValueError):
            poll_seconds = None

    in_flight = status.get("in_flight") if isinstance(status, Mapping) else None
    in_flight_rows = tuple(
        dict(row) for row in (in_flight or []) if isinstance(row, Mapping)
    )
    pool_size = status.get("grunts") if isinstance(status, Mapping) else None
    pool_size = pool_size if isinstance(pool_size, int) else None

    stopped = status.get("stopped") if isinstance(status, Mapping) else None
    stopped = dict(stopped) if isinstance(stopped, Mapping) else {}
    workspaces = status.get("workspaces") if isinstance(status, Mapping) else None
    workspaces = tuple(
        int(k) for k in workspaces if isinstance(k, int)
    ) if isinstance(workspaces, list) else ()

    suspended_until_ms = (
        status.get("suspended_until_ms") if isinstance(status, Mapping) else None
    )
    try:
        suspended_until_ms = int(suspended_until_ms)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        suspended_until_ms = None
    suspended_for = None
    if suspended_until_ms:
        remaining = suspended_until_ms / 1000.0 - now
        suspended_for = remaining if remaining > 0 else None

    last_pass = status.get("last_pass") if isinstance(status, Mapping) else None
    last_pass = last_pass if isinstance(last_pass, Mapping) else {}
    last_pass_age = None
    try:
        last_pass_age = max(0.0, now - float(last_pass.get("at_ms")) / 1000.0)
    except (TypeError, ValueError):
        last_pass_age = None

    export = spool_mod.read_candidates(root)
    export_cycle = export.get("cycle") if isinstance(export, Mapping) else None
    export_mtime = spool_mod.candidates_mtime(root)

    attempt_procs = tuple(
        {
            "pid": pid,
            "node": identity.node,
            "entry_seq": identity.entry_seq,
            "grunt": identity.grunt,
            "attempt_id": identity.attempt_id,
        }
        for pid, identity in adopt_mod.scan_attempt_processes(
            root, proc_root=Path(proc_root)
        )
    )

    return PoolHealth(
        runtime_root=str(root),
        sidecar_dir_present=True,
        liveness=liveness,
        status_present=status is not None,
        status_age_seconds=status_age_seconds(status, now=now),
        status_stale=status_is_stale(status, now=now),
        status_stale_after_seconds=status_stale_after_seconds(status),
        poll_seconds=poll_seconds,
        phase=str(status.get("phase", "")) if isinstance(status, Mapping) else "",
        stopped=stopped,
        pool_size=pool_size,
        workspaces=workspaces,
        in_flight=in_flight_rows,
        attempt_processes=attempt_procs,
        suspended_until_ms=suspended_until_ms or None,
        suspended_for_seconds=suspended_for,
        last_pass_status=str(last_pass.get("status", "")),
        last_pass_age_seconds=last_pass_age,
        stop_sentinel=(sidecar_dir / "stop").exists(),
        drain_sentinel=(sidecar_dir / "drain").exists(),
        spool_counts=_spool_counts(root),
        export_present=isinstance(export, Mapping),
        export_cycle=export_cycle if isinstance(export_cycle, int) else None,
        export_age_seconds=(
            None if export_mtime is None else max(0.0, now - export_mtime)
        ),
        spent_without_outcome=tuple(spent_without_outcome(root, export)),
    )


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def _age(seconds: Optional[float]) -> str:
    if seconds is None:
        return "unknown"
    if seconds < 90:
        return f"{seconds:.0f}s ago"
    if seconds < 5400:
        return f"{seconds / 60:.0f}m ago"
    return f"{seconds / 3600:.1f}h ago"


def render(health: PoolHealth) -> str:
    lines: List[str] = [f"sidecar {health.runtime_root}"]
    if not health.sidecar_dir_present:
        lines.append("  daemon:   NO SIDECAR DIRECTORY (never started here)")
        return "\n".join(lines)

    live = health.liveness
    if live.state == STATE_RUNNING:
        head = f"RUNNING (pid {live.pid})" if live.pid else "RUNNING"
        if not live.identity_ok:
            head += "  [identity NOT corroborated]"
    elif live.state == STATE_NOT_RUNNING:
        head = "NOT RUNNING"
    else:
        head = "UNKNOWN"
    lines.append(f"  daemon:   {head}")
    if live.detail:
        lines.append(f"            {live.detail}")

    freshness = "stale" if health.status_stale else "fresh"
    if health.status_age_seconds is None:
        when = "no status file, or none with a timestamp"
    else:
        when = f"written {_age(health.status_age_seconds)}"
    lines.append(
        f"  status:   {freshness} — {when} "
        f"(stale after {health.status_stale_after_seconds:.0f}s)"
    )
    if health.phase:
        lines.append(f"            phase: {health.phase}")
    if health.stopped:
        # The one reading that must never be mistaken for an outage: a
        # daemon that is gone BECAUSE IT WAS TOLD TO GO. Without this the
        # only difference from a crash is `last_pass.status`, which reads
        # like weather.
        reason = str(health.stopped.get("reason", "")) or "unspecified"
        detail = f"  ({health.stopped['run_phase']})" if health.stopped.get(
            "run_phase"
        ) else ""
        # A drain is an exit that LEAVES WORK RUNNING; labelling it
        # STOPPED next to the live attempt processes `status` prints
        # would contradict them.
        label = "DRAINED: " if reason == "drain_sentinel" else "STOPPED: "
        lines.append(f"  {label} deliberate exit — {reason}{detail}")
        if reason == "phase_complete":
            released = health.stopped.get("workspaces_released") or []
            lines.append(
                "            the run left the phases the sidecar works in; it "
                "cancelled "
                f"{health.stopped.get('cancelled_attempts', 0)} in-flight "
                f"attempt(s) and released {len(released)} grunt workspace(s). "
                "Start it again if the run re-enters formalization."
            )
    if health.last_pass_status:
        lines.append(
            f"  last pass: {health.last_pass_status} "
            f"({_age(health.last_pass_age_seconds)})"
        )
    if health.suspended_for_seconds:
        lines.append(
            f"  SUSPENDED: no new assignments for another "
            f"{health.suspended_for_seconds / 60:.0f}m (transport failures)"
        )
    pool = "unknown" if health.pool_size is None else str(health.pool_size)
    lines.append(
        f"  pool:     {pool} grunt(s), {len(health.in_flight)} in-flight row(s) "
        f"in status.json"
    )
    if health.pool_size is not None and len(health.workspaces) > health.pool_size:
        # Lowering `daemon.grunts` orphans the dropped indices'
        # checkouts, which are tens of GB each and which nothing else
        # reports. Named here so the disk is not a surprise.
        lines.append(
            f"            {len(health.workspaces)} workspace(s) on disk "
            f"{sorted(health.workspaces)} — more than the configured pool; "
            "the extras are orphans of a lowered `daemon.grunts`"
        )
    for row in health.in_flight:
        lines.append(
            f"            - {row.get('node')} [seq {row.get('entry_seq')}] "
            f"grunt {row.get('grunt')} attempt {row.get('attempt_id')}"
        )
    lines.append(
        f"  processes: {len(health.attempt_processes)} live attempt process(es) "
        "found in /proc"
    )
    for row in health.attempt_processes:
        lines.append(
            f"            - pid {row['pid']} {row['node']} "
            f"[seq {row['entry_seq']}] grunt {row['grunt']}"
        )
    if health.stop_sentinel or health.drain_sentinel:
        names = ", ".join(
            name
            for name, present in (
                ("stop", health.stop_sentinel),
                ("drain", health.drain_sentinel),
            )
            if present
        )
        lines.append(
            f"  SENTINEL: {names} present — the next daemon pass exits. "
            "A sentinel left behind while no daemon runs is cleared at the "
            "next daemon start."
        )
    lines.append(
        "  export:   "
        + (
            f"cycle {health.export_cycle}, written {_age(health.export_age_seconds)}"
            if health.export_present
            else "absent (the kernel has not exported candidates.json)"
        )
    )
    lanes = ", ".join(f"{lane}={health.spool_counts.get(lane, 0)}" for lane in SPOOL_LANES)
    lines.append(f"  spool:    {lanes}")
    if health.spent_without_outcome:
        lines.append(
            f"  spent with no publication marker: "
            f"{len(health.spent_without_outcome)} queue entry(ies)"
        )
        for row in health.spent_without_outcome:
            lines.append(
                f"            - {row['node']} [seq {row['entry_seq']}] "
                f"{row['status']}"
            )
        # Three different situations produce this count, and only the
        # last is a fault. Ordered by how likely each is.
        lines.append(
            "            Expected in two cases: an attempt row written "
            "before the `outcome_published` marker existed (it never gets "
            "one — these clear as the entries cycle out), or a record still "
            f"sitting unconsumed in outcomes/ ({health.spool_counts.get('outcomes', 0)} "
            "there now), which the kernel retires at its next boundary."
        )
        lines.append(
            "            Only if outcomes/ is empty AND the rows are recent "
            "was the kernel never told; then the entry sits in the queue "
            "until the reviewer removes it. Never hand-write a spool record."
        )
    return "\n".join(lines)


def exit_code(health: PoolHealth) -> int:
    if not health.sidecar_dir_present:
        return EXIT_NO_SIDECAR_DIR
    if health.liveness.state == STATE_UNKNOWN:
        return EXIT_UNDETERMINABLE
    if health.liveness.state == STATE_NOT_RUNNING:
        return EXIT_NOT_RUNNING
    if health.status_stale or health.sentinel_pending:
        return EXIT_DEGRADED
    return EXIT_RUNNING


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        prog="python3 -m trellis.sidecar.health",
        description=(
            "Report whether the sidecar daemon over <runtime_root> is alive "
            "and whether its status file may still be quoted."
        ),
        epilog=(
            "exit codes: 0 running+fresh, 1 not running, 2 degraded (stale "
            "status or a pending stop/drain sentinel), 3 no sidecar "
            "directory, 4 undeterminable, 64 usage"
        ),
    )
    parser.add_argument("runtime_root", type=Path)
    parser.add_argument(
        "--json", action="store_true", help="machine-readable output"
    )
    parser.add_argument(
        "--proc-root",
        type=Path,
        default=DEFAULT_PROC_ROOT,
        help=argparse.SUPPRESS,
    )
    try:
        args = parser.parse_args(list(argv) if argv is not None else None)
    except SystemExit as exc:
        return EXIT_USAGE if exc.code else 0
    health = pool_health(args.runtime_root, proc_root=args.proc_root)
    if args.json:
        print(json.dumps(health.to_dict(), indent=2, sort_keys=True))
    else:
        print(render(health))
    return exit_code(health)


if __name__ == "__main__":  # pragma: no cover — process entry
    sys.exit(main())
