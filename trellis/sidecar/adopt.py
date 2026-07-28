"""Attempt-process liveness + adoption primitives (§2.3).

A manager restart must not lose the attempts that were in flight when
the previous daemon exited. Everything here answers exactly two
questions about a pid the CURRENT process did not fork:

  * is that pid STILL the attempt we think it is (``attempt_is_alive``)?
  * what attempt is a given attempt process running
    (``parse_attempt_cmdline`` / ``scan_attempt_processes``)?

Identity, not liveness, is the load-bearing predicate. ``/proc/<pid>``
existing says only that SOME process holds that pid; after a reboot or
a long enough gap the number is somebody else's. So every check reads
``/proc/<pid>/cmdline`` and requires the ATTEMPT ID to appear in it —
which no unrelated process can spell — before the pid is adopted or
signalled. A zombie's cmdline reads EMPTY, so a just-exited attempt
correctly reads dead even though its ``/proc`` entry still exists.

``AdoptedProc`` wraps such a pid in the Popen-alike surface the manager
already speaks (``.poll()``, ``.wait()``, ``.pid``, ``.terminate()``,
``.kill()``), so an adopted attempt is an ORDINARY busy slot: the
cancel-watch, the rewind cancel, the reaper and the status surface all
work on it unchanged. Every attempt is spawned ``start_new_session=True``
(``trellis/sidecar/__main__.py``), so pgid == pid and a single
``os.killpg`` takes the whole attempt down, warm lean server included.

``proc_root`` is injectable everywhere so the whole feature is testable
against a synthetic /proc tree.
"""

from __future__ import annotations

import os
import signal
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, List, Optional, Sequence, Tuple


ATTEMPT_MODULE = "trellis.sidecar.attempt"
DEFAULT_PROC_ROOT = Path("/proc")


@dataclass(frozen=True)
class AttemptIdentity:
    """What an attempt process's own argv says it is running."""

    runtime_root: str
    grunt: int
    node: str
    entry_seq: int
    attempt_id: str
    result_path: str


def parse_attempt_cmdline(argv: Sequence[str]) -> Optional[AttemptIdentity]:
    """Recognise ``python -m trellis.sidecar.attempt <runtime_root> …``.

    Pure: no filesystem, no process access. Returns None for anything
    that is not an attempt runner or that lacks a field the manager
    needs to key bookkeeping by."""
    argv = list(argv)
    try:
        module_at = argv.index(ATTEMPT_MODULE)
    except ValueError:
        return None
    if module_at == 0 or argv[module_at - 1] != "-m":
        return None
    rest = argv[module_at + 1 :]
    if not rest or rest[0].startswith("-"):
        return None
    runtime_root = rest[0]
    # Positional pairing over the raw argv (never a dict-of-defaults):
    # an EMPTY option value is legal (``--node-file-sha ''``) and must
    # not shift the pairing, so interior empty strings are preserved.
    flags = {}
    index = 1
    while index < len(rest):
        token = rest[index]
        if token.startswith("--") and index + 1 < len(rest):
            flags[token] = rest[index + 1]
            index += 2
        else:
            index += 1
    attempt_id = flags.get("--attempt-id", "")
    node = flags.get("--node", "")
    if not attempt_id or not node:
        return None
    try:
        grunt = int(flags["--grunt"])
        entry_seq = int(flags["--entry-seq"])
    except (KeyError, TypeError, ValueError):
        return None
    return AttemptIdentity(
        runtime_root=runtime_root,
        grunt=grunt,
        node=node,
        entry_seq=entry_seq,
        attempt_id=attempt_id,
        result_path=flags.get("--result", ""),
    )


def read_cmdline(
    pid: int, *, proc_root: Path = DEFAULT_PROC_ROOT
) -> Optional[List[str]]:
    """``/proc/<pid>/cmdline`` as argv, or None when the process is gone
    or is a ZOMBIE (empty cmdline — the reaped-but-not-waited state a
    just-exited orphan passes through)."""
    try:
        raw = (Path(proc_root) / str(pid) / "cmdline").read_bytes()
    except (OSError, ValueError):
        return None
    if not raw:
        return None
    parts = raw.decode("utf-8", "replace").split("\0")
    while parts and parts[-1] == "":
        parts.pop()
    return parts or None


def attempt_is_alive(
    pid: Optional[int],
    attempt_id: str,
    *,
    proc_root: Path = DEFAULT_PROC_ROOT,
) -> bool:
    """Is ``pid`` STILL running attempt ``attempt_id``?

    THE gate in front of every killpg and every adoption, so it is an
    IDENTITY PARSE, never a substring scan: the process must itself be
    an attempt runner (``parse_attempt_cmdline`` recognises the
    ``-m trellis.sidecar.attempt`` form) AND must name exactly this
    attempt id. A substring test over the raw argv says ALIVE for an
    operator's ``tail -f …/attempt-<id>.log``, which on exact pid reuse
    would hold a grunt slot that never reaps and aim a ``killpg`` at an
    innocent process group."""
    if not attempt_id:
        return False
    try:
        pid_int = int(pid)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        return False
    if pid_int <= 0:
        return False
    argv = read_cmdline(pid_int, proc_root=proc_root)
    if not argv:
        return False
    identity = parse_attempt_cmdline(argv)
    return identity is not None and identity.attempt_id == attempt_id


def proc_started_at_ms(
    pid: int, *, proc_root: Path = DEFAULT_PROC_ROOT
) -> int:
    """Best-effort start time for a process we did not fork (the
    ``/proc/<pid>`` directory's ctime). Used only for orphans with no
    journal row — an adopted journal row keeps its ORIGINAL
    ``started_at_ms``, never a re-stamped one."""
    try:
        return int(Path(proc_root).joinpath(str(pid)).stat().st_ctime * 1000)
    except OSError:
        return 0


def scan_attempt_processes(
    runtime_root: Path, *, proc_root: Path = DEFAULT_PROC_ROOT
) -> List[Tuple[int, AttemptIdentity]]:
    """Every live attempt process belonging to ``runtime_root``.

    The BELT under the journal: it finds orphans with no journal row at
    all — a daemon SIGKILLed between ``Popen`` and the journal write,
    and (the migration path) every orphan left by a pre-journal
    daemon."""
    out: List[Tuple[int, AttemptIdentity]] = []
    root = Path(proc_root)
    if not root.is_dir():
        return out
    want = os.path.realpath(str(runtime_root))
    try:
        entries = sorted(root.iterdir(), key=lambda p: p.name)
    except OSError:
        return out
    for entry in entries:
        if not entry.name.isdigit():
            continue
        argv = read_cmdline(int(entry.name), proc_root=root)
        if not argv:
            continue
        identity = parse_attempt_cmdline(argv)
        if identity is None:
            continue
        if os.path.realpath(identity.runtime_root) != want:
            continue
        out.append((int(entry.name), identity))
    return out


class AdoptedProc:
    """Popen-alike over a pid this process did NOT fork.

    ``.pid`` reports -1 once the attempt is no longer alive, so
    ``SidecarDaemon._kill_group``'s ``os.getpgid(proc.pid)`` raises and
    its terminate()/kill() fallback engages — into the methods below,
    which re-check identity and no-op. There is therefore no path from
    an adopted slot to a signal aimed at a pid that is not still running
    that attempt."""

    def __init__(
        self,
        pid: int,
        attempt_id: str,
        *,
        proc_root: Path = DEFAULT_PROC_ROOT,
    ) -> None:
        self._pid = int(pid)
        self.attempt_id = str(attempt_id)
        self.proc_root = Path(proc_root)

    # -- identity ----------------------------------------------------------

    def alive(self) -> bool:
        return attempt_is_alive(
            self._pid, self.attempt_id, proc_root=self.proc_root
        )

    @property
    def pid(self) -> int:
        return self._pid if self.alive() else -1

    # -- Popen surface -----------------------------------------------------

    def poll(self) -> Optional[int]:
        # An adopted child is not ours to wait(2) on, so there is no exit
        # status to report — -1 stands for "ended, status unknown". The
        # manager only ever tests `is None`.
        return None if self.alive() else -1

    def wait(self, timeout: Optional[float] = None) -> int:
        deadline = None if timeout is None else time.monotonic() + timeout
        while self.alive():
            if deadline is not None and time.monotonic() >= deadline:
                break
            time.sleep(0.05)
        return -1

    def terminate(self) -> None:
        self._signal(signal.SIGTERM)

    def kill(self) -> None:
        self._signal(signal.SIGKILL)

    def _signal(self, sig: int) -> None:
        # Re-check identity at signal time: never signal a pid that is
        # not, right now, running this attempt.
        if not self.alive():
            return
        try:
            # pgid == pid: every attempt is spawned start_new_session=True.
            os.killpg(self._pid, sig)
        except OSError:
            pass

    def __repr__(self) -> str:  # pragma: no cover — diagnostics only
        return f"AdoptedProc(pid={self._pid}, attempt_id={self.attempt_id!r})"


def adopted_proc_for(
    pid: int, attempt_id: str, *, proc_root: Any = DEFAULT_PROC_ROOT
) -> AdoptedProc:
    return AdoptedProc(pid, attempt_id, proc_root=Path(proc_root))
