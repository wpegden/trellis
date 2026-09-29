"""Harness memory limits, using small Python processes in place of Lean."""

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

import pytest

from trellis.sidecar import compile_loop, workspace
from trellis.sidecar.config import SidecarConfig
from trellis.sidecar.memory_guard import AttemptBudgetExhausted


NODE = "theorem Foo : True := by\n-- BODY\n  sorry\n"

# A protocol-speaking stand-in that grows a child during initialization,
# didOpen, or didChange. The child proves the cap/cleanup cover descendants.
SERVER = r'''
import json, os, subprocess, sys, time
from pathlib import Path
phase, directory = sys.argv[1:]
directory = Path(directory)
(directory / "server.json").write_text(json.dumps([os.getpid(), os.getpgrp()]))

def hog():
    child = subprocess.Popen([sys.executable, "-c", """
import os, sys, time
from pathlib import Path
memory = bytearray(64 * 1024 * 1024)
Path(sys.argv[1]).write_text(str(os.getpid()))
time.sleep(30)
""", str(directory / "child.pid")])
    child.wait()

def send(value):
    data = json.dumps(value).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(data) + data)
    sys.stdout.buffer.flush()

if phase == "prewarm":
    hog()
    sys.exit(0)

while True:
    length = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line == b"\r\n":
            break
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":", 1)[1])
    message = json.loads(sys.stdin.buffer.read(length))
    method = message.get("method")
    if method == "initialize":
        if phase == "init":
            hog()
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": {}}})
    elif method in {"textDocument/didOpen", "textDocument/didChange"}:
        if (method.endswith("didOpen") and phase == "open") or (method.endswith("didChange") and phase == "change"):
            hog()
        uri = message["params"]["textDocument"]["uri"]
        send({"method": "textDocument/publishDiagnostics", "params": {"uri": uri, "diagnostics": []}})
        send({"method": "$/lean/fileProgress", "params": {"textDocument": {"uri": uri}, "processing": []}})
    elif method == "exit":
        sys.exit(0)
'''


def _command(tmp_path: Path, phase: str):
    script = tmp_path / "server.py"
    script.write_text(SERVER)
    return [sys.executable, str(script), phase, str(tmp_path)]


def _config(cap=0.03):
    return SidecarConfig(sandbox_role="", attempt_memory_gb=cap, system_memory_floor_gb=0)


def _alive(pid):
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
    except FileNotFoundError:
        return False
    return stat[stat.rfind(")") + 2:].split()[0] != "Z"


def _assert_tree_dead(tmp_path):
    pid, pgid = json.loads((tmp_path / "server.json").read_text())
    child = int((tmp_path / "child.pid").read_text())
    deadline = time.monotonic() + 3
    while (_alive(pid) or _alive(child)) and time.monotonic() < deadline:
        time.sleep(0.02)
    assert not _alive(pid) and not _alive(child)
    return pgid


def test_prewarm_memory_breach_kills_descendants_and_is_terminal(tmp_path, monkeypatch):
    command = _command(tmp_path, "prewarm")
    monkeypatch.setattr(workspace, "build_lake_command", lambda *_a, **_kw: command)
    with pytest.raises(AttemptBudgetExhausted, match="prewarm build killed: memory budget") as exc:
        workspace.prewarm_node_closure(tmp_path, "Foo", _config(), timeout=10)
    assert exc.value.peak_bytes > 32 * 1024 * 1024
    assert _assert_tree_dead(tmp_path) == os.getpgrp()


def test_prewarm_compile_error_remains_nonfatal(tmp_path, monkeypatch):
    monkeypatch.setattr(workspace, "build_lake_command", lambda *_a, **_kw: [
        sys.executable, "-c", "import sys; print('unknown tactic'); sys.exit(1)",
    ])
    ok, _seconds, detail = workspace.prewarm_node_closure(tmp_path, "Foo", _config(), timeout=10)
    assert not ok and "unknown tactic" in detail


def test_attempt_peak_is_the_maximum_across_agent_rounds(tmp_path, monkeypatch):
    from trellis.sidecar import codex_driver
    from trellis.sidecar.driver import AttemptResult

    (tmp_path / "Tablet").mkdir()
    (tmp_path / "Tablet" / "Foo.lean").write_text(NODE)
    rounds = iter([
        AttemptResult(status="failed", retryable=True, proof_body="  simp\n", peak_memory_bytes=200),
        AttemptResult(status="success", proof_body="  trivial\n", peak_memory_bytes=100),
    ])
    monkeypatch.setattr(codex_driver, "_attempt_round", lambda **_kw: next(rounds))
    result = codex_driver.run_attempt_codex(
        config=_config(), repo=tmp_path, node="Foo", node_content=NODE,
        system_prompt="fixture", initial_body="  sorry\n", compile_body_factory=lambda: None,
    )
    assert result.status == "success" and result.iterations == 2
    assert result.peak_memory_bytes == 200


@pytest.mark.parametrize("phase", ["init", "open", "change"])
def test_lsp_memory_breach_never_falls_back_or_restarts(tmp_path, monkeypatch, phase):
    (tmp_path / "Tablet").mkdir()
    loop = compile_loop.SidecarCompileLoop(tmp_path, _config(), server_cmd=_command(tmp_path, phase))
    monkeypatch.setattr(loop, "_lake_fallback", lambda *_a: pytest.fail("memory kill must not fallback"))
    try:
        opened = loop.open_node("Foo", NODE)
        assert opened == (phase == "change")
        verdict = loop.check_body("Foo", NODE, "  trivial\n")
        assert not verdict.ok and verdict.budget_exhausted
        assert "memory budget" in verdict.log
        assert loop.peak_memory_bytes > 32 * 1024 * 1024
        assert _assert_tree_dead(tmp_path) == os.getpgrp()
        monkeypatch.setattr(compile_loop, "_MemoryGuardedLeanServer", lambda *_a, **_kw: pytest.fail("must not restart"))
        assert not loop.open_node("Foo", NODE)
        assert loop.check_body("Foo", NODE, "  trivial\n").budget_exhausted
        assert loop.confirm("Foo", NODE, "  trivial\n").budget_exhausted
    finally:
        loop.shutdown()


@pytest.mark.parametrize("phase", ["prewarm", "lsp", "confirm"])
def test_manager_cancellation_reaches_harness_builds(tmp_path, phase):
    """The manager kills the attempt's group, including active harness work."""
    command = _command(tmp_path, "open" if phase == "lsp" else "prewarm")
    (tmp_path / "command.json").write_text(json.dumps(command))
    runner = r'''
import json, sys
from pathlib import Path
from trellis.sidecar import compile_loop, workspace
from trellis.sidecar.config import SidecarConfig
root, phase = Path(sys.argv[1]), sys.argv[2]
command = json.loads((root / "command.json").read_text())
config = SidecarConfig(sandbox_role="", attempt_memory_gb=0, system_memory_floor_gb=0)
if phase == "prewarm":
    workspace.build_lake_command = lambda *a, **kw: command
    workspace.prewarm_node_closure(root, "Foo", config)
elif phase == "confirm":
    compile_loop.build_lake_command = lambda *a, **kw: command
    compile_loop.SidecarCompileLoop(root, config)._lake_build("Foo")
else:
    loop = compile_loop.SidecarCompileLoop(root, config, server_cmd=command)
    loop.open_node("Foo", "theorem Foo : True := by\n-- BODY\n  sorry\n")
'''
    proc = subprocess.Popen([sys.executable, "-c", runner, str(tmp_path), phase], start_new_session=True)
    try:
        deadline = time.monotonic() + 10
        while not (tmp_path / "child.pid").exists() and time.monotonic() < deadline:
            assert proc.poll() is None
            time.sleep(0.02)
        assert (tmp_path / "child.pid").exists()
        os.killpg(proc.pid, signal.SIGTERM)
        proc.wait(timeout=5)
        assert _assert_tree_dead(tmp_path) == proc.pid
    finally:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.wait(timeout=5)


def test_confirm_releases_lsp_before_starting_its_build(tmp_path, monkeypatch):
    loop = compile_loop.SidecarCompileLoop(tmp_path, _config(), server_cmd=_command(tmp_path, "healthy"))
    try:
        assert loop.open_node("Foo", NODE)
        pid, _pgid = json.loads((tmp_path / "server.json").read_text())
        assert _alive(pid)

        def build(*_a, **_kw):
            assert not _alive(pid), "LSP and lake must not overlap their separate memory caps"
            return [sys.executable, "-c", "print('Build completed successfully')"]

        monkeypatch.setattr(compile_loop, "build_lake_command", build)
        assert loop._lake_build("Foo").ok
    finally:
        loop.shutdown()
