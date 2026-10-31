"""Exercise a real driver -> shell -> grandchild process tree."""
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

import pytest


def test_cancellation_at_handler_restoration_is_not_swallowed(monkeypatch):
    from trellis.pv_execution import run_owned_process
    original = signal.signal
    previous = signal.getsignal(signal.SIGINT)
    injected = False
    def install(sig, handler):
        nonlocal injected
        if sig == signal.SIGINT and handler is previous and not injected:
            injected = True
            signal.getsignal(sig)(sig, None)
        return original(sig, handler)
    monkeypatch.setattr(signal, "signal", install)
    with pytest.raises(KeyboardInterrupt, match="cancelled"):
        run_owned_process([sys.executable, "-c", "pass"])
    assert injected
    assert signal.getsignal(signal.SIGINT) is previous


@pytest.mark.parametrize("nested_session", [False, True])
@pytest.mark.parametrize("cancel_signal", [signal.SIGINT, signal.SIGTERM])
def test_interruption_kills_bootstrap_grandchildren(tmp_path, nested_session, cancel_signal):
    child = tmp_path / "child.py"
    child.write_text("from pathlib import Path\nimport os,signal,time\n"
                     "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
                     f"Path({str(tmp_path / 'started')!r}).write_text(str(os.getpid()))\n"
                     "time.sleep(2)\n"
                     f"Path({str(tmp_path / 'continued')!r}).write_text('still running')\n")
    shell = tmp_path / "shell.sh"
    owner = tmp_path / "extractor.py"
    owner.write_text("import os\nfrom pathlib import Path\n"
                     "from trellis.modeling.environment import run_source_command\n"
                     f"run_source_command([{sys.executable!r}, {str(child)!r}], cwd=Path({str(tmp_path)!r}), "
                     f"env=dict(os.environ), stdout=Path({str(tmp_path / 'extractor.log')!r}))\n")
    shell.write_text(f'#!/bin/sh\n"{sys.executable}" "{owner if nested_session else child}"\nwait\n')
    driver = tmp_path / "driver.py"
    driver.write_text("from trellis.pv_execution import run_owned_process\n"
                      f"run_owned_process(['sh', {str(shell)!r}])\n")
    env = dict(os.environ, PYTHONPATH=str(Path(__file__).resolve().parents[1]))
    process = subprocess.Popen([sys.executable, str(driver)], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               start_new_session=True)
    child_pid = None
    try:
        deadline = time.monotonic() + 10
        while not (tmp_path / "started").exists():
            assert process.poll() is None
            assert time.monotonic() < deadline
            time.sleep(0.02)
        child_pid = int((tmp_path / "started").read_text())
        process.send_signal(cancel_signal)
        process.communicate(timeout=10)
        assert process.returncode != 0
        deadline = time.monotonic() + 2
        while Path(f"/proc/{child_pid}/stat").exists():
            state = Path(f"/proc/{child_pid}/stat").read_text().rsplit(')', 1)[1].split()[0]
            if state == 'Z':
                break
            assert time.monotonic() < deadline, 'bootstrap grandchild survived cancellation'
            time.sleep(0.02)
        assert not (tmp_path / "continued").exists()
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        if child_pid:
            try:
                os.kill(child_pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
