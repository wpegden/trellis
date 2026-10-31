"""A managed process tree has a memory budget, enforced like the wall."""

from __future__ import annotations

import os
import subprocess
import sys
import time

from trellis.memory_guard import (
    GIB,
    MemoryGuard,
    descendant_rss,
    wait_with_guard,
)

# A child that holds ~256 MiB resident and then idles; a grandchild proves
# the walk reaches the whole tree.
HOG = (
    "import time, sys; x = bytearray(256 * 1024 * 1024); x[::4096] = b'x' * len(x[::4096]); "
    "sys.stdout.write('ready\\n'); sys.stdout.flush(); time.sleep(60)"
)


def _spawn_tree():
    proc = subprocess.Popen(
        ["bash", "-c", f"{sys.executable} -c \"{HOG}\" & wait"],
        stdout=subprocess.PIPE,
        start_new_session=True,
    )
    assert proc.stdout is not None
    assert proc.stdout.readline().strip() == b"ready"
    return proc


def _dead(pgid: int) -> bool:
    for _ in range(50):
        try:
            os.killpg(pgid, 0)
        except ProcessLookupError:
            return True
        time.sleep(0.1)
    return False


def test_descendant_rss_sees_the_whole_tree():
    proc = _spawn_tree()
    try:
        total, rows = descendant_rss(proc.pid)
        assert total > 200 * 1024 * 1024
        assert rows[0][2].startswith("python")
        assert any(comm == "bash" for _pid, _rss, comm in rows)
    finally:
        os.killpg(proc.pid, 9)


def test_guard_trips_on_the_tree_cap_and_names_the_largest_process():
    proc = _spawn_tree()
    try:
        guard = MemoryGuard.from_gib(0.1)
        reason = guard.check(proc.pid)
        assert reason and reason.startswith("memory budget:")
        assert "exceeds the 0.1 GiB cap" in reason and "python" in reason
        assert guard.peak_bytes > 200 * 1024 * 1024
        assert MemoryGuard.from_gib(4.0).check(proc.pid) is None
    finally:
        os.killpg(proc.pid, 9)


def test_guard_trips_on_the_machine_floor(tmp_path):
    meminfo = tmp_path / "meminfo"
    meminfo.write_text("MemTotal:  60000000 kB\nMemAvailable:  1000000 kB\n")
    proc = _spawn_tree()
    try:
        guard = MemoryGuard(limit_bytes=0, floor_bytes=2 * GIB, meminfo=meminfo,
                            floor_min_tree_bytes=64 * 1024 * 1024)
        reason = guard.check(proc.pid)
        assert reason and "under the 2.0 GiB floor" in reason
        meminfo.write_text("MemAvailable:  50000000 kB\n")
        assert guard.check(proc.pid) is None
        # A tree below the floor's minimum is not the consumer to kill: a
        # 256 MiB attempt survives a short machine (default minimum is a
        # quarter of the cap, 5 GiB here).
        meminfo.write_text("MemAvailable:  1000000 kB\n")
        assert MemoryGuard.from_gib(20.0, 6.0).check(proc.pid) is None
        bystander = MemoryGuard.from_gib(20.0, 6.0)
        assert bystander.floor_min_tree_bytes == 5 * GIB
    finally:
        os.killpg(proc.pid, 9)


def test_wait_with_guard_kills_the_group_on_a_memory_breach():
    proc = _spawn_tree()
    seen = []
    budget, rc, reason = wait_with_guard(
        proc, wall_seconds=30, guard=MemoryGuard.from_gib(0.1), poll_seconds=0.2,
        log=seen.append,
    )
    assert (budget, rc) == ("memory", None)
    assert reason.startswith("memory budget:") and seen == [reason]
    assert _dead(proc.pid)


def test_wait_with_guard_keeps_wall_and_normal_exit_semantics():
    proc = _spawn_tree()
    budget, rc, reason = wait_with_guard(
        proc, wall_seconds=0.5, guard=MemoryGuard.from_gib(4.0), poll_seconds=0.1
    )
    assert (budget, rc, reason) == ("wall", None, "wall budget")
    assert _dead(proc.pid)

    quick = subprocess.Popen(["true"], start_new_session=True)
    assert wait_with_guard(quick, wall_seconds=5, guard=MemoryGuard.from_gib(4.0)) == (None, 0, None)
