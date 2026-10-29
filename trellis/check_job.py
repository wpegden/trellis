#!/usr/bin/env python3
"""Standalone client for host-owned advisory jobs (also copied into bursts).

A disconnected client never restarts a compiler. Repeating the same command
attaches to the job for the same captured inputs. Infrastructure errors use
exit 2; Lean's diagnostics and exit status are preserved.
"""
from __future__ import annotations
import argparse
import json
import os
from pathlib import Path
import socket
import struct
import sys
import time
import uuid

SOCKET_ENV = "TRELLIS_CHECK_JOBS_SOCKET"
TOKEN_ENV = "TRELLIS_CHECK_JOBS_TOKEN"
TERMINAL = {"succeeded", "failed", "cancelled", "interrupted", "resource_exhausted", "timed_out", "infrastructure_error"}
MAX_FRAME = 32 * 1024 * 1024
SCRATCH_ROOTS = (".trellis/scratch", ".trellis/tmp/correspondence", ".trellis/stuck-math-audit")


class JobError(RuntimeError):
    pass


class JobTransportError(JobError):
    """The connection failed; the host may still have the same request/job."""


def input_path(target, kind):
    """Validate a workspace-relative advisory input; shared with the host."""
    roots = SCRATCH_ROOTS if kind == "scratch" else ("Tablet",)
    path = Path(target)
    if (path.is_absolute() or ".." in path.parts or not target.endswith(".lean")
            or not any(target.startswith(root + "/") for root in roots)):
        expected = ", ".join(root + "/*.lean" for root in roots)
        raise ValueError(f"input must be a relative {expected} file")
    return path.as_posix()


def relative_input(target):
    """The prompt may name an absolute probe under its `cd` workspace."""
    path = Path(target)
    if path.is_absolute():
        try:
            path = path.relative_to(Path.cwd())
        except ValueError:
            raise JobError("input must be inside the current workspace") from None
    return path.as_posix()


def configured():
    return bool(os.environ.get(SOCKET_ENV))


def _socket_budget(sock, deadline):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("checker connection timed out")
    sock.settimeout(remaining)


def _recv(sock, count, deadline):
    chunks = []
    while count:
        _socket_budget(sock, deadline)
        chunk = sock.recv(count)
        if not chunk:
            raise JobTransportError("checker connection closed; the job may still be running")
        chunks.append(chunk)
        count -= len(chunk)
    return b"".join(chunks)


def rpc(request, *, socket_path=None, token=None, timeout=30):
    # Host coordination passes the exact attempt channel. Avoid process-global
    # environment mutation when releasing several workers concurrently.
    socket_path = os.environ.get(SOCKET_ENV, "") if socket_path is None else socket_path
    path = Path(socket_path)
    token = os.environ.get(TOKEN_ENV, "") if token is None else token
    if not socket_path or not token:
        raise JobError("central checker is not configured for this workspace")
    request = dict(request, token=token)
    data = json.dumps(request).encode()
    directory_fd = None
    deadline = time.monotonic() + timeout
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            address = str(path)
            if len(os.fsencode(path)) >= 108:
                directory_fd = os.open(path.parent, os.O_PATH | os.O_DIRECTORY | os.O_CLOEXEC)
                address = f"/proc/self/fd/{directory_fd}/{path.name}"
            _socket_budget(sock, deadline)
            sock.connect(address)
            _socket_budget(sock, deadline)
            sock.sendall(struct.pack(">I", len(data)) + data)
            size = struct.unpack(">I", _recv(sock, 4, deadline))[0]
            if not 0 < size <= MAX_FRAME:
                raise JobError("invalid response length from checker")
            response = json.loads(_recv(sock, size, deadline))
    except OSError as exc:
        error_type = (JobTransportError if isinstance(exc, (ConnectionError, TimeoutError,
                      InterruptedError, FileNotFoundError)) else JobError)
        raise error_type(f"central checker unavailable: {exc}; reconnect to the same job, or repeat the same check command") from exc
    except ValueError as exc:
        raise JobError(f"invalid response from central checker: {exc}") from exc
    finally:
        if directory_fd is not None:
            os.close(directory_fd)
    if "error" in response:
        raise JobError(response["error"])
    return response


def wait(job, *, announce=False):
    job_id = job["job_id"]
    if announce and job["state"] not in TERMINAL:
        print(f"check-job: {job_id} ({job['state']}); waiting. If interrupted: .trellis/scripts/check-job wait {job_id}", file=sys.stderr, flush=True)
    while job["state"] not in TERMINAL:
        time.sleep(0.5)
        job = rpc({"action": "status", "job_id": job_id})
    return job


def check(kind, target, *, timeout=1800, announce=False):
    # A repeat invocation also deduplicates by capability + captured inputs;
    # explicit protocol retries can reuse this request ID after a lost ack.
    request = dict(action="submit", request_id=uuid.uuid4().hex,
                   kind=kind, target=target, timeout=timeout)
    job = rpc(request)
    return wait(job, announce=announce)


def tool_info(tool, argument):
    """Reconnect only read-only metadata, retaining the request and job IDs.

    A brief checker restart must not look like a different compiler identity.
    Ordinary compilation and terminal interrupted jobs are never retried here.
    """
    request = dict(action="submit", request_id=uuid.uuid4().hex,
                   kind="tool_info", target=tool + "/" + argument, timeout=30)
    reconnect_deadline = None
    transport_error = None
    while True:
        started = time.monotonic()
        remaining = 30 if reconnect_deadline is None else reconnect_deadline - started
        if remaining <= 0:
            raise JobError("central checker remained unavailable for 30 seconds during the tool metadata query; retry the check when it is available") from transport_error
        try:
            job = rpc(request, timeout=remaining)
        except JobTransportError as exc:
            transport_error = exc
            if reconnect_deadline is None:
                reconnect_deadline = started + 30
            remaining = reconnect_deadline - time.monotonic()
            time.sleep(min(0.25, max(0, remaining)))
            continue
        # Healthy admission waits can be longer than the reconnect allowance.
        # The existing worker deadline bounds them; each successful RPC starts
        # a fresh allowance if a later connection fails.
        reconnect_deadline = None
        transport_error = None
        if job["state"] in TERMINAL:
            return job
        request = dict(action="status", job_id=job["job_id"])
        time.sleep(0.5)


def render(job):
    result = job.get("result") or {}
    sys.stdout.write(result.get("stdout", ""))
    sys.stderr.write(result.get("stderr", ""))
    if job["state"] not in {"succeeded", "failed"}:
        print(f"check-job: {job['state']}: {result.get('message', 'check did not complete')}", file=sys.stderr)
        return 2
    if job.get("current_inputs_match") is False:
        print("check-job: workspace changed after submission; these diagnostics describe the captured inputs. Run the check again for the current files.", file=sys.stderr)
        return 2
    if not result.get("stdout") and not result.get("stderr"):
        print(f"check-job: {job['state']}", file=sys.stderr)
    return int(result["returncode"])


def provision(scripts):
    """Called only by the trusted launcher; all these files are RO in bursts."""
    import shutil
    scripts = Path(scripts)
    scripts.mkdir(parents=True, exist_ok=True)
    destination = scripts / "check_job.py"
    if destination.resolve() != Path(__file__).resolve():
        shutil.copy2(__file__, destination)
    (scripts / "check-job").write_text('#!/bin/sh\nexec python3 "$(dirname "$0")/check_job.py" "$@"\n')
    (scripts / "check-job").chmod(0o755)
    guard = scripts / "compiler-command-guard"
    guard.write_text("""#!/bin/sh
if [ "${TRELLIS_COMPILER_CAPABILITY:-}" = none ]; then
  echo 'This task has no Lean capability; report the need for Lean to the reviewer. Do not work around this guard.' >&2
else
  echo 'Use .trellis/scripts/incremental-check for node checks or .trellis/scripts/check-job for builds and scratch files. Do not invoke Lean/Lake directly or work around this guard.' >&2
fi
exit 2
""")
    guard.chmod(0o755)
    shims = scripts / "check-bin"
    shims.mkdir(exist_ok=True)
    for tool in ("lake", "lean"):
        path = shims / tool
        path.write_text(f'#!/bin/sh\nexec python3 "$(dirname "$0")/../check_job.py" --shim {tool} "$@"\n')
        path.chmod(0o755)


def shim(tool, args):
    """Route ordinary agent compiler commands into the same durable service."""
    # The runtime hashes Lean/Lake beneath this prefix during certification.
    # Forward only the exact read-only query, never arbitrary compiler flags.
    if args in (["--version"], ["--help"], ["-h"]) or (tool == "lean" and args == ["--print-prefix"]):
        if os.environ.get("TRELLIS_COMPILER_CAPABILITY") == "managed":
            return render(tool_info(tool, args[0]))
        import subprocess
        real = os.environ.get("TRELLIS_CHECK_JOBS_REAL_" + tool.upper())
        if not real:
            raise JobError("tool path is not configured")
        return subprocess.run([real, *args]).returncode
    if tool == "lake" and args and args[0] == "build" and len(args) > 1:
        for node in args[1:]:
            code = main(["build", node])
            if code:
                return code
        return 0
    if tool == "lake" and args[:2] == ["env", "lean"]:
        return shim("lean", args[2:])
    if tool == "lean" and len(args) == 1:
        path = relative_input(args[0])
        if any(path.startswith(root + "/") for root in SCRATCH_ROOTS):
            return main(["scratch", path])
        if path.startswith("Tablet/"):
            return main(["file", path])
    raise JobError("use `lake build Tablet.Node`, `lake env lean FILE.lean`, or `.trellis/scripts/check-job`; files must be in Tablet/ or " + ", ".join(SCRATCH_ROOTS))


def main(argv=None):
    if os.environ.get("TRELLIS_COMPILER_CAPABILITY") == "none":
        print("check-job: this task has no Lean capability; report the need for Lean to the reviewer.", file=sys.stderr)
        return 2
    argv = list(sys.argv[1:] if argv is None else argv)
    if argv[:1] == ["--shim"]:
        try:
            if len(argv) < 2 or argv[1] not in {"lake", "lean"}:
                raise JobError("expected a lake or lean command")
            return shim(argv[1], argv[2:])
        except (JobError, OSError) as exc:
            print(f"check-job: {exc}", file=sys.stderr)
            return 2
    parser = argparse.ArgumentParser(description="Run or reconnect to a managed advisory Lean check.")
    parser.add_argument("action", choices=["build", "incremental", "scratch", "file", "wait", "status", "cancel"])
    parser.add_argument("target", help="node, scratch file, or job ID")
    parser.add_argument("--timeout", type=int, default=1800)
    args = parser.parse_args(argv)
    try:
        if args.action in {"build", "incremental", "scratch", "file"}:
            if args.action in {"scratch", "file"}:
                args.target = input_path(relative_input(args.target), args.action)
            if not configured():
                if os.environ.get("TRELLIS_MANAGED_AGENT_CHECKS") == "1" or os.environ.get("TRELLIS_COMPILER_CAPABILITY") == "managed":
                    raise JobError("central checker is not configured; report this infrastructure problem in your response. Do not run Lean/Lake outside the supplied checker tools.")
                # Offline/rollback mode retains the existing burst-local
                # execution boundary. A configured but unavailable service
                # MUST NOT silently launch unmanaged fallback work.
                import subprocess
                try:
                    from trellis import incremental_check as ic
                except ImportError:
                    import incremental_check as ic
                safe, reason = ic._sandbox_markers()
                if not safe:
                    raise JobError(f"local check requires the agent sandbox: {reason}")
                if args.action in {"scratch", "file"}:
                    command = ["lake", "env", "lean", args.target]
                else:
                    node = ic._parse_target(args.target)
                    if args.action == "incremental":
                        return ic._run_incremental(Path.cwd(), node)
                    command = ["lake", "build", "Tablet." + node]
                print("check-job: central service is disabled; checking in the local sandbox.", file=sys.stderr)
                return subprocess.run(command, timeout=args.timeout).returncode
            return render(check(args.action, args.target, timeout=args.timeout, announce=True))
        job = rpc({"action": "cancel" if args.action == "cancel" else "status", "job_id": args.target})
        if args.action == "wait":
            return render(wait(job, announce=True))
        print(json.dumps(job, indent=2))
        return 0
    except (JobError, ValueError, OSError) as exc:
        print(f"check-job: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
