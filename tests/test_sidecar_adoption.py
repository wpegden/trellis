"""Drain + adoption tests (§2.2-§2.4): attempts outlive the manager.

Two shapes of process are used, both real in the sense that matters:

  * a REAL child whose argv IS an attempt cmdline — ``python -c
    'sleep(...)' -m trellis.sidecar.attempt <root> --grunt …`` — so the
    REAL ``/proc`` carries a real, adoptable identity, and ``killpg``
    observations are genuine (each child gets its own session, exactly
    like ``trellis/sidecar/__main__.py`` spawns attempts);
  * a SYNTHETIC ``/proc`` tree under ``tmp_path`` (the injectable
    ``proc_root`` seam) for the dead/alive matrix, where fabricating a
    pid is the whole point.

The two sharpest tests here are ``test_adopted_generation_is_never_
reassigned`` (the double-attempt hazard adoption exists to close) and
``test_startup_sweep_spares_a_live_adopted_attempts_inflight_record``
(the lost-closure hazard adoption would OPEN if it shipped without the
sweep exclusion).
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Dict, List

import pytest

from test_sidecar_daemon import Harness, _queue_row, _seed_export

from trellis.sidecar import daemon as daemon_mod
from trellis.sidecar import ledger as ledger_mod
from trellis.sidecar.adopt import (
    AdoptedProc,
    AttemptIdentity,
    attempt_is_alive,
    parse_attempt_cmdline,
    scan_attempt_processes,
)
from trellis.sidecar.daemon import (
    GruntSlot,
    ProcRootUnavailableError,
    SidecarDaemon,
    drain_sentinel_path,
    load_attempted,
    load_slots,
    slots_path,
    status_path,
    stop_sentinel_path,
)
from trellis.sidecar.spool import spool_dirs


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _attempt_argv(
    runtime_root: Path,
    *,
    grunt: int = 0,
    node: str = "A",
    entry_seq: int = 1,
    attempt_id: str = "sc-live",
    result_path: str = "",
    node_file_sha: str = "",
) -> List[str]:
    """The production attempt cmdline (``__main__._make_spawn_fn``)."""
    return [
        sys.executable,
        "-m",
        "trellis.sidecar.attempt",
        str(runtime_root),
        "--grunt",
        str(grunt),
        "--node",
        node,
        "--entry-seq",
        str(entry_seq),
        "--snapshot",
        "abc123",
        "--node-file-sha",
        node_file_sha,
        "--statement-prefix-sha",
        "sp",
        "--attempt-id",
        attempt_id,
        "--result",
        result_path or str(runtime_root / f"result-{attempt_id}.json"),
        "--config",
        str(runtime_root / "trellis.config.json"),
    ]


def _live_attempt_child(runtime_root: Path, **kwargs) -> subprocess.Popen:
    """A real, killable process in its own session whose ``/proc``
    cmdline is a real attempt cmdline. ``python -c CMD ARGS…`` passes
    everything after CMD through as ignored argv, so the process sleeps
    while spelling the identity adoption keys off."""
    argv = _attempt_argv(runtime_root, **kwargs)
    child = argv[:1] + ["-c", "import time; time.sleep(900)"] + argv[1:]
    return subprocess.Popen(child, start_new_session=True)


def _kill(*procs) -> None:
    for proc in procs:
        if proc is None:
            continue
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except (OSError, ProcessLookupError, AttributeError):
            pass


def _synthetic_proc(proc_root: Path, pid: int, argv: List[str]) -> int:
    entry = proc_root / str(pid)
    entry.mkdir(parents=True, exist_ok=True)
    (entry / "cmdline").write_bytes(("\0".join(argv) + "\0").encode("utf-8"))
    return pid


def _journal_row(
    *,
    grunt: int = 0,
    node: str = "A",
    entry_seq: int = 1,
    attempt_id: str = "sc-live",
    pid: int = 4242,
    started_at_ms: int = 111,
    result_path: str = "",
    assigned_at_cycle: int = 10,
) -> Dict[str, Any]:
    return {
        "grunt": grunt,
        "node": node,
        "entry_seq": entry_seq,
        "attempt_id": attempt_id,
        "pid": pid,
        "started_at_ms": started_at_ms,
        "result_path": result_path,
        "assigned_at_cycle": assigned_at_cycle,
    }


def _seed_journal(tmp_path: Path, rows: List[Dict[str, Any]]) -> None:
    path = slots_path(tmp_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(
            {"schema": 1, "written_at": 0.0, "drained_at": None, "slots": rows}
        ),
        encoding="utf-8",
    )


def _ledger_rows(tmp_path: Path) -> List[Dict[str, Any]]:
    path = tmp_path / "sidecar" / "ledger.jsonl"
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line]


def _outcomes(tmp_path: Path) -> List[Dict[str, Any]]:
    dirs = spool_dirs(tmp_path)
    if not dirs.outcomes.is_dir():
        return []
    return [
        json.loads(path.read_text(encoding="utf-8"))
        for path in sorted(dirs.outcomes.glob("outcome-*.json"))
    ]


def _spawn_live(h: Harness, procs: Dict[str, subprocess.Popen], runtime_root: Path):
    """A spawn_fn that starts a REAL adoptable attempt process."""

    def spawn(grunt, row, export, attempt_id) -> GruntSlot:
        result_path = h.results_dir / f"result-{attempt_id}.json"
        proc = _live_attempt_child(
            runtime_root,
            grunt=grunt,
            node=str(row["node"]),
            entry_seq=int(row["entry_seq"]),
            attempt_id=attempt_id,
            result_path=str(result_path),
        )
        procs[attempt_id] = proc
        h.spawned.append(
            {
                "grunt": grunt,
                "node": row["node"],
                "entry_seq": row["entry_seq"],
                "attempt_id": attempt_id,
            }
        )
        return GruntSlot(
            grunt=grunt,
            node=str(row["node"]),
            entry_seq=int(row["entry_seq"]),
            attempt_id=attempt_id,
            started_at_ms=4242,
            proc=proc,
            result_path=result_path,
            assigned_at_cycle=int(export.get("cycle", 0) or 0),
        )

    return spawn


# ---------------------------------------------------------------------------
# 1-3: the drain sentinel
# ---------------------------------------------------------------------------


def test_drain_sentinel_leaves_children_running_and_exits(tmp_path: Path) -> None:
    """Drain records NOTHING: the attempt has not ended, so a ledger
    row, an attempted-set row or a published outcome would each be a
    lie about live work — and the outcome would expire the queue entry,
    making this very child's later closure die ``not_queued``."""
    h = Harness(tmp_path)
    procs: Dict[str, subprocess.Popen] = {}
    h.daemon.spawn_fn = _spawn_live(h, procs, tmp_path)
    _seed_export(tmp_path, [_queue_row("A", 1)])
    try:
        assert h.daemon.run_once() == "assigned"
        proc = procs[h.spawned[0]["attempt_id"]]

        drain_sentinel_path(tmp_path).touch()
        assert h.daemon.run_once() == "drain"

        assert proc.poll() is None, "the child keeps running through a drain"
        assert not any(r.get("status") == "cancelled" for r in _ledger_rows(tmp_path))
        assert _outcomes(tmp_path) == []
        assert load_attempted(tmp_path) == {}
        rows = load_slots(tmp_path)
        assert [(r["node"], r["entry_seq"], r["grunt"]) for r in rows] == [("A", 1, 0)]
        assert rows[0]["pid"] == proc.pid
        journal = json.loads(slots_path(tmp_path).read_text())
        assert journal["drained_at"] is not None
    finally:
        _kill(*procs.values())


def test_hard_stop_beats_drain_when_both_sentinels_present(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    procs: Dict[str, subprocess.Popen] = {}
    h.daemon.spawn_fn = _spawn_live(h, procs, tmp_path)
    _seed_export(tmp_path, [_queue_row("A", 1)])
    try:
        assert h.daemon.run_once() == "assigned"
        proc = procs[h.spawned[0]["attempt_id"]]
        drain_sentinel_path(tmp_path).touch()
        stop_sentinel_path(tmp_path).touch()

        assert h.daemon.run_once() == "stop", "the hard stop wins"
        assert proc.poll() is not None, "stop kills the process group"
        assert any(r.get("status") == "cancelled" for r in _ledger_rows(tmp_path))
        assert load_slots(tmp_path) == []
    finally:
        _kill(*procs.values())


def test_drain_status_json_still_lists_the_in_flight_attempt(tmp_path: Path) -> None:
    h = Harness(tmp_path)
    procs: Dict[str, subprocess.Popen] = {}
    h.daemon.spawn_fn = _spawn_live(h, procs, tmp_path)
    _seed_export(tmp_path, [_queue_row("A", 1)])
    try:
        h.daemon.run_once()
        drain_sentinel_path(tmp_path).touch()
        assert h.daemon.run_once() == "drain"
        status = json.loads(status_path(tmp_path).read_text())
        assert [row["node"] for row in status["in_flight"]] == ["A"]
        assert status["in_flight"][0]["attempt_id"] == h.spawned[0]["attempt_id"]
        # The slot is NOT cleared on the way out — drain leaves the
        # attempt in flight, it does not end it.
        assert [s.node for s in h.daemon.slots.values() if s is not None] == ["A"]
    finally:
        _kill(*procs.values())


# ---------------------------------------------------------------------------
# 4: THE double-attempt hazard
# ---------------------------------------------------------------------------


def test_adopted_generation_is_never_reassigned(tmp_path: Path) -> None:
    """Drain without adoption would be UNSAFE, not merely wasteful: a
    drained-but-unreaped attempt leaves no attempted.json row, so a
    fresh daemon would hand the same (node, entry_seq) to a second
    grunt. Daemon B here sees the same export and spawns NOTHING."""
    h1 = Harness(tmp_path)
    procs: Dict[str, subprocess.Popen] = {}
    h1.daemon.spawn_fn = _spawn_live(h1, procs, tmp_path)
    _seed_export(tmp_path, [_queue_row("A", 1)])
    try:
        assert h1.daemon.run_once() == "assigned"
        attempt_id = h1.spawned[0]["attempt_id"]
        drain_sentinel_path(tmp_path).touch()
        assert h1.daemon.run_once() == "drain"
        drain_sentinel_path(tmp_path).unlink()

        # A brand-new manager over the same runtime root (real /proc).
        h2 = Harness(tmp_path)
        h2.daemon.startup()

        adopted = [s for s in h2.daemon.slots.values() if s is not None]
        assert [s.attempt_id for s in adopted] == [attempt_id]
        assert adopted[0].adopted is True
        assert h2.daemon.run_once() == "idle"
        assert len(h2.spawned) == 0, "the live generation is NEVER re-assigned"
        status = json.loads(status_path(tmp_path).read_text())
        assert status["in_flight"][0]["attempt_id"] == attempt_id
        assert status["in_flight"][0]["adopted"] is True
    finally:
        _kill(*procs.values())


# ---------------------------------------------------------------------------
# 5: THE lost-closure hazard
# ---------------------------------------------------------------------------


def test_startup_sweep_spares_a_live_adopted_attempts_inflight_record(
    tmp_path: Path,
) -> None:
    """``publish_attempt`` writes ``inflight/attempt-<id>.json`` and then
    renames it into ``pending/``. A daemon starting in that window used
    to move the staged record to ``abandoned/``, so the child's
    ``os.replace`` raised and a SUCCESSFUL closure became an ``error``.
    Adoption + the sweep exclusion close it; a dead attempt's leftover
    is still swept."""
    proc_root = tmp_path / "proc"
    live_id, dead_id = "sc-live-1", "sc-dead-1"
    _synthetic_proc(
        proc_root,
        4242,
        _attempt_argv(tmp_path, attempt_id=live_id, node="A", entry_seq=1),
    )
    _seed_journal(
        tmp_path,
        [
            _journal_row(grunt=0, node="A", entry_seq=1, attempt_id=live_id, pid=4242),
            _journal_row(grunt=1, node="B", entry_seq=2, attempt_id=dead_id, pid=4343),
        ],
    )
    h = Harness(tmp_path)
    h.daemon.proc_root = proc_root
    dirs = spool_dirs(tmp_path)
    dirs.inflight.mkdir(parents=True, exist_ok=True)
    live_record = dirs.inflight / f"attempt-{live_id}.json"
    dead_record = dirs.inflight / f"attempt-{dead_id}.json"
    live_record.write_text('{"mid": "publish"}')
    dead_record.write_text('{"crash": "debris"}')

    h.daemon.startup()

    assert live_record.exists(), (
        "a live adopted attempt's mid-publish record must NOT be swept"
    )
    assert not (dirs.abandoned / live_record.name).exists()
    assert not dead_record.exists()
    assert (dirs.abandoned / dead_record.name).exists()


# ---------------------------------------------------------------------------
# 6-9: settling the attempts that ENDED during the gap
# ---------------------------------------------------------------------------


def _dead_journal(tmp_path: Path, *, attempt_id: str, result: Dict[str, Any] | None):
    result_path = tmp_path / f"result-{attempt_id}.json"
    if result is not None:
        result_path.write_text(json.dumps({"attempt_id": attempt_id, **result}))
    _seed_journal(
        tmp_path,
        [
            _journal_row(
                node="A",
                entry_seq=1,
                attempt_id=attempt_id,
                pid=999999,
                result_path=str(result_path),
            )
        ],
    )


def test_orphan_that_finished_during_the_gap_is_reaped_exactly_once(
    tmp_path: Path,
) -> None:
    proc_root = tmp_path / "proc"
    proc_root.mkdir()
    _dead_journal(tmp_path, attempt_id="sc-done", result={"status": "failed",
                                                          "detail": "unsolved goals"})
    h = Harness(tmp_path)
    h.daemon.proc_root = proc_root
    h.daemon.startup()

    assert [r["status"] for r in load_attempted(tmp_path)["A"]] == ["failed"]
    assert len(_ledger_rows(tmp_path)) == 1
    assert load_slots(tmp_path) == [], "the settled row leaves the journal"

    # Re-seed the SAME journal (a daemon that died before its journal
    # rewrite) and start again: the exactly-once guard refuses the re-reap.
    _dead_journal(tmp_path, attempt_id="sc-done", result={"status": "failed",
                                                          "detail": "unsolved goals"})
    h2 = Harness(tmp_path)
    h2.daemon.proc_root = proc_root
    h2.daemon.startup()
    assert len(load_attempted(tmp_path)["A"]) == 1, "exactly once"
    assert len(_ledger_rows(tmp_path)) == 1
    assert len(_outcomes(tmp_path)) == 1, "one spent-generation outcome, not two"


def test_orphan_already_bookkept_is_dropped_silently(tmp_path: Path) -> None:
    """A generation-spending outcome is "already bookkept" exactly when
    its ATTEMPTED row is on disk — that is the write it owns."""
    proc_root = tmp_path / "proc"
    proc_root.mkdir()
    daemon_mod.record_attempted(
        tmp_path,
        "A",
        {"entry_seq": 1, "attempt_id": "sc-known", "status": "failed"},
    )
    ledger_mod.append_ledger_row(
        tmp_path, {"attempt_id": "sc-known", "node": "A", "status": "failed"}
    )
    _dead_journal(tmp_path, attempt_id="sc-known", result={"status": "failed"})
    h = Harness(tmp_path)
    h.daemon.proc_root = proc_root
    h.daemon.startup()

    assert len(load_attempted(tmp_path)["A"]) == 1, "no second bookkeeping"
    assert len(_ledger_rows(tmp_path)) == 1
    assert load_slots(tmp_path) == []


def test_transport_class_orphan_already_ledgered_is_dropped_silently(
    tmp_path: Path,
) -> None:
    """A transport-class outcome writes NO attempted row, so its ledger
    row is the write that proves it was bookkept."""
    proc_root = tmp_path / "proc"
    proc_root.mkdir()
    ledger_mod.append_ledger_row(
        tmp_path, {"attempt_id": "sc-transport", "node": "A", "status": "error"}
    )
    _dead_journal(
        tmp_path, attempt_id="sc-transport", result={"status": "error", "detail": "5xx"}
    )
    h = Harness(tmp_path)
    h.daemon.proc_root = proc_root
    h.daemon.startup()

    assert len(_ledger_rows(tmp_path)) == 1, "not re-reaped"
    assert load_attempted(tmp_path) == {}
    assert h.daemon.state.error_streaks == {}, "no streak bump from a re-read"


def test_ledger_row_alone_never_lets_a_spent_generation_be_reassigned(
    tmp_path: Path,
) -> None:
    """AUDIT F1. `_bookkeep_outcome` used to append the ledger row FIRST,
    so a crash between the two writes left a ledger row with no
    attempted row — and a ledger-keyed guard then dropped the journal
    row as "already bookkept", leaving the generation unspent and
    assignable. The guard now asks for the write this OUTCOME owns, so
    the half-written state is re-bookkept instead of re-attempted."""
    proc_root = tmp_path / "proc"
    proc_root.mkdir()
    ledger_mod.append_ledger_row(
        tmp_path, {"attempt_id": "sc-halfway", "node": "A", "status": "failed"}
    )
    _dead_journal(
        tmp_path,
        attempt_id="sc-halfway",
        result={"status": "failed", "detail": "unsolved goals"},
    )
    h = Harness(tmp_path)
    h.daemon.proc_root = proc_root
    h.daemon.startup()

    assert load_attempted(tmp_path)["A"][0]["entry_seq"] == 1, (
        "the missing attempted row is written at startup"
    )
    assert len(_outcomes(tmp_path)) == 1, "the generation is reported spent"
    _seed_export(tmp_path, [_queue_row("A", 1)])
    assert h.daemon.run_once() == "idle"
    assert h.spawned == [], "the generation is NEVER attempted a second time"


def test_bookkeeping_writes_the_ledger_row_last(tmp_path: Path, monkeypatch) -> None:
    """The ordering AUDIT F1 turns on: nothing may make the ledger row
    exist before the attempted-set row it stands for."""

    def boom(*args, **kwargs):
        raise RuntimeError("crash between the two durable writes")

    h = Harness(tmp_path)
    _seed_export(tmp_path, [_queue_row("A", 1)])
    h.daemon.run_once()
    attempt_id = h.spawned[0]["attempt_id"]
    h.finish(attempt_id, {"status": "failed", "detail": "x"})
    monkeypatch.setattr(daemon_mod, "record_attempted", boom)
    with pytest.raises(RuntimeError):
        h.daemon._reap()
    assert _ledger_rows(tmp_path) == [], "no ledger row without its attempted row"

    # Same ordering on the cancel path.
    other = tmp_path / "cancel"
    other.mkdir()
    h2 = Harness(other)
    _seed_export(other, [_queue_row("B", 2)])
    h2.daemon.run_once()
    _seed_export(other, [])  # the reviewer removes the in-flight entry
    with pytest.raises(RuntimeError):
        h2.daemon.run_once()
    assert _ledger_rows(other) == [], "no cancelled ledger row without its record"


def test_orphan_dead_without_result_does_not_spend_the_generation(
    tmp_path: Path,
) -> None:
    proc_root = tmp_path / "proc"
    proc_root.mkdir()
    _dead_journal(tmp_path, attempt_id="sc-crashed", result=None)
    h = Harness(tmp_path)
    h.daemon.proc_root = proc_root
    h.daemon.startup()

    # Transport-class: ledgered, but the generation stays assignable —
    # identical to today's mid-run crash path.
    assert [r["status"] for r in _ledger_rows(tmp_path)] == ["error"]
    assert load_attempted(tmp_path) == {}
    assert _outcomes(tmp_path) == []
    _seed_export(tmp_path, [_queue_row("A", 1)])
    assert h.daemon.run_once() == "assigned"
    assert h.spawned[0]["node"] == "A" and h.spawned[0]["entry_seq"] == 1


def test_successful_orphan_publishes_no_outcome(tmp_path: Path) -> None:
    proc_root = tmp_path / "proc"
    proc_root.mkdir()
    _dead_journal(tmp_path, attempt_id="sc-win", result={"status": "success",
                                                         "detail": "closed"})
    h = Harness(tmp_path)
    h.daemon.proc_root = proc_root
    h.daemon.startup()

    assert load_attempted(tmp_path)["A"][0]["status"] == "success"
    assert _outcomes(tmp_path) == [], (
        "the closure is in pending/; expiring the entry would make the "
        "kernel reject it not_queued"
    )


# ---------------------------------------------------------------------------
# 10-12: an adopted slot is an ORDINARY busy slot
# ---------------------------------------------------------------------------


def _adopt_live_child(tmp_path: Path, h: Harness, **kwargs) -> subprocess.Popen:
    attempt_id = kwargs.pop("attempt_id", "sc-adopt")
    node = kwargs.pop("node", "A")
    entry_seq = kwargs.pop("entry_seq", 1)
    started_at_ms = kwargs.pop("started_at_ms", 111)
    proc = _live_attempt_child(
        tmp_path, attempt_id=attempt_id, node=node, entry_seq=entry_seq
    )
    _seed_journal(
        tmp_path,
        [
            _journal_row(
                node=node,
                entry_seq=entry_seq,
                attempt_id=attempt_id,
                pid=proc.pid,
                started_at_ms=started_at_ms,
                result_path=str(h.results_dir / f"result-{attempt_id}.json"),
            )
        ],
    )
    return proc


def test_adopted_attempt_whose_entry_left_the_queue_is_hard_cancelled(
    tmp_path: Path,
) -> None:
    h = Harness(tmp_path)
    proc = _adopt_live_child(tmp_path, h)
    try:
        h.daemon.startup()
        assert [s.attempt_id for s in h.daemon.slots.values() if s] == ["sc-adopt"]
        _seed_export(tmp_path, [])  # the reviewer removed the entry
        h.daemon.run_once()

        assert proc.poll() is not None, "the adopted process group is dead"
        assert any(r["status"] == "cancelled" for r in _ledger_rows(tmp_path))
        assert [o["status"] for o in _outcomes(tmp_path)] == ["cancelled"]
        assert all(s is None for s in h.daemon.slots.values())
        assert load_slots(tmp_path) == []
    finally:
        _kill(proc)


def test_rewind_cancels_adopted_attempts_and_publishes_nothing(
    tmp_path: Path,
) -> None:
    h = Harness(tmp_path)
    proc = _adopt_live_child(tmp_path, h)
    try:
        _seed_export(tmp_path, [_queue_row("A", 1)], cycle=40)
        h.daemon.startup()
        h.daemon.run_once()  # cursor := 40
        _seed_export(tmp_path, [_queue_row("A", 1)], cycle=12)  # kernel rewound
        h.daemon.run_once()

        assert proc.poll() is not None
        assert any(r["status"] == "cancelled" for r in _ledger_rows(tmp_path))
        assert _outcomes(tmp_path) == [], "a rewind cancel spends no generation"
        assert load_attempted(tmp_path) == {}
    finally:
        _kill(proc)


def test_adopted_slot_keeps_its_original_started_at_ms(tmp_path: Path) -> None:
    """The per-attempt wall lives INSIDE the child (``run_attempt_codex``), so
    adoption can neither extend nor reset it; the manager's own clock
    must not lie about it either."""
    h = Harness(tmp_path)
    proc = _adopt_live_child(tmp_path, h, started_at_ms=1234567)
    try:
        h.daemon.startup()
        slot = next(s for s in h.daemon.slots.values() if s is not None)
        assert slot.started_at_ms == 1234567
        h.daemon._write_status(h.daemon.now())
        status = json.loads(status_path(tmp_path).read_text())
        assert status["in_flight"][0]["started_at_ms"] == 1234567
        assert load_slots(tmp_path)[0]["started_at_ms"] == 1234567
    finally:
        _kill(proc)


# ---------------------------------------------------------------------------
# 13-14: identity, never a bare pid
# ---------------------------------------------------------------------------


def test_pid_reuse_is_never_adopted_or_signalled(tmp_path: Path) -> None:
    """AUDIT F2. The journal's pid points at a live process that is NOT
    the attempt (the number was recycled). Liveness is an IDENTITY
    PARSE, so the decoy here is the realistic worst case: a process
    whose argv CONTAINS the attempt id — an operator's ``tail -f`` on
    the attempt log — which a substring test would call alive, holding
    the grunt slot forever and aiming the next cancel's killpg at an
    innocent process group."""
    decoy = subprocess.Popen(
        [
            sys.executable,
            "-c",
            "import time; time.sleep(60)",
            "tail",
            "-f",
            str(tmp_path / "attempt-sc-gone.log"),
        ],
        start_new_session=True,
    )
    try:
        argv = ["tail", "-f", str(tmp_path / "attempt-sc-gone.log")]
        assert "sc-gone" in argv[2], "the decoy really does spell the id"
        assert parse_attempt_cmdline(argv) is None
        _seed_journal(
            tmp_path,
            [_journal_row(attempt_id="sc-gone", pid=decoy.pid, result_path="")],
        )
        h = Harness(tmp_path)
        h.daemon.startup()

        assert all(s is None for s in h.daemon.slots.values()), "not adopted"
        assert not attempt_is_alive(decoy.pid, "sc-gone")
        # Settled as a dead crash (transport-class), NOT as a cancel:
        # nothing signalled the decoy.
        assert [r["status"] for r in _ledger_rows(tmp_path)] == ["error"]
        time.sleep(0.2)
        assert decoy.poll() is None, "the innocent pid holder survives"
    finally:
        _kill(decoy)


def test_malformed_journal_row_for_a_live_attempt_falls_back_to_the_scan(
    tmp_path: Path,
) -> None:
    """AUDIT F3. A row malformed in a NON-identity field (here
    ``entry_seq``) cannot be rebuilt into a slot — but its attempt is
    alive, so it must not be treated as resolved: the /proc scan
    reconstructs the identity from the child's own argv. Otherwise the
    grunt index goes back into the free pool and the next attempt's
    ``refresh_to_snapshot`` runs `git reset --hard` over the running
    child's compile tree."""
    proc = _live_attempt_child(
        tmp_path, grunt=0, node="A", entry_seq=1, attempt_id="sc-malformed"
    )
    try:
        row = _journal_row(attempt_id="sc-malformed", pid=proc.pid)
        row["entry_seq"] = "not-an-int"
        _seed_journal(tmp_path, [row])
        h = Harness(tmp_path)
        h.daemon.startup()

        slot = h.daemon.slots[0]
        assert slot is not None and slot.attempt_id == "sc-malformed"
        assert (slot.node, slot.entry_seq) == ("A", 1), "identity from the argv"
        # The journal is rewritten well-formed, so the next daemon needs
        # no scan.
        assert load_slots(tmp_path)[0]["entry_seq"] == 1
        # And the generation is not handed to a second grunt.
        _seed_export(tmp_path, [_queue_row("A", 1)])
        assert h.daemon.run_once() == "idle"
        assert h.spawned == []
    finally:
        _kill(proc)


def test_unjournaled_orphan_is_found_by_the_proc_scan(tmp_path: Path) -> None:
    """The belt: a daemon SIGKILLed between Popen and the journal write
    (and every orphan of a pre-journal daemon) has no row at all."""
    proc = _live_attempt_child(
        tmp_path, grunt=1, node="Z", entry_seq=7, attempt_id="sc-unjournaled"
    )
    try:
        found = scan_attempt_processes(tmp_path)
        assert [(pid, ident.attempt_id) for pid, ident in found] == [
            (proc.pid, "sc-unjournaled")
        ]
        h = Harness(tmp_path)
        h.daemon.startup()
        slot = h.daemon.slots[1]
        assert slot is not None and slot.attempt_id == "sc-unjournaled"
        assert (slot.node, slot.entry_seq) == ("Z", 7)
        assert slot.assigned_at_cycle == 0
        assert slot.adopted is True
        # Journalled by the adoption, so the NEXT daemon needs no scan.
        assert [r["attempt_id"] for r in load_slots(tmp_path)] == ["sc-unjournaled"]
        # And that generation is not handed to a second grunt.
        _seed_export(tmp_path, [_queue_row("Z", 7)])
        assert h.daemon.run_once() == "idle"
        assert h.spawned == []
    finally:
        _kill(proc)


def test_parse_attempt_cmdline_table(tmp_path: Path) -> None:
    good = _attempt_argv(
        tmp_path, grunt=2, node="Node", entry_seq=9, attempt_id="sc-id",
        result_path="/tmp/r.json",
    )
    assert parse_attempt_cmdline(good) == AttemptIdentity(
        runtime_root=str(tmp_path),
        grunt=2,
        node="Node",
        entry_seq=9,
        attempt_id="sc-id",
        result_path="/tmp/r.json",
    )
    # An EMPTY option value is legal and must not shift the pairing.
    empty_sha = _attempt_argv(tmp_path, attempt_id="sc-e", node_file_sha="")
    parsed = parse_attempt_cmdline(empty_sha)
    assert parsed is not None and parsed.attempt_id == "sc-e"
    for bad in (
        [],
        ["python", "-m", "trellis.checker.server", str(tmp_path)],
        # module named but not as `-m`'s argument
        ["python", "grep", "trellis.sidecar.attempt"],
        # missing the fields the manager keys bookkeeping by
        ["python", "-m", "trellis.sidecar.attempt", str(tmp_path), "--node", "A"],
        ["python", "-m", "trellis.sidecar.attempt", "--grunt", "0"],
    ):
        assert parse_attempt_cmdline(bad) is None, bad


def test_zombie_cmdline_reads_dead(tmp_path: Path) -> None:
    """A zombie's cmdline is EMPTY — a just-exited orphan must read dead
    even though ``/proc/<pid>`` still exists."""
    proc_root = tmp_path / "proc"
    (proc_root / "77").mkdir(parents=True)
    (proc_root / "77" / "cmdline").write_bytes(b"")
    assert not attempt_is_alive(77, "sc-x", proc_root=proc_root)
    assert AdoptedProc(77, "sc-x", proc_root=proc_root).poll() is not None
    assert AdoptedProc(77, "sc-x", proc_root=proc_root).pid == -1


# ---------------------------------------------------------------------------
# 15-17: startup ordering, pool size, fail-closed journal
# ---------------------------------------------------------------------------


def test_bootstrap_skipped_for_grunts_with_an_adopted_attempt(
    tmp_path: Path,
) -> None:
    """Hazard (b): ``bootstrap_workspace`` copies oleans and seeds
    sidecars into ``grunts/<k>/repo`` — a SECOND writer into the tree an
    adopted attempt is running ``lake build`` in."""
    proc_root = tmp_path / "proc"
    _synthetic_proc(
        proc_root, 4242, _attempt_argv(tmp_path, grunt=0, attempt_id="sc-busy")
    )
    _seed_journal(tmp_path, [_journal_row(grunt=0, attempt_id="sc-busy", pid=4242)])
    h = Harness(tmp_path)
    h.daemon.proc_root = proc_root
    calls: List[int] = []
    h.daemon.bootstrap_fn = calls.append

    h.daemon.startup()

    assert calls == [1], "only the grunt with no live attempt is bootstrapped"


def test_adopted_grunt_index_beyond_the_configured_pool_does_not_grow_it(
    tmp_path: Path,
) -> None:
    proc_root = tmp_path / "proc"
    _synthetic_proc(
        proc_root, 4242, _attempt_argv(tmp_path, grunt=3, attempt_id="sc-wide")
    )
    _seed_journal(tmp_path, [_journal_row(grunt=3, attempt_id="sc-wide", pid=4242)])
    h = Harness(tmp_path, grunts=1)
    h.daemon.proc_root = proc_root
    h.daemon.startup()

    assert sorted(h.daemon.slots) == [0, 3]
    assert h.daemon.slots[3] is not None
    # The attempt ends: the over-capacity index disappears rather than
    # becoming a free slot (which would silently grow the pool).
    (proc_root / "4242" / "cmdline").write_bytes(b"")
    h.daemon._reap()
    assert sorted(h.daemon.slots) == [0]


def test_journal_write_failure_blocks_new_assignment_but_not_reaping(
    tmp_path: Path,
) -> None:
    h = Harness(tmp_path)
    _seed_export(tmp_path, [_queue_row("A", 1)])
    assert h.daemon.run_once() == "assigned"
    attempt_id = h.spawned[0]["attempt_id"]
    h.finish(attempt_id, {"status": "failed", "detail": "nope"})

    # Park a DIRECTORY where the journal belongs.
    slots_path(tmp_path).unlink()
    slots_path(tmp_path).mkdir()

    _seed_export(tmp_path, [_queue_row("B", 2)])
    assert h.daemon.run_once() == "journal_error"
    # Reaping happened anyway: A's outcome is fully bookkept.
    assert load_attempted(tmp_path)["A"][0]["status"] == "failed"
    assert [r["status"] for r in _ledger_rows(tmp_path)] == ["failed"]
    # Nothing new was spawned — never spawn what you cannot journal.
    assert len(h.spawned) == 1
    assert any("ERROR writing the slot journal" in line for line in h.logs)

    slots_path(tmp_path).rmdir()
    assert h.daemon.run_once() == "assigned"
    assert h.spawned[1]["node"] == "B"


def test_missing_proc_root_with_a_non_empty_journal_refuses_to_start(
    tmp_path: Path,
) -> None:
    """Fail-closed: without liveness the manager cannot tell an
    adoptable child from a dead row, and freeing those slots would hand
    live generations to a second grunt."""
    _seed_journal(tmp_path, [_journal_row(attempt_id="sc-x", pid=4242)])
    h = Harness(tmp_path)
    h.daemon.proc_root = tmp_path / "no-such-proc"
    with pytest.raises(ProcRootUnavailableError):
        h.daemon.startup()
    # An EMPTY journal is not a reason to refuse.
    _seed_journal(tmp_path, [])
    h.daemon.startup()


def test_two_live_processes_on_one_grunt_keep_the_older(tmp_path: Path) -> None:
    """They share ``grunts/<k>/repo`` and would corrupt each other."""
    h = Harness(tmp_path)
    older = _live_attempt_child(tmp_path, grunt=0, node="A", entry_seq=1,
                                attempt_id="sc-older")
    newer = _live_attempt_child(tmp_path, grunt=0, node="B", entry_seq=2,
                                attempt_id="sc-newer")
    try:
        _seed_journal(
            tmp_path,
            [
                _journal_row(grunt=0, node="A", entry_seq=1, attempt_id="sc-older",
                             pid=older.pid, started_at_ms=1000),
                _journal_row(grunt=0, node="B", entry_seq=2, attempt_id="sc-newer",
                             pid=newer.pid, started_at_ms=2000),
            ],
        )
        h.daemon.startup()

        assert h.daemon.slots[0] is not None
        assert h.daemon.slots[0].attempt_id == "sc-older"
        assert older.poll() is None
        assert newer.poll() is not None, "the newer duplicate is hard-cancelled"
        cancelled = [r for r in _ledger_rows(tmp_path) if r["status"] == "cancelled"]
        assert [r["attempt_id"] for r in cancelled] == ["sc-newer"]
        assert cancelled[0]["detail"] == "duplicate grunt occupant"
    finally:
        _kill(older, newer)


# ---------------------------------------------------------------------------
# The destructive stale sentinel
#
# ``run_forever`` unlinks a sentinel only on its way OUT, so one written
# while no daemon is running simply stays on disk. The next daemon then
# runs ``startup()`` FIRST — which adopts every live orphan into its
# slots — and only then reaches the stop branch, which killpgs all of
# them AND records a spent generation for each. A file somebody forgot
# to delete therefore destroys live proof work, and the attempts it
# kills are exactly the long-running ones (the ones that survived the
# previous daemon).
# ---------------------------------------------------------------------------


def test_stale_stop_sentinel_never_kills_an_adopted_orphan(tmp_path: Path) -> None:
    proc = None
    try:
        proc = _live_attempt_child(tmp_path, node="A", entry_seq=1, attempt_id="sc-orphan")
        _seed_journal(
            tmp_path,
            [_journal_row(node="A", entry_seq=1, attempt_id="sc-orphan", pid=proc.pid)],
        )
        # The forgotten file: written when no daemon was running.
        stop_sentinel_path(tmp_path).touch()

        h = Harness(tmp_path)
        _seed_export(tmp_path, [_queue_row("A", 1)])
        # Exactly what run_forever does, in order.
        daemon_mod.clear_startup_sentinels(tmp_path, h.daemon.log)
        h.daemon.startup()
        status = h.daemon.run_once()

        assert proc.poll() is None, "the adopted orphan must still be running"
        assert status != "stop"
        assert not stop_sentinel_path(tmp_path).exists()
        assert load_attempted(tmp_path) == {}, "its generation must not be spent"
        assert _outcomes(tmp_path) == [], "nothing may be published as spent"
        assert not any(
            r.get("status") == "cancelled" for r in _ledger_rows(tmp_path)
        )
        assert [s.attempt_id for s in h.daemon.slots.values() if s] == ["sc-orphan"]
    finally:
        _kill(proc)


def test_a_sentinel_written_while_the_daemon_runs_still_stops_it(
    tmp_path: Path,
) -> None:
    """The clearing is scoped to STARTUP: an operator's stop against a
    running daemon is untouched."""
    h = Harness(tmp_path)
    _seed_export(tmp_path, [_queue_row("A", 1)])
    daemon_mod.clear_startup_sentinels(tmp_path, h.daemon.log)
    h.daemon.startup()
    assert h.daemon.run_once() == "assigned"
    stop_sentinel_path(tmp_path).touch()
    assert h.daemon.run_once() == "stop"


def test_run_forever_clears_the_sentinel_before_startup(
    tmp_path: Path, monkeypatch
) -> None:
    """Ordering is the whole fix: startup() adopts, and the stop branch
    kills what startup() adopted."""
    order: List[str] = []
    h = Harness(tmp_path)
    (tmp_path / "sidecar").mkdir(parents=True, exist_ok=True)
    stop_sentinel_path(tmp_path).touch()
    drain_sentinel_path(tmp_path).touch()

    real_startup = h.daemon.startup

    def _startup() -> None:
        order.append("startup")
        order.append(
            "stop-present" if stop_sentinel_path(tmp_path).exists() else "stop-cleared"
        )
        order.append(
            "drain-present"
            if drain_sentinel_path(tmp_path).exists()
            else "drain-cleared"
        )
        real_startup()

    monkeypatch.setattr(h.daemon, "startup", _startup)

    class _Stop(Exception):
        pass

    def _sleep(_seconds):
        raise _Stop()

    monkeypatch.setattr(daemon_mod.time, "sleep", _sleep)
    with pytest.raises(_Stop):
        h.daemon.run_forever()
    assert order == ["startup", "stop-cleared", "drain-cleared"]


def test_a_sentinel_that_cannot_be_removed_is_logged_loudly(
    tmp_path: Path, monkeypatch
) -> None:
    """A failed unlink leaves the sentinel ARMED, so the destructive
    path is still live. Reporting it the same way as "there was no
    sentinel" (a bare `continue`) is what makes that silent."""
    logs: List[str] = []
    (tmp_path / "sidecar").mkdir(parents=True, exist_ok=True)
    stop_sentinel_path(tmp_path).touch()

    real_unlink = Path.unlink

    def _unlink(self, *args, **kwargs):
        if self.name == "stop":
            raise PermissionError(13, "Permission denied")
        return real_unlink(self, *args, **kwargs)

    monkeypatch.setattr(Path, "unlink", _unlink)
    cleared = daemon_mod.clear_startup_sentinels(tmp_path, logs.append)

    assert cleared == [], "a sentinel that is still there was not cleared"
    assert any("ERROR removing" in line and "STILL ARMED" in line for line in logs), (
        f"the failure must be logged at error level, got {logs}"
    )


def test_no_sentinel_present_is_silent(tmp_path: Path) -> None:
    """The ordinary case stays quiet — the error log above must mean
    something when it appears."""
    logs: List[str] = []
    (tmp_path / "sidecar").mkdir(parents=True, exist_ok=True)
    assert daemon_mod.clear_startup_sentinels(tmp_path, logs.append) == []
    assert logs == []


def test_crash_between_attempted_and_publish_is_re_reaped(tmp_path: Path) -> None:
    """AUDIT P1. `record_attempted` makes three durable writes —
    `attempted.json`, `spool.publish_outcome`, then `attempted.json`
    again to stamp `outcome_published` — and the old guard keyed on the
    FIRST. A crash between writes one and two therefore left a row that
    reads "already bookkept" while the kernel had never been told the
    generation was spent: the next daemon dropped the row, no outcome
    was ever published, and the queue entry sat SPENT forever with no
    automatic recovery.

    This pins that exact half-written state: attempted row present,
    outcomes/ empty, ledger row absent (it is written LAST of all)."""
    proc_root = tmp_path / "proc"
    proc_root.mkdir()
    # Write ONE of record_attempted's three writes, exactly as a crash
    # after the first would leave it.
    daemon_mod._write_json_atomic(
        daemon_mod.attempted_path(tmp_path),
        {"A": [{"entry_seq": 1, "attempt_id": "sc-strand", "status": "failed"}]},
    )
    assert _outcomes(tmp_path) == [], "the fixture is mid-strand"
    assert _ledger_rows(tmp_path) == []

    _dead_journal(
        tmp_path,
        attempt_id="sc-strand",
        result={"status": "failed", "detail": "unsolved goals"},
    )
    h = Harness(tmp_path)
    h.daemon.proc_root = proc_root
    h.daemon.startup()

    # RECOVERED: the outcome the kernel was waiting for is published, so
    # the entry can leave the queue instead of sitting SPENT forever.
    outcomes = _outcomes(tmp_path)
    assert len(outcomes) == 1, "the strand is re-reaped, not dropped"
    assert (outcomes[0]["node"], outcomes[0]["entry_seq"]) == ("A", 1)
    assert len(_ledger_rows(tmp_path)) == 1

    # And the generation is still spent exactly once: never re-attempted.
    _seed_export(tmp_path, [_queue_row("A", 1)])
    assert h.daemon.run_once() == "idle"
    assert h.spawned == []


def test_fully_bookkept_row_is_still_dropped(tmp_path: Path) -> None:
    """P1's other side: when ALL of `record_attempted`'s writes landed
    AND the ledger row (written last) is there, the row is dropped — no
    duplicate outcome, no duplicate ledger row."""
    proc_root = tmp_path / "proc"
    proc_root.mkdir()
    daemon_mod.record_attempted(
        tmp_path,
        "A",
        {"entry_seq": 1, "attempt_id": "sc-complete", "status": "failed"},
        publish=True,
    )
    ledger_mod.append_ledger_row(
        tmp_path, {"attempt_id": "sc-complete", "node": "A", "status": "failed"}
    )
    assert len(_outcomes(tmp_path)) == 1

    _dead_journal(
        tmp_path,
        attempt_id="sc-complete",
        result={"status": "failed", "detail": "unsolved goals"},
    )
    h = Harness(tmp_path)
    h.daemon.proc_root = proc_root
    h.daemon.startup()

    assert len(_outcomes(tmp_path)) == 1, "not re-published"
    assert len(_ledger_rows(tmp_path)) == 1, "not double-counted"
    assert len(load_attempted(tmp_path)["A"]) == 1, "no duplicate attempt row"
