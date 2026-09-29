from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import threading
import time
from typing import Any, Dict, List, Mapping, Optional, Tuple


class KernelCliError(RuntimeError):
    """Raised when the trellis kernel CLI cannot be invoked successfully."""


# Env var key shared with `trellis.atomic_actions.observations._progress_emit`.
# The kernel binary inherits the parent process env and does not env_clear()
# its own python3 children (see `run_repo_command_json` in
# kernel/src/runtime_cli_observations.rs), so setting this here propagates
# through to the deep-child observations module that does the per-node
# work inside phase 6/6.
_PROGRESS_LOG_ENV = "TRELLIS_ACCEPTANCE_PROGRESS_LOG"


def _setup_progress_tail() -> Tuple[Optional[Path], Dict[str, str], Optional[threading.Event], Optional[threading.Thread]]:
    """Create a per-call progress-tail log file and tail-and-forward thread.

    The kernel binary captures (via `Stdio::piped + cmd.output`) the stderr
    of the python3 children it spawns for `materialize-tablet-oleans`,
    `lean-semantic-payloads`, and the other atomic-action subcommands. That
    means a plain `print(..., file=sys.stderr)` from inside those children
    is invisible to whoever ran the outer kernel CLI — so phase 6/6's
    multi-minute per-node loops appear silent.

    To restore visibility without rebuilding the kernel binary, we open a
    private append-only log file and put its path into the environment
    variable `TRELLIS_ACCEPTANCE_PROGRESS_LOG`. The python3 child
    `_progress_emit` helper appends a line per substep; this thread tails
    the file and forwards each new line to `sys.stderr` in real time.

    The file is per-call (unique tempfile name) so concurrent kernel CLI
    invocations don't share a sink. Returns
    `(path, env_addition, stop_event, thread)`. When tempfile creation
    fails (read-only filesystem, etc.) the function returns
    `(None, {}, None, None)` and the caller proceeds without sub-progress
    forwarding — visibility is best-effort, never required for
    correctness.
    """
    try:
        fd, raw_path = tempfile.mkstemp(prefix="trellis-progress-", suffix=".log")
        os.close(fd)
    except OSError:
        return None, {}, None, None
    path = Path(raw_path)
    stop_event = threading.Event()

    def _tail_and_forward() -> None:
        # Open in binary append+read mode so we can both ensure the file
        # exists and read whatever has been appended so far. We seek to
        # end-of-file initially because a clean start should not replay
        # any historical content (the file is fresh anyway, but staying
        # defensive keeps this simple to reason about).
        try:
            handle = open(path, "rb")
        except OSError:
            return
        try:
            handle.seek(0, os.SEEK_END)
            buf = b""
            while not stop_event.is_set():
                chunk = handle.read(65536)
                if not chunk:
                    time.sleep(0.1)
                    continue
                buf += chunk
                while b"\n" in buf:
                    line, buf = buf.split(b"\n", 1)
                    try:
                        sys.stderr.write(line.decode("utf-8", errors="replace") + "\n")
                        sys.stderr.flush()
                    except Exception:
                        pass
            # Drain any final lines after the stop signal so we do not
            # lose progress messages emitted during shutdown.
            try:
                tail = handle.read()
                if tail:
                    buf += tail
            except OSError:
                pass
            if buf:
                # Whatever is left after the last newline (likely a
                # trailing partial line). Forward it best-effort.
                try:
                    sys.stderr.write(buf.decode("utf-8", errors="replace"))
                    sys.stderr.flush()
                except Exception:
                    pass
        finally:
            try:
                handle.close()
            except Exception:
                pass

    thread = threading.Thread(target=_tail_and_forward, daemon=True)
    thread.start()
    return path, {_PROGRESS_LOG_ENV: str(path)}, stop_event, thread


def _teardown_progress_tail(
    path: Optional[Path],
    stop_event: Optional[threading.Event],
    thread: Optional[threading.Thread],
) -> None:
    if stop_event is not None:
        stop_event.set()
    if thread is not None:
        thread.join(timeout=2.0)
    if path is not None:
        try:
            path.unlink()
        except OSError:
            pass


def kernel_cli_command() -> list[str]:
    raw = os.environ.get("TRELLIS_TRELLIS_KERNEL_CMD", "").strip()
    if raw:
        return shlex.split(raw)
    source_root = Path(__file__).resolve().parents[2]
    vendored_binary = source_root.parent / "bin" / "trellis_runtime_cli"
    if vendored_binary.is_file():
        return [str(vendored_binary)]
    built_binary = source_root / "kernel" / "target" / "debug" / "trellis_runtime_cli"
    if built_binary.is_file():
        return [str(built_binary)]
    cargo = shutil.which("cargo")
    if cargo is None:
        fallback = Path.home() / ".cargo" / "bin" / "cargo"
        if fallback.exists():
            cargo = str(fallback)
    if cargo is None:
        raise KernelCliError("cannot find cargo for trellis kernel invocation")
    manifest = source_root / "kernel" / "Cargo.toml"
    if not manifest.is_file():
        raise KernelCliError(f"vendored kernel manifest is missing: {manifest}")
    return [
        cargo,
        "run",
        "--quiet",
        "--manifest-path",
        str(manifest),
        "--bin",
        "trellis_runtime_cli",
    ]


def _run_kernel_cli_once(payload_text: str) -> subprocess.CompletedProcess[str]:
    """Run the kernel CLI once with line-by-line stderr forwarding.

    The kernel CLI's `check_trellis_worker_result` action emits
    `[acceptance] phase k/N: ...` progress lines to stderr while it runs
    its multi-minute disk-bound checks. Buffering stderr until the child
    exits (as the previous `subprocess.run(capture_output=True)` did)
    leaves the calling agent staring at a silent tool until the JSON
    response arrives. We instead spawn the child with `Popen` and drive
    its three pipes from three sidecar threads:

      - `_drain_stderr` reads stderr line-by-line and forwards each
        complete line to the host's `sys.stderr` immediately, so the
        calling agent sees real-time progress in its tool-output stream.
      - `_pump_stdin` writes the JSON request payload and closes stdin.
      - the main thread `read()`s stdout to EOF (the kernel response is
        a single JSON document; we parse it after the child exits).

    We do NOT use `process.communicate()` because it spawns its own
    internal stderr drainer when stderr is a PIPE — that drainer races
    our `_drain_stderr` for the same fd, causing some lines to be
    silently consumed by the wrong reader. Driving the three pipes
    directly avoids the race.

    Stderr is opened in BINARY mode (no `text=True`) and decoded after
    each readline. Python's text-mode `Popen.stderr.readline()` over a
    pipe is not reliably line-streaming (the TextIOWrapper above the
    BufferedReader can hold complete lines until a chunk's worth of
    bytes arrives, making short progress lines invisible until a much
    larger write or EOF flushes the buffer). Reading raw bytes via
    `BufferedReader.readline()` returns each complete line as soon as
    its trailing `\n` is seen, which is what we need.

    A second sidecar thread (`_setup_progress_tail`) tails an append-only
    log file whose path is exported to the child via the
    `TRELLIS_ACCEPTANCE_PROGRESS_LOG` env var. The kernel binary's stderr
    drainer above only sees what the kernel itself prints — when the
    kernel shells out to `python3 .../check.py materialize-tablet-oleans`
    or `lean-semantic-payloads` inside phase 6/6, those grandchild Python
    processes have their stderr captured by the kernel (via Stdio::piped +
    cmd.output) and discarded on success. The progress-tail file is the
    side channel that lets the deep-child observations module stream
    per-node sub-progress (`[acceptance]   materialize-tablet-oleans
    (3/29) ProjectionOccupancyBound`) to the calling agent in real time.
    """
    progress_path, progress_env, progress_stop, progress_thread = _setup_progress_tail()
    try:
        return _run_kernel_cli_inner(payload_text, progress_env)
    finally:
        _teardown_progress_tail(progress_path, progress_stop, progress_thread)


def _run_kernel_cli_inner(
    payload_text: str,
    progress_env: Mapping[str, str],
) -> subprocess.CompletedProcess[str]:
    if progress_env:
        env = os.environ.copy()
        env.update(progress_env)
    else:
        env = None
    process = subprocess.Popen(
        kernel_cli_command(),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    payload_bytes = payload_text.encode("utf-8")
    stderr_chunks: List[bytes] = []
    stdout_chunks: List[bytes] = []
    stdin_error: List[BaseException] = []

    def _drain_stderr() -> None:
        # readline() returns b'' only at EOF; each iteration writes one
        # complete line as it arrives so progress shows up in real time.
        # Reading directly from the underlying BufferedReader (binary
        # mode, no TextIOWrapper) is what makes line streaming actually
        # work — see the function docstring.
        stderr = process.stderr
        if stderr is None:
            return
        try:
            for raw in iter(stderr.readline, b""):
                stderr_chunks.append(raw)
                try:
                    decoded = raw.decode("utf-8", errors="replace")
                    sys.stderr.write(decoded)
                    sys.stderr.flush()
                except Exception:
                    # Don't let a flaky stderr writer kill the kernel
                    # call — the captured copy in `stderr_chunks` is
                    # still authoritative for the error-path message
                    # below.
                    pass
        except (ValueError, OSError):
            # The pipe may close from under us during shutdown if the
            # parent kills the child; lines written before the close
            # have already been forwarded.
            pass

    def _drain_stdout() -> None:
        stdout = process.stdout
        if stdout is None:
            return
        try:
            while True:
                chunk = stdout.read(65536)
                if not chunk:
                    return
                stdout_chunks.append(chunk)
        except (ValueError, OSError):
            pass

    def _pump_stdin() -> None:
        stdin = process.stdin
        if stdin is None:
            return
        try:
            stdin.write(payload_bytes)
        except BrokenPipeError as exc:
            # Child died before reading the full payload; the stdout
            # path will surface the actual error.
            stdin_error.append(exc)
        finally:
            try:
                stdin.close()
            except Exception:
                pass

    stderr_thread = threading.Thread(target=_drain_stderr, daemon=True)
    stdout_thread = threading.Thread(target=_drain_stdout, daemon=True)
    stdin_thread = threading.Thread(target=_pump_stdin, daemon=True)
    stderr_thread.start()
    stdout_thread.start()
    stdin_thread.start()

    try:
        process.wait()
    except Exception:
        # Best-effort cleanup so we don't leak a child on unexpected
        # exceptions in the parent (KeyboardInterrupt, etc.).
        process.kill()
        for t in (stdin_thread, stdout_thread, stderr_thread):
            t.join(timeout=1.0)
        raise

    stdin_thread.join(timeout=5.0)
    stdout_thread.join(timeout=5.0)
    stderr_thread.join(timeout=5.0)

    stdout_text = b"".join(stdout_chunks).decode("utf-8", errors="replace")
    stderr_text = b"".join(stderr_chunks).decode("utf-8", errors="replace")
    return subprocess.CompletedProcess(
        args=process.args,
        returncode=process.returncode,
        stdout=stdout_text,
        stderr=stderr_text,
    )


def run_kernel_cli(payload: Mapping[str, Any]) -> Dict[str, Any]:
    payload_text = json.dumps(payload)
    result: subprocess.CompletedProcess[str] | None = None
    for attempt in range(3):
        result = _run_kernel_cli_once(payload_text)
        if not (
            result.returncode == -15
            and not result.stdout.strip()
            and not result.stderr.strip()
        ):
            break
        if attempt < 2:
            time.sleep(1.0)
    assert result is not None
    stdout = result.stdout.strip()
    stderr = result.stderr.strip()
    try:
        data = json.loads(stdout) if stdout else {}
    except Exception as exc:
        raise KernelCliError(f"invalid kernel CLI response: {exc}") from exc
    if result.returncode == 0:
        if not isinstance(data, dict):
            raise KernelCliError("kernel CLI response must be a JSON object")
        return data
    message = ""
    if isinstance(data, dict):
        message = str(data.get("message", "") or data.get("error", "") or "").strip()
    if not message:
        message = stderr or stdout or f"kernel CLI exited with status {result.returncode}"
    raise KernelCliError(message)
