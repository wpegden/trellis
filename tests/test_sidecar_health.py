"""Sidecar liveness/health surface (``trellis.sidecar.health``).

The rules under test are the ones seven production incidents were
caused by getting wrong:

  * a manager is alive iff it holds its own pid lock — not iff
    ``pgrep -f trellis.sidecar`` matches something (that ERE matches
    every ``.attempt`` CHILD and ``trellis_sidecar.sh`` besides);
  * ``status.json``'s CONTENTS may be quoted only while the file is
    fresh;
  * a probe must not change what it measures: no ``O_CREAT``, no
    truncation, no lock left held.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import List

import pytest

from trellis.sidecar import health as health_mod
from trellis.sidecar.daemon import acquire_pid_lock
from trellis.sidecar.health import (
    EXIT_DEGRADED,
    EXIT_NOT_RUNNING,
    EXIT_NO_SIDECAR_DIR,
    EXIT_RUNNING,
    STATE_NOT_RUNNING,
    STATE_RUNNING,
    STATE_UNKNOWN,
    STATUS_STALE_FLOOR_SECONDS,
    DaemonLiveness,
    main,
    parse_daemon_cmdline,
    pool_health,
    probe_daemon,
    read_status,
    spent_without_outcome,
    status_age_seconds,
    status_is_stale,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _daemon_argv(runtime_root: Path) -> List[str]:
    return [
        sys.executable,
        "-m",
        "trellis.sidecar",
        str(runtime_root),
        "--repo",
        str(runtime_root / "repo"),
    ]


def _attempt_argv(runtime_root: Path, attempt_id: str = "sc-a") -> List[str]:
    return [
        sys.executable,
        "-m",
        "trellis.sidecar.attempt",
        str(runtime_root),
        "--grunt",
        "0",
        "--node",
        "Rung",
        "--entry-seq",
        "4",
        "--attempt-id",
        attempt_id,
        "--result",
        str(runtime_root / "r.json"),
    ]


def _fake_proc(root: Path, pid: int, argv: List[str]) -> Path:
    entry = root / str(pid)
    entry.mkdir(parents=True, exist_ok=True)
    (entry / "cmdline").write_bytes(("\0".join(argv) + "\0").encode("utf-8"))
    return root


def _sidecar(tmp_path: Path) -> Path:
    d = tmp_path / "sidecar"
    d.mkdir(parents=True, exist_ok=True)
    return d


def _write_status(tmp_path: Path, **fields) -> None:
    _sidecar(tmp_path)
    (tmp_path / "sidecar" / "status.json").write_text(json.dumps(fields))


# ---------------------------------------------------------------------------
# Identity parsing — exactness is the whole point
# ---------------------------------------------------------------------------


def test_parse_daemon_cmdline_table(tmp_path: Path) -> None:
    assert parse_daemon_cmdline(_daemon_argv(tmp_path)) == str(tmp_path)
    # The attempt CHILD. `trellis.sidecar.attempt` starts with
    # `trellis.sidecar`; a prefix match here makes a dead manager with
    # live children read UP, which is how a 15-minute outage went
    # unnoticed.
    assert parse_daemon_cmdline(_attempt_argv(tmp_path)) is None
    # Not launched with -m.
    assert parse_daemon_cmdline(["python3", "trellis.sidecar", "/rt"]) is None
    # No positional runtime root.
    assert parse_daemon_cmdline(["python3", "-m", "trellis.sidecar"]) is None
    assert parse_daemon_cmdline(["python3", "-m", "trellis.sidecar", "--help"]) is None
    # An operator's shell wrapper is not the daemon either.
    assert parse_daemon_cmdline(["bash", "scripts/trellis_sidecar.sh", "/rt"]) is None


# ---------------------------------------------------------------------------
# probe_daemon
# ---------------------------------------------------------------------------


def test_probe_with_no_pid_file_is_not_running(tmp_path: Path) -> None:
    _sidecar(tmp_path)
    live = probe_daemon(tmp_path)
    assert live.state == STATE_NOT_RUNNING
    assert live.pid_file_present is False


def test_probe_never_creates_the_pid_file(tmp_path: Path) -> None:
    """O_CREAT would make the probe invent the state it reports, and
    leave a file the next daemon's lock semantics depend on."""
    _sidecar(tmp_path)
    probe_daemon(tmp_path)
    assert not (tmp_path / "sidecar" / "daemon.pid").exists()


def test_a_held_pid_lock_reads_running_and_the_pid_survives(tmp_path: Path) -> None:
    """flock is held by the LIVE process and released by the kernel
    however it dies, so there is no stale-pid-file failure mode. The
    probe opens O_RDONLY, so it also cannot truncate the pid away."""
    fd = acquire_pid_lock(tmp_path)
    try:
        live = probe_daemon(tmp_path)
        assert live.state == STATE_RUNNING
        assert live.pid == os.getpid()
        assert live.pid_file_present is True
        recorded = (tmp_path / "sidecar" / "daemon.pid").read_text().strip()
        assert recorded == str(os.getpid()), "the probe must not truncate the pid"
    finally:
        os.close(fd)


def test_a_free_pid_lock_reads_not_running_even_with_a_stale_pid(
    tmp_path: Path,
) -> None:
    fd = acquire_pid_lock(tmp_path)
    os.close(fd)  # the "daemon" exited; the kernel dropped the lock
    live = probe_daemon(tmp_path)
    assert live.state == STATE_NOT_RUNNING
    assert live.pid == os.getpid(), "the file still names the dead pid"
    assert "stale" in live.detail


def test_the_probe_releases_its_shared_lock(tmp_path: Path) -> None:
    """A lock left held by the probe makes a daemon starting one
    millisecond later fail acquire_pid_lock with "already running"."""
    fd = acquire_pid_lock(tmp_path)
    os.close(fd)
    probe_daemon(tmp_path)
    probe_daemon(tmp_path)
    # Still acquirable: nothing the probe did holds it.
    fd2 = acquire_pid_lock(tmp_path)
    os.close(fd2)


def test_identity_is_corroborated_against_proc(tmp_path: Path) -> None:
    fd = acquire_pid_lock(tmp_path)
    try:
        proc_root = _fake_proc(tmp_path / "proc", os.getpid(), _daemon_argv(tmp_path))
        live = probe_daemon(tmp_path, proc_root=proc_root)
        assert live.state == STATE_RUNNING
        assert live.identity_ok is True
        assert live.argv is not None and "-m" in live.argv
    finally:
        os.close(fd)


def test_a_lock_holder_that_is_not_the_daemon_is_flagged(tmp_path: Path) -> None:
    fd = acquire_pid_lock(tmp_path)
    try:
        proc_root = _fake_proc(tmp_path / "proc", os.getpid(), _attempt_argv(tmp_path))
        live = probe_daemon(tmp_path, proc_root=proc_root)
        assert live.state == STATE_RUNNING, "something holds the lock"
        assert live.identity_ok is False
        assert "not a `-m trellis.sidecar" in live.detail
    finally:
        os.close(fd)


def test_a_daemon_over_another_runtime_root_is_flagged(tmp_path: Path) -> None:
    other = tmp_path / "other"
    other.mkdir()
    fd = acquire_pid_lock(tmp_path)
    try:
        proc_root = _fake_proc(tmp_path / "proc", os.getpid(), _daemon_argv(other))
        live = probe_daemon(tmp_path, proc_root=proc_root)
        assert live.identity_ok is False
        assert str(other) in live.detail
    finally:
        os.close(fd)


def test_an_unreadable_pid_file_is_unknown_not_an_exception(tmp_path: Path) -> None:
    """Nothing here may raise at a caller."""
    sidecar = _sidecar(tmp_path)
    pid_path = sidecar / "daemon.pid"
    pid_path.write_text("1\n")
    pid_path.chmod(0o000)
    try:
        live = probe_daemon(tmp_path)
        if os.geteuid() == 0:  # pragma: no cover — root ignores the mode
            pytest.skip("root can read a 0000 file")
        assert live.state == STATE_UNKNOWN
        assert "cannot read" in live.detail
    finally:
        pid_path.chmod(0o644)


def test_a_real_second_process_holding_the_lock_reads_running(tmp_path: Path) -> None:
    """The cross-process case, with a real pid the probe did not fork."""
    _sidecar(tmp_path)
    code = (
        "import fcntl,os,sys,time\n"
        f"fd=os.open({str(tmp_path / 'sidecar' / 'daemon.pid')!r}, "
        "os.O_RDWR|os.O_CREAT, 0o644)\n"
        "fcntl.flock(fd, fcntl.LOCK_EX)\n"
        "os.ftruncate(fd,0); os.pwrite(fd, str(os.getpid()).encode(), 0)\n"
        "sys.stdout.write('up\\n'); sys.stdout.flush()\n"
        "time.sleep(30)\n"
    )
    proc = subprocess.Popen(
        [sys.executable, "-c", code], stdout=subprocess.PIPE, text=True
    )
    try:
        assert proc.stdout is not None
        assert proc.stdout.readline().strip() == "up"
        live = probe_daemon(tmp_path)
        assert live.state == STATE_RUNNING
        assert live.pid == proc.pid
    finally:
        proc.kill()
        proc.wait(timeout=10)
    # And once it is gone, the same files read NOT RUNNING.
    assert probe_daemon(tmp_path).state == STATE_NOT_RUNNING


# ---------------------------------------------------------------------------
# status.json freshness
# ---------------------------------------------------------------------------


def test_status_age_and_the_stale_floor(tmp_path: Path) -> None:
    now = 10_000.0
    status = {"generated_at_ms": int((now - 100) * 1000)}
    assert status_age_seconds(status, now=now) == pytest.approx(100.0)
    assert status_is_stale(status, now=now) is False
    # The floor is 900s and NOT the poll interval's order of magnitude:
    # a bootstrap of N grunt workspaces legitimately goes minutes
    # between writes, and an alert that fires on every cold start is an
    # alert nobody reads.
    assert STATUS_STALE_FLOOR_SECONDS == 900.0
    old = {"generated_at_ms": int((now - 901) * 1000)}
    assert status_is_stale(old, now=now) is True


def test_a_long_poll_interval_widens_the_threshold(tmp_path: Path) -> None:
    now = 10_000.0
    status = {"generated_at_ms": int((now - 1000) * 1000), "poll_seconds": 600}
    # 4 * 600 = 2400 > the 900 floor.
    assert status_is_stale(status, now=now) is False
    assert status_is_stale(status, now=now, poll_seconds=30) is True


def test_absent_or_undated_status_is_stale(tmp_path: Path) -> None:
    """Fail-safe, exactly like spool.export_is_stale: the direction that
    makes a consumer stop asserting rather than start guessing."""
    assert read_status(tmp_path) is None
    assert status_is_stale(None, now=1.0) is True
    assert status_is_stale({}, now=1.0) is True
    assert status_is_stale({"generated_at_ms": "soon"}, now=1.0) is True


def test_a_future_timestamp_is_not_read_as_fresh_forever(tmp_path: Path) -> None:
    """Clock skew must clamp to 0, never go negative — a negative age
    passes every `age > threshold` test."""
    assert status_age_seconds({"generated_at_ms": 9_000_000}, now=1.0) == 0.0


def test_read_status_survives_a_corrupt_file(tmp_path: Path) -> None:
    _sidecar(tmp_path)
    (tmp_path / "sidecar" / "status.json").write_text("{not json")
    assert read_status(tmp_path) is None


# ---------------------------------------------------------------------------
# pool_health
# ---------------------------------------------------------------------------


def test_no_sidecar_dir_is_its_own_answer(tmp_path: Path) -> None:
    health = pool_health(tmp_path, now=1.0, proc_root=tmp_path / "proc")
    assert health.sidecar_dir_present is False
    assert health.liveness.state == STATE_NOT_RUNNING
    assert health_mod.exit_code(health) == EXIT_NO_SIDECAR_DIR


def test_a_post_drain_gap_is_legible(tmp_path: Path) -> None:
    """No daemon + live attempt processes is a DRAIN (a redeploy window
    that costs no grunt work); no daemon + no attempts is an outage.
    Nothing else on the box distinguishes them, and reading a drain as
    an outage is how a routine restart gets escalated."""
    _sidecar(tmp_path)
    proc_root = _fake_proc(tmp_path / "proc", 5150, _attempt_argv(tmp_path, "sc-live"))
    health = pool_health(tmp_path, now=1.0, proc_root=proc_root)
    assert health.liveness.state == STATE_NOT_RUNNING
    assert [row["attempt_id"] for row in health.attempt_processes] == ["sc-live"]
    assert health.attempt_processes[0]["node"] == "Rung"
    assert health_mod.exit_code(health) == EXIT_NOT_RUNNING
    assert "1 live attempt process(es)" in health_mod.render(health)


def test_pool_health_reports_the_status_surface(tmp_path: Path) -> None:
    now = 10_000.0
    _write_status(
        tmp_path,
        schema=1,
        generated_at_ms=int((now - 30) * 1000),
        pid=4242,
        poll_seconds=30,
        grunts=2,
        phase="running",
        suspended_until_ms=int((now + 600) * 1000),
        last_pass={"status": "journal_error", "at_ms": int((now - 30) * 1000)},
        in_flight=[
            {"node": "Rung", "entry_seq": 4, "grunt": 0, "attempt_id": "sc-a"}
        ],
        attempts={},
    )
    health = pool_health(tmp_path, now=now, proc_root=tmp_path / "proc")
    assert health.pool_size == 2
    assert health.status_stale is False
    assert health.poll_seconds == 30
    assert health.last_pass_status == "journal_error"
    assert health.suspended_for_seconds == pytest.approx(600.0)
    assert [row["node"] for row in health.in_flight] == ["Rung"]
    text = health_mod.render(health)
    assert "last pass: journal_error" in text
    assert "SUSPENDED" in text


def test_a_pending_sentinel_is_degraded_not_healthy(tmp_path: Path) -> None:
    now = 10_000.0
    sidecar = _sidecar(tmp_path)
    fd = acquire_pid_lock(tmp_path)
    try:
        _write_status(tmp_path, generated_at_ms=int((now - 5) * 1000), grunts=2)
        proc_root = _fake_proc(
            tmp_path / "proc", os.getpid(), _daemon_argv(tmp_path)
        )
        health = pool_health(tmp_path, now=now, proc_root=proc_root)
        assert health_mod.exit_code(health) == EXIT_RUNNING
        (sidecar / "drain").touch()
        health = pool_health(tmp_path, now=now, proc_root=proc_root)
        assert health.drain_sentinel is True
        assert health_mod.exit_code(health) == EXIT_DEGRADED
    finally:
        os.close(fd)


def test_a_stale_status_behind_a_live_daemon_is_degraded(tmp_path: Path) -> None:
    now = 10_000.0
    fd = acquire_pid_lock(tmp_path)
    try:
        _write_status(tmp_path, generated_at_ms=int((now - 5000) * 1000), grunts=2)
        proc_root = _fake_proc(
            tmp_path / "proc", os.getpid(), _daemon_argv(tmp_path)
        )
        health = pool_health(tmp_path, now=now, proc_root=proc_root)
        assert health.liveness.state == STATE_RUNNING
        assert health.status_stale is True
        assert health_mod.exit_code(health) == EXIT_DEGRADED
    finally:
        os.close(fd)


def test_every_spool_lane_is_counted(tmp_path: Path) -> None:
    sidecar = _sidecar(tmp_path)
    for lane in ("pending", "outcomes", "outcomes_consumed"):
        (sidecar / "spool" / lane).mkdir(parents=True)
    (sidecar / "spool" / "outcomes" / "outcome-sc-a.json").write_text("{}")
    health = pool_health(tmp_path, now=1.0, proc_root=tmp_path / "proc")
    assert health.spool_counts["outcomes"] == 1
    assert health.spool_counts["claimed_outcomes"] == 0
    assert "outcomes=1" in health_mod.render(health)


# ---------------------------------------------------------------------------
# spent_without_outcome — REPORT ONLY
# ---------------------------------------------------------------------------


def _seed_queue_and_attempts(tmp_path: Path, queue, attempted) -> None:
    sidecar = _sidecar(tmp_path)
    (sidecar / "candidates.json").write_text(
        json.dumps({"schema": 2, "cycle": 5, "queue": queue})
    )
    (sidecar / "attempted.json").write_text(json.dumps(attempted))


def test_spent_without_outcome_counts_only_unreported_generations(
    tmp_path: Path,
) -> None:
    _seed_queue_and_attempts(
        tmp_path,
        [
            {"node": "Reported", "entry_seq": 1, "status": "ready"},
            {"node": "Unreported", "entry_seq": 2, "status": "ready"},
            {"node": "Closed", "entry_seq": 3, "status": "ready"},
            {"node": "Untried", "entry_seq": 4, "status": "ready"},
        ],
        {
            "Reported": [
                {"entry_seq": 1, "status": "failed", "outcome_published": True}
            ],
            "Unreported": [{"entry_seq": 2, "status": "failed"}],
            # A success is never published on purpose: its closure is in
            # pending/, and expiring the entry would make the kernel
            # reject that closure `not_queued`.
            "Closed": [{"entry_seq": 3, "status": "success"}],
        },
    )
    rows = spent_without_outcome(tmp_path)
    assert [row["node"] for row in rows] == ["Unreported"]


def test_spent_without_outcome_ignores_other_generations(tmp_path: Path) -> None:
    """A remove + re-add mints a fresh entry_seq; the OLD generation's
    unpublished row says nothing about the new entry."""
    _seed_queue_and_attempts(
        tmp_path,
        [{"node": "Rung", "entry_seq": 9, "status": "ready"}],
        {"Rung": [{"entry_seq": 4, "status": "failed"}]},
    )
    assert spent_without_outcome(tmp_path) == []


def test_spent_without_outcome_is_reported_never_republished(tmp_path: Path) -> None:
    """Automatic republishing was designed and CUT: sidecar_queue_seq
    lives in ProtocolState, so a rewind the daemon did not witness
    reuses entry_seq values and a republished outcome would expire a
    live, never-attempted entry. Nothing here writes to the spool."""
    _seed_queue_and_attempts(
        tmp_path,
        [{"node": "Rung", "entry_seq": 1, "status": "ready"}],
        {"Rung": [{"entry_seq": 1, "status": "failed"}]},
    )
    spool = tmp_path / "sidecar" / "spool"
    health = pool_health(tmp_path, now=1.0, proc_root=tmp_path / "proc")
    assert [row["node"] for row in health.spent_without_outcome] == ["Rung"]
    assert not spool.exists() or not list(spool.rglob("*.json"))
    assert "Never hand-write a spool record." in health_mod.render(health)


def test_an_unconsumed_outcome_record_is_explained_not_blamed(tmp_path: Path) -> None:
    """THE false positive, measured on the live run: the outcome record
    is already published and merely unconsumed, and the row lacks the
    marker only because `outcome_published` did not exist when it was
    written. Telling the operator "the kernel was never told, remove it
    by hand" sends them to remove a live, correctly-reported entry."""
    _seed_queue_and_attempts(
        tmp_path,
        [{"node": "Rung", "entry_seq": 44, "status": "ready"}],
        {"Rung": [{"entry_seq": 44, "status": "failed"}]},
    )
    outcomes = tmp_path / "sidecar" / "spool" / "outcomes"
    outcomes.mkdir(parents=True)
    (outcomes / "outcome-sc-a.json").write_text(
        json.dumps({"attempt_id": "sc-a", "node": "Rung", "entry_seq": 44})
    )
    text = health_mod.render(pool_health(tmp_path, now=1.0, proc_root=tmp_path / "proc"))
    assert "spent with no publication marker" in text
    assert "unconsumed in outcomes/ (1 there now)" in text
    assert "Only if outcomes/ is empty" in text
    # The claim that would send the operator to break a healthy entry.
    assert "the kernel was never told the generation was spent" not in text


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def test_cli_exit_codes_and_json(tmp_path: Path, capsys) -> None:
    assert main([str(tmp_path)]) == EXIT_NO_SIDECAR_DIR
    _sidecar(tmp_path)
    assert main([str(tmp_path)]) == EXIT_NOT_RUNNING
    fd = acquire_pid_lock(tmp_path)
    try:
        # Held lock, but no status file at all -> stale -> degraded.
        assert main([str(tmp_path)]) == EXIT_DEGRADED
        _write_status(
            tmp_path,
            generated_at_ms=int(time.time() * 1000),
            pid=os.getpid(),
            grunts=2,
            in_flight=[],
        )
        assert main([str(tmp_path)]) == EXIT_RUNNING
        capsys.readouterr()
        assert main([str(tmp_path), "--json"]) == EXIT_RUNNING
        payload = json.loads(capsys.readouterr().out)
        assert payload["liveness"]["state"] == STATE_RUNNING
        assert payload["pool_size"] == 2
    finally:
        os.close(fd)


def test_cli_usage_error_is_64(tmp_path: Path, capsys) -> None:
    assert main([]) == 64


def test_liveness_to_dict_is_json_safe(tmp_path: Path) -> None:
    live = DaemonLiveness(state=STATE_RUNNING, pid=1, argv=("a", "b"))
    assert json.loads(json.dumps(live.to_dict()))["argv"] == ["a", "b"]
