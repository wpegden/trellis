"""Execution budgets are operational settings, independent of model identity."""
from __future__ import annotations

import math
import os
import signal
import subprocess
import threading
import time
from contextlib import contextmanager, ExitStack

DEFAULT_TIMEOUT_SECONDS = 3600.0
TIMEOUT_ENV = "TRELLIS_SOURCE_TIMEOUT"


@contextmanager
def owned_process(argv, *, cancel_grace=0.0, compute_timeout=None, **kwargs):
    """Own a tool session and propagate cancellation across nested owners.

    A parent may cancel us with SIGTERM while our tool is in another session.
    Convert that signal into an exception so the tool session is cleaned up.
    Defer signals during Popen until its process handle is available, and ignore
    repeated cancellation during cleanup. Signal ownership is main-thread only.
    """
    process = None
    cancelled = False
    cleaning = False
    armed = False
    previous = {}
    resources = ExitStack()
    leaf = None

    def cancel(signum, frame):
        nonlocal cancelled
        cancelled = True
        if armed and not cleaning:
            raise KeyboardInterrupt("Owned tool execution cancelled")

    if threading.current_thread() is threading.main_thread():
        for sig in (signal.SIGINT, signal.SIGTERM):
            previous[sig] = signal.signal(sig, cancel)
    try:
        if compute_timeout is not None:
            from trellis.checker.compute_leaf import launch
            leaf = resources.enter_context(launch(argv, environment=kwargs.get('env'),
                cwd=kwargs.get('cwd'), timeout=compute_timeout))
            if leaf is not None:
                argv, kwargs = leaf.command, dict(kwargs, env=leaf.environment)
        process = subprocess.Popen(argv, start_new_session=True, **kwargs)
        try:
            armed = True
            if cancelled:
                raise KeyboardInterrupt("Owned tool execution cancelled")
            if leaf is not None:
                leaf.entered(process)
            yield process
            if leaf is not None:
                leaf.finished(process)
        except BaseException:
            cleaning = True
            try:
                if leaf is not None:
                    leaf.cancel(process)
                if cancel_grace:
                    os.killpg(process.pid, signal.SIGTERM)
                    # Nested Python owners immediately kill their tool groups.
                    time.sleep(cancel_grace)
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            finally:
                process.wait()
            raise
        finally:
            armed = False
    finally:
        resources.close()
        for sig, handler in previous.items():
            signal.signal(sig, handler)
        if cancelled and not cleaning:
            # A signal can arrive after the wait completes while we restore
            # handlers. It must still stop the caller from launching more work.
            raise KeyboardInterrupt("Owned tool execution cancelled")


def run_owned_process(argv, **kwargs) -> subprocess.CompletedProcess:
    """Run bootstrap shells, allowing nested session owners to clean up."""
    with owned_process(argv, cancel_grace=0.5, **kwargs) as process:
        return subprocess.CompletedProcess(argv, process.wait())


def run_compute(argv, *, timeout=None, check=False, capture_output=False, **kwargs):
    """Subprocess-compatible host leaf, including standalone bootstrap checks."""
    from trellis.checker.compute_leaf import host_pool
    if host_pool() is None:
        return subprocess.run(argv, timeout=timeout, check=check, capture_output=capture_output, **kwargs)
    timeout = execution_timeout({}, 'standalone', timeout)
    if capture_output:
        kwargs.update(stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    with owned_process(argv, compute_timeout=timeout, **kwargs) as process:
        out, err = process.communicate(timeout=timeout)
    result = subprocess.CompletedProcess(argv, process.returncode, out, err)
    if check:
        result.check_returncode()
    return result


def positive_timeout(value) -> float:
    seconds = float(value)
    if isinstance(value, bool) or not math.isfinite(seconds) or seconds <= 0:
        raise ValueError("timeout must be a finite positive number of seconds")
    return seconds


def execution_timeout(tools: dict, operation: str, override: float | None = None,
                      *, default: float = DEFAULT_TIMEOUT_SECONDS) -> float:
    """Invocation > run environment > operation setting > legacy setting > default.

    There is no upper clamp. The environment override follows supervisor checks,
    sandboxed agents, and native bootstrap subprocesses.
    """
    timeouts = tools.get("timeouts", {})
    if not isinstance(timeouts, dict):
        raise ValueError("tools.timeouts must map operation names to seconds")
    value = override if override is not None else os.environ.get(
        TIMEOUT_ENV, timeouts.get(operation, tools.get("timeout", default)))
    return positive_timeout(value)


def semantic_tools(tools: dict) -> dict:
    """Successful artifacts retain their identity when execution budgets change."""
    result = {name: value for name, value in tools.items() if name not in {"timeout", "timeouts"}}
    if "env" in result:
        result["env"] = {name: value for name, value in result["env"].items() if name != TIMEOUT_ENV}
    return result


@contextmanager
def execution_budget(seconds: float | None):
    """Scope a CLI override, including its child processes."""
    previous = os.environ.get(TIMEOUT_ENV)
    if seconds is not None:
        os.environ[TIMEOUT_ENV] = str(positive_timeout(seconds))
    try:
        yield
    finally:
        if previous is None:
            os.environ.pop(TIMEOUT_ENV, None)
        else:
            os.environ[TIMEOUT_ENV] = previous


if __name__ == '__main__':
    import sys
    command = sys.argv[1:]
    if command[:1] == ['--']:
        command = command[1:]
    raise SystemExit(run_compute(command).returncode)
