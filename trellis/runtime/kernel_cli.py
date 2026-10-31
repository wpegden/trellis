from __future__ import annotations

import json
import ctypes
from contextlib import contextmanager, nullcontext
import gzip
import hashlib
import os
import signal
import stat
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

# These requests carry protocol/source data, not provider launch environments
# or credentials. Other actions receive metadata-only crash diagnostics.
_FAILURE_INPUT_ACTIONS = frozenset({
    "dispatch_control", "dispatch_routing", "dispatch_acceptance_state",
    "dispatch_observe_sources", "dispatch_verifier_inputs", "plan_staged_worker",
})
_FAILURE_INPUT_LIMIT = 512 * 1024 * 1024
_FAILURE_TAIL_LIMIT = 64 * 1024
_NATIVE_CRASH_CODES = frozenset(-s for s in (
    signal.SIGABRT, signal.SIGSEGV, signal.SIGBUS, signal.SIGILL,
))


def _retain_native_failure(
    payload: bytes, pid: int, returncode: int, stderr: bytes,
    progress_path: Optional[str],
) -> str:
    """Best-effort, host-local evidence after a native crash; never a retry.

    The receipt is written last. A disk/refusal error must not replace the
    native error. No command environment or provider/account files are read.
    """
    digest = hashlib.sha256(payload).hexdigest()
    detail = f"native pid={pid} returncode={returncode} input_sha256={digest}"
    try:
        request = json.loads(payload)
        action = request.get("action") if isinstance(request, dict) else None
        if not isinstance(action, str) or len(action) > 128:
            action = None
        detail += f" action={action!r}"
        base = Path.home() / ".local/state/trellis/kernel-failures"
        base.mkdir(mode=0o700, parents=True, exist_ok=True)
        info = base.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise OSError("native failure directory is not private and owned")
        directory = Path(tempfile.mkdtemp(prefix=f"{time.time_ns()}-{pid}-", dir=base))

        def write(name: str, content: bytes) -> None:
            fd = os.open(directory / name, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "wb") as stream:
                stream.write(content)
                stream.flush()
                os.fsync(stream.fileno())

        receipt = dict(schema=1, child_pid=pid, returncode=returncode,
                       observed_at_ns=time.time_ns(), action=action,
                       input_sha256=digest, input_bytes=len(payload),
                       input_retained=False, stderr_bytes=len(stderr))
        if action in _FAILURE_INPUT_ACTIONS and len(payload) <= _FAILURE_INPUT_LIMIT:
            compressed = gzip.compress(payload, compresslevel=1, mtime=0)
            write("input.json.gz", compressed)
            receipt.update(input_retained=True, input_archive_sha256=hashlib.sha256(compressed).hexdigest())
        write("stderr.tail", stderr[-_FAILURE_TAIL_LIMIT:])
        if progress_path:
            try:
                with open(progress_path, "rb") as stream:
                    stream.seek(0, os.SEEK_END)
                    size = stream.tell()
                    stream.seek(max(0, size - _FAILURE_TAIL_LIMIT))
                    tail = stream.read(_FAILURE_TAIL_LIMIT)
                write("progress.tail", tail)
                receipt["progress_bytes"] = size
            except OSError:
                receipt["progress_unavailable"] = True
        write("receipt.json", (json.dumps(receipt, sort_keys=True, indent=2) + "\n").encode())
        for path in (directory, base):
            fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
        return f"{detail} receipt={directory / 'receipt.json'}"
    except Exception as exc:
        return f"{detail} diagnostic_unavailable={type(exc).__name__}"


def _setup_progress_tail(*, on_setup=None) -> Tuple[Optional[Path], Dict[str, str], Optional[threading.Event], Optional[threading.Thread]]:
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
    if on_setup is not None:
        on_setup(path, stop_event, None)

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
    if on_setup is not None:
        on_setup(path, stop_event, thread)
    thread.start()
    return path, {_PROGRESS_LOG_ENV: str(path)}, stop_event, thread


def _teardown_progress_tail(
    path: Optional[Path],
    stop_event: Optional[threading.Event],
    thread: Optional[threading.Thread],
) -> None:
    if stop_event is not None:
        stop_event.set()
    if thread is not None and thread.ident is not None:
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


@contextmanager
def _kernel_child_scope():
    """Reserve the calling process's child tree for one terminal native call."""
    tasks = list(Path('/proc/self/task').iterdir())
    if (threading.current_thread() is not threading.main_thread() or len(tasks) != 1
            or any((task / 'children').read_text().strip() for task in tasks)):
        raise KernelCliError('Owned kernel invocation requires exclusive child ownership')
    libc = ctypes.CDLL(None)
    previous = ctypes.c_int()
    if libc.prctl(37, ctypes.byref(previous), 0, 0, 0) != 0:
        raise KernelCliError('Owned kernel descendant ownership unavailable')
    closed = failed = False

    def drain():
        nonlocal closed, failed
        if failed:
            raise KernelCliError('Owned kernel cleanup uncertain')
        if closed:
            return
        from trellis.dispatch.process import reap_descendants
        mask = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM})
        try:
            reap_descendants()
            closed = True
        except BaseException:
            failed = True
            raise KernelCliError('Owned kernel cleanup uncertain') from None
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, mask)

    try:
        if libc.prctl(36, 1, 0, 0, 0) != 0:
            raise KernelCliError('Owned kernel descendant ownership unavailable')
        yield drain
    finally:
        original = sys.exc_info()[1]
        cleanup_error = None
        try:
            drain()
        except BaseException as error:
            cleanup_error = error
        if libc.prctl(36, previous.value, 0, 0, 0) != 0:
            cleanup_error = KernelCliError('Owned kernel cleanup uncertain')
        if original is not None:
            original.kernel_quiescent = closed and cleanup_error is None and getattr(original, 'kernel_quiescent', True)
            if cleanup_error is not None:
                original.add_note('Owned kernel cleanup uncertain')
        elif cleanup_error is not None:
            cleanup_error.kernel_quiescent = False
            raise cleanup_error


def _run_kernel_cli_once(payload_text: str, *, owned=False, pass_fds=(), timeout_seconds=None) -> subprocess.CompletedProcess[str]:
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
    with _kernel_child_scope() if owned else nullcontext(None) as drain:
        progress_path = progress_stop = progress_thread = None
        def remember_progress(path, stop, thread):
            nonlocal progress_path, progress_stop, progress_thread
            progress_path, progress_stop, progress_thread = path, stop, thread
        try:
            progress_path, progress_env, progress_stop, progress_thread = (
                _setup_progress_tail(on_setup=remember_progress) if owned else _setup_progress_tail())
            if owned:
                return _run_kernel_cli_inner(payload_text, progress_env, owned=True,
                    pass_fds=pass_fds, timeout_seconds=timeout_seconds, drain=drain)
            return _run_kernel_cli_inner(payload_text, progress_env)
        finally:
            original = sys.exc_info()[1]
            try:
                if owned:
                    drain()
                _teardown_progress_tail(progress_path, progress_stop, progress_thread)
                if owned and progress_thread is not None and progress_thread.is_alive():
                    raise KernelCliError('Owned kernel progress cleanup uncertain')
            except BaseException as error:
                if original is None:
                    error.kernel_quiescent = False
                    raise
                original.kernel_quiescent = False
                original.add_note('Owned kernel progress cleanup uncertain')


def _run_kernel_cli_inner(
    payload_text: str,
    progress_env: Mapping[str, str],
    *, owned=False, pass_fds=(), timeout_seconds=None, drain=None,
) -> subprocess.CompletedProcess[str]:
    if progress_env:
        env = os.environ.copy()
        env.update(progress_env)
    else:
        env = None
    from trellis.pv_execution import owned_process
    command = kernel_cli_command()
    arguments = dict(stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    manager = (owned_process(command, pass_fds=pass_fds, **arguments) if owned else
               nullcontext(subprocess.Popen(command, **arguments)))
    threads = []
    try:
        with manager as process:
            return _collect_kernel_cli(process, payload_text, progress_env, threads,
                                       owned=owned, timeout_seconds=timeout_seconds, drain=drain)
    finally:
        if owned:
            original = sys.exc_info()[1]
            try:
                drain()
                for thread in threads:
                    if thread.ident is not None:
                        thread.join(timeout=5.0)
                if any(thread.is_alive() for thread in threads):
                    raise KernelCliError('Owned kernel stream cleanup uncertain')
            except BaseException as error:
                if original is None:
                    error.kernel_quiescent = False
                    raise
                original.kernel_quiescent = False
                original.add_note('Owned kernel stream cleanup uncertain')


def _collect_kernel_cli(process, payload_text, progress_env, threads, *, owned=False, timeout_seconds=None, drain=None):
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
    threads.extend((stdin_thread, stdout_thread, stderr_thread))
    stderr_thread.start()
    stdout_thread.start()
    stdin_thread.start()

    try:
        process.wait(timeout=timeout_seconds) if owned else process.wait()
    except Exception:
        # Best-effort cleanup so we don't leak a child on unexpected
        # exceptions in the parent (KeyboardInterrupt, etc.).
        if not owned:
            process.kill()
            for t in (stdin_thread, stdout_thread, stderr_thread):
                t.join(timeout=1.0)
        raise

    if owned:
        # Drain escaped helpers before waiting for their inherited pipe ends.
        drain()

    stdin_thread.join(timeout=5.0)
    stdout_thread.join(timeout=5.0)
    stderr_thread.join(timeout=5.0)

    stdout_text = b"".join(stdout_chunks).decode("utf-8", errors="replace")
    stderr_bytes = b"".join(stderr_chunks)
    stderr_text = stderr_bytes.decode("utf-8", errors="replace")
    if process.returncode in _NATIVE_CRASH_CODES:
        diagnostic = _retain_native_failure(payload_bytes, process.pid, process.returncode,
                                            stderr_bytes, progress_env.get(_PROGRESS_LOG_ENV))
        stderr_text += f"\n[{diagnostic}]\n"
    return subprocess.CompletedProcess(
        args=process.args,
        returncode=process.returncode,
        stdout=stdout_text,
        stderr=stderr_text,
    )


def run_kernel_cli(payload: Mapping[str, Any], *, owned=False, pass_fds=(), timeout_seconds=None) -> Dict[str, Any]:
    if owned:
        from trellis.pv_execution import positive_timeout
        timeout_seconds = positive_timeout(timeout_seconds)
    elif pass_fds or timeout_seconds is not None:
        raise KernelCliError('Descriptor forwarding requires an owned kernel invocation')
    payload_text = json.dumps(payload)
    result: subprocess.CompletedProcess[str] | None = None
    for attempt in range(1 if owned else 3):
        result = (_run_kernel_cli_once(payload_text, owned=True, pass_fds=pass_fds,
                                      timeout_seconds=timeout_seconds) if owned else
                  _run_kernel_cli_once(payload_text))
        if not (
            result.returncode == -15
            and not result.stdout.strip()
            and not result.stderr.strip()
        ):
            break
        if not owned and attempt < 2:
            time.sleep(1.0)
    assert result is not None
    try:
        return _kernel_cli_result(result)
    except BaseException as error:
        if owned:
            error.kernel_quiescent = True
        raise


def _kernel_cli_result(result):
    stdout = result.stdout.strip()
    stderr = result.stderr.strip()
    try:
        data = json.loads(stdout) if stdout else {}
    except Exception as exc:
        detail = f"\n{stderr}" if result.returncode in _NATIVE_CRASH_CODES else ""
        raise KernelCliError(f"invalid kernel CLI response: {exc}{detail}") from exc
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
