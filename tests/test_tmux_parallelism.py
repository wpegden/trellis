"""Real isolated tmux regression: each agent inherits its own run's settings."""

import json
import os
import shutil
import subprocess
import sys
import time
import uuid

import pytest

from trellis.agents import tmux_backend


@pytest.mark.skipif(shutil.which("tmux") is None, reason="requires tmux")
def test_agent_parallelism_overrides_stale_server_without_changing_other_runs(tmp_path, monkeypatch):
    socket = "trellis-test-par-" + uuid.uuid4().hex[:12]
    keys = ("TRELLIS_LEAN_PARALLELISM", "TRELLIS_BURST_LEAN_PARALLELISM", "LEAN_NUM_THREADS")
    monkeypatch.setenv("TRELLIS_TMUX_SOCKET", socket)
    stale_env = dict(os.environ, **dict.fromkeys(keys, "9"))
    subprocess.run(
        ["tmux", "-L", socket, "new-session", "-d", "-s", "anchor", "sleep", "60"],
        env=stale_env, check=True, capture_output=True,
    )
    sessions = []
    try:
        for selected in ("3", "2"):
            for key in keys:
                monkeypatch.setenv(key, selected)
            output = tmp_path / f"agent-{selected}.json"
            command = [
                sys.executable, "-c",
                "import json,os,pathlib,time; "
                f"pathlib.Path({str(output)!r}).write_text(json.dumps("
                f"{{k:os.getenv(k) for k in {keys!r}}})); time.sleep(30)",
            ]
            wrapped = tmux_backend.sandbox_wrap(
                command, enabled=False, work_dir=tmp_path, burst_home=None, role="worker",
            )
            name = f"worker-{selected}"
            sessions.append(name)
            tmux_backend.new_session(name, cwd=tmp_path, cmd=wrapped, isolate_tmpdir=False)
            deadline = time.monotonic() + 5
            while not output.exists() and time.monotonic() < deadline:
                time.sleep(0.02)
            assert json.loads(output.read_text()) == dict.fromkeys(keys, selected)
        # These are per-session overrides; unrelated runs retain their settings.
        for key in keys:
            result = subprocess.run(
                ["tmux", "-L", socket, "show-environment", "-g", key],
                check=True, capture_output=True, text=True,
            )
            assert result.stdout.strip() == f"{key}=9"
    finally:
        for name in sessions:
            tmux_backend.kill_session(name)
        subprocess.run(["tmux", "-L", socket, "kill-server"], capture_output=True)
