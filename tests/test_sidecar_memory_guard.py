"""A grunt attempt's process tree has a memory budget, enforced like the wall."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path

import pytest

from trellis.sidecar import codex_driver
from trellis.sidecar.codex_driver import build_codex_task, run_attempt_codex
from trellis.sidecar.config import SidecarConfig
from trellis.sidecar.memory_guard import (
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


def test_config_reads_the_memory_budget_and_lets_zero_disable_it(tmp_path):
    def cfg(budgets):
        path = tmp_path / "trellis.config.json"
        path.write_text(json.dumps({"sidecar": {"enabled": True, "budgets": budgets}}))
        return SidecarConfig.load(path)

    assert SidecarConfig().attempt_memory_gb == 20.0
    assert SidecarConfig().system_memory_floor_gb == 6.0
    assert cfg({"attempt_memory_gb": 32, "system_memory_floor_gb": 4}).attempt_memory_gb == 32.0
    assert cfg({"system_memory_floor_gb": 4}).system_memory_floor_gb == 4.0
    assert cfg({"attempt_memory_gb": 0}).attempt_memory_gb == 0.0
    assert cfg({"attempt_memory_gb": "lots"}).attempt_memory_gb == 20.0


def test_prompt_states_the_memory_cap_once():
    task = build_codex_task("Foo", "SYS", wall_seconds=600, memory_gb=20)
    assert task.count("20 GB memory cap") == 1
    assert "Run one build at a time" in task
    assert "ONLY time budget" in task
    assert "memory cap" not in build_codex_task("Foo", "SYS", wall_seconds=600)


PREFIX = "theorem Foo : True := by\n"
NODE_CONTENT = PREFIX + "-- BODY\n  sorry\n"


def test_a_memory_breach_ends_the_attempt_as_budget_exhausted(tmp_path, monkeypatch):
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / "Tablet" / "Foo.lean").write_text(NODE_CONTENT)
    subprocess.run(["git", "-C", str(repo), "init", "-q"], check=True)
    subprocess.run(["git", "-C", str(repo), "add", "-A"], check=True)
    subprocess.run(["git", "-C", str(repo), "-c", "user.email=t@t", "-c", "user.name=t",
                    "commit", "-qm", "base"], check=True)
    bindir = tmp_path / "bin"
    bindir.mkdir()
    codex = bindir / "codex"
    codex.write_text(
        "#!/usr/bin/env bash\ncat >/dev/null\n"
        f"exec {sys.executable} -c \"{HOG}\"\n"
    )
    codex.chmod(0o755)
    env = dict(os.environ)
    env["PATH"] = f"{bindir}{os.pathsep}{env['PATH']}"
    monkeypatch.setattr(os, "environ", env)
    monkeypatch.setattr(codex_driver, "CODEX", str(codex))

    config = SidecarConfig(provider="codex", model_name="gpt-5.6-luna", attempt_wall_seconds=60.0,
                           lean_threads=1, attempt_memory_gb=0.1, system_memory_floor_gb=0.0)
    logged = []
    result = run_attempt_codex(
        config=config, repo=repo, node="Foo", node_content=NODE_CONTENT, system_prompt="SYS",
        initial_body="  sorry", compile_body_factory=lambda: (lambda body: None),
        codex_home=tmp_path / "codex-home", stream_path=tmp_path / "stream.jsonl", log=logged.append,
    )
    assert result.status == "budget_exhausted", result
    assert "memory budget: attempt tree at" in result.detail, result.detail
    assert result.peak_memory_bytes > 200 * 1024 * 1024
    assert result.wall_secs < 30
    assert any(m.startswith("memory budget:") for m in logged)
    assert (repo / "Tablet" / "Foo.lean").read_text() == NODE_CONTENT


def test_a_memory_breach_after_an_edit_is_still_budget_exhausted(tmp_path, monkeypatch):
    """The grunt had already rewritten the body (leaving a `sorry`) when the
    cap ended the attempt: the ledger must carry the guard's reason, not read
    the half-written body as a deliberate banned token."""
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / "Tablet" / "Foo.lean").write_text(NODE_CONTENT)
    subprocess.run(["git", "-C", str(repo), "init", "-q"], check=True)
    subprocess.run(["git", "-C", str(repo), "add", "-A"], check=True)
    subprocess.run(["git", "-C", str(repo), "-c", "user.email=t@t", "-c", "user.name=t",
                    "commit", "-qm", "base"], check=True)
    bindir = tmp_path / "bin"
    bindir.mkdir()
    codex = bindir / "codex"
    codex.write_text(
        "#!/usr/bin/env bash\ncat >/dev/null\n"
        "printf '  simp\\n  sorry\\n' >> Tablet/Foo.lean\n"
        f"exec {sys.executable} -c \"{HOG}\"\n"
    )
    codex.chmod(0o755)
    env = dict(os.environ)
    env["PATH"] = f"{bindir}{os.pathsep}{env['PATH']}"
    monkeypatch.setattr(os, "environ", env)
    monkeypatch.setattr(codex_driver, "CODEX", str(codex))

    config = SidecarConfig(provider="codex", model_name="gpt-5.6-luna", attempt_wall_seconds=60.0,
                           lean_threads=1, attempt_memory_gb=0.1, system_memory_floor_gb=0.0)
    result = run_attempt_codex(
        config=config, repo=repo, node="Foo", node_content=NODE_CONTENT, system_prompt="SYS",
        initial_body="  sorry", compile_body_factory=lambda: (lambda body: None),
        codex_home=tmp_path / "codex-home", stream_path=tmp_path / "stream.jsonl", log=lambda _m: None,
    )
    assert result.status == "budget_exhausted", result
    assert result.detail.startswith("memory budget: attempt tree at"), result.detail
    assert "banned token" not in result.detail
    assert not result.retryable
