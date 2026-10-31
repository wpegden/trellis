"""Memory budget for a managed process tree.

A Lean elaboration has a wall budget but, before this module, no memory
budget. One `lake env lean` of a heavy node peaked at 43 GB in 65 s on a
59 GB box (2026-09-27, `NumberHelperAcceptance__Refutation`), and three
such checks running concurrently drove available memory to 2 GB — the
pressure that stalls tmux and OOM-kills whatever is largest.

`MemoryGuard.check` sums the resident memory of every descendant of the
root process (read from /proc, so it sees through bwrap's pid namespace:
the host still lists those processes) and, when the tree exceeds the cap
or the machine's available memory falls under the floor, reports a
reason. The caller kills the whole tree exactly as it would on a wall
timeout. Killing only the largest child was considered and rejected: the
agent would rerun the same check and blow up again.

Originally written for the retired closure sidecar; the checker's managed
compiler admission is its remaining consumer.
"""

from __future__ import annotations

import os
import signal
import subprocess
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Optional

GIB = 1024 ** 3
_PAGE = os.sysconf("SC_PAGE_SIZE") if hasattr(os, "sysconf") else 4096


class AttemptBudgetExhausted(RuntimeError):
    """A harness build exhausted its budget before the agent could run."""

    def __init__(self, reason: str, *, peak_bytes: int = 0) -> None:
        super().__init__(reason)
        self.peak_bytes = peak_bytes


def _read_proc_table(proc_root: Path = Path("/proc")) -> dict[int, tuple[int, int, str]]:
    """pid -> (ppid, rss_bytes, comm) for every process /proc lists."""
    table: dict[int, tuple[int, int, str]] = {}
    try:
        entries = os.listdir(proc_root)
    except OSError:
        return table
    # This scan runs once per compiler memory poll. Keep the same two fresh
    # proc reads without constructing several Path objects for every PID.
    proc_path = str(proc_root)
    for name in entries:
        if not name.isdigit():
            continue
        pid = int(name)
        try:
            process_path = os.path.join(proc_path, name)
            with open(os.path.join(process_path, "stat")) as stream:
                stat = stream.read()
            with open(os.path.join(process_path, "statm")) as stream:
                statm = stream.read()
        except OSError:
            continue
        # `comm` may contain spaces/parens; it ends at the last ')'.
        close = stat.rfind(")")
        comm = stat[stat.find("(") + 1:close]
        fields = stat[close + 2:].split()
        try:
            ppid = int(fields[1])
            rss = int(statm.split()[1]) * _PAGE
        except (IndexError, ValueError):
            continue
        table[pid] = (ppid, rss, comm)
    return table


class ProcessSnapshot:
    """Share a recent host process scan between concurrent compiler guards."""
    def __init__(self, interval=.1, clock=time.monotonic):
        self.interval, self.clock = interval, clock
        self.lock = threading.Lock()
        self.sample = None

    def __call__(self, proc_root):
        with self.lock:
            now = self.clock()
            if self.sample is None or self.sample[0] != proc_root or now - self.sample[1] >= self.interval:
                self.sample = (proc_root, now, _read_proc_table(proc_root))
            return self.sample[2]


def descendant_rss(root_pid: int, proc_root: Path = Path("/proc"), *, table=None) -> tuple[int, list[tuple[int, int, str]]]:
    """Total resident bytes of ``root_pid`` and all its descendants, plus the
    per-process rows ``(pid, rss_bytes, comm)`` sorted largest first."""
    table = _read_proc_table(proc_root) if table is None else table
    children: dict[int, list[int]] = {}
    for pid, (ppid, _rss, _comm) in table.items():
        children.setdefault(ppid, []).append(pid)
    rows: list[tuple[int, int, str]] = []
    stack = [root_pid]
    seen: set[int] = set()
    while stack:
        pid = stack.pop()
        if pid in seen or pid not in table:
            continue
        seen.add(pid)
        _ppid, rss, comm = table[pid]
        rows.append((pid, rss, comm))
        stack.extend(children.get(pid, ()))
    rows.sort(key=lambda r: r[1], reverse=True)
    return sum(r[1] for r in rows), rows


def system_available_bytes(meminfo: Path = Path("/proc/meminfo")) -> Optional[int]:
    try:
        for line in meminfo.read_text().splitlines():
            if line.startswith("MemAvailable:"):
                return int(line.split()[1]) * 1024
    except (OSError, ValueError, IndexError):
        pass
    return None


def _gib(n: int) -> str:
    return f"{n / GIB:.1f} GiB"


@dataclass
class MemoryGuard:
    """``limit_bytes`` caps the attempt tree; ``floor_bytes`` is the
    machine-wide available-memory floor. Either at 0 disables that check.

    The floor only ends an attempt whose own tree holds at least
    ``floor_min_tree_bytes`` (a quarter of the cap by default): a small
    attempt is not why the machine is short, and killing it leaves the
    real consumer running — observed on the first live hour, where a
    1.3 GiB attempt died for a sibling's 22 GiB elaboration."""

    limit_bytes: int
    floor_bytes: int = 0
    floor_min_tree_bytes: Optional[int] = None
    proc_root: Path = Path("/proc")
    meminfo: Path = Path("/proc/meminfo")
    peak_bytes: int = 0
    rows_at_peak: list = field(default_factory=list)
    process_snapshot: Optional[Callable] = None

    def __post_init__(self) -> None:
        if self.floor_min_tree_bytes is None:
            self.floor_min_tree_bytes = self.limit_bytes // 4 if self.limit_bytes else 2 * GIB

    @classmethod
    def from_gib(cls, limit_gib: float, floor_gib: float = 0.0) -> "MemoryGuard":
        return cls(limit_bytes=int(max(0.0, limit_gib) * GIB), floor_bytes=int(max(0.0, floor_gib) * GIB))

    def check(self, root_pid: int) -> Optional[str]:
        """A reason string when the attempt must be killed, else ``None``."""
        total, rows = (descendant_rss(root_pid, self.proc_root,
                      table=self.process_snapshot(self.proc_root)) if self.process_snapshot
                      else descendant_rss(root_pid, self.proc_root))
        if total > self.peak_bytes:
            self.peak_bytes, self.rows_at_peak = total, rows[:3]
        largest = f"{rows[0][2]} pid {rows[0][0]} at {_gib(rows[0][1])}" if rows else "no live descendant"
        if self.limit_bytes and total > self.limit_bytes:
            return (
                f"memory budget: attempt tree at {_gib(total)} exceeds the "
                f"{_gib(self.limit_bytes)} cap (largest: {largest})"
            )
        if self.floor_bytes and total >= (self.floor_min_tree_bytes or 0):
            available = system_available_bytes(self.meminfo)
            if available is not None and available < self.floor_bytes:
                return (
                    f"memory budget: machine has {_gib(available)} available, under the "
                    f"{_gib(self.floor_bytes)} floor, while this attempt holds {_gib(total)} "
                    f"(largest: {largest})"
                )
        return None


def kill_process_group(proc: subprocess.Popen, grace_seconds: float = 20.0) -> None:
    """TERM the group, wait, then KILL — the shape every managed kill uses."""
    try:
        os.killpg(proc.pid, signal.SIGTERM)
        proc.wait(timeout=grace_seconds)
    except Exception:  # noqa: BLE001 — fall through to the hard kill
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except Exception:  # noqa: BLE001 — already gone
            pass
        try:
            proc.wait(timeout=grace_seconds)
        except Exception:  # noqa: BLE001 — best effort
            pass


def kill_process_tree(proc: subprocess.Popen) -> None:
    """Hard-stop a compiler subtree without signalling the attempt runner.

    Harness compilers stay in the manager's process group so cancellation
    still reaches them. Their budget kill must therefore target individual
    descendants, never ``killpg`` (which would kill the runner as well).
    Freeze the subtree before killing it so new forks or reparenting cannot
    hide a child between discovery and teardown.
    """
    if proc.poll() is not None:
        return
    frozen = set()
    pending = {proc.pid}
    while pending:
        for pid in pending:
            try:
                os.kill(pid, signal.SIGSTOP)
            except ProcessLookupError:
                pass
        frozen.update(pending)
        _total, rows = descendant_rss(proc.pid)
        pending = {pid for pid, _rss, _comm in rows} - frozen
    for pid in frozen - {proc.pid}:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    try:
        proc.kill()
    except ProcessLookupError:
        pass
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass


def wait_with_guard(
    proc: subprocess.Popen,
    *,
    wall_seconds: float,
    guard: Optional[MemoryGuard],
    poll_seconds: float = 2.0,
    now: Callable[[], float] = time.monotonic,
    log: Callable[[str], None] = lambda _m: None,
    kill: Callable[[subprocess.Popen], None] = kill_process_group,
) -> tuple[Optional[str], Optional[int], Optional[str]]:
    """Wait for ``proc`` until it
    exits, the wall passes, or the guard trips. Returns ``(budget, rc,
    reason)``: ``budget`` is ``None`` on a normal exit (``rc`` set), or
    ``"wall"`` / ``"memory"`` when the tree was killed (``rc`` is then
    ``None`` and ``reason`` says why). The default kill requires a private
    process group; in-group harness compilers pass ``kill_process_tree``."""
    deadline = now() + wall_seconds
    while True:
        rc = proc.poll()
        if rc is not None:
            return None, rc, None
        if now() >= deadline:
            kill(proc)
            return "wall", None, "wall budget"
        if guard is not None:
            reason = guard.check(proc.pid)
            if reason:
                log(reason)
                kill(proc)
                return "memory", None, reason
        try:
            proc.wait(timeout=min(poll_seconds, max(0.0, deadline - now())))
        except subprocess.TimeoutExpired:
            continue
