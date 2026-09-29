"""Fast-exiting supervisor fixture for real create/tmux control-flow tests.

The actual kernel pause wire contract is tested in kernel/tests/runtime_cli.rs.
This fixture replaces provider dispatch, never the launch verification code.
"""
import json
import os
from pathlib import Path
import sys
import time

from trellis.launch_verification import capture_launch_environment


def write_pause(runtime, repo, executable, *, delay=0, failure=None, capture=True):
    time.sleep(delay)
    runtime, repo = Path(runtime), Path(repo)
    if failure == "no_runtime":
        return
    def write(name, value):
        (runtime / name).write_text(json.dumps(value))
    request = {"id": 42, "cycle": 7, "kind": "Worker"}
    state_path = runtime / "protocol_state.json"
    # Preserve valid pending state on ordinary Resume; initialize only the
    # intentionally minimal setup collaborator's placeholder.
    state = json.loads(state_path.read_text()) if state_path.exists() else {}
    if state.get("in_flight_request") != request:
        write("protocol_state.json", {"cycle": 7, "stage": "Worker", "in_flight_request": request,
                                      "request_seq": 42, "reviewer_decisions": ["retained"]})
    write("runtime_metadata.json", {"repo_path": str(repo)})
    env = {**os.environ, "TRELLIS_KERNEL_CACHE_ROOT": str(runtime),
           "TRELLIS_CHECKER_SOCKET": str(runtime / "sockets/checker.sock"),
           "TRELLIS_TRELLIS_KERNEL_CMD": str(executable)}
    if capture:
        capture_launch_environment(runtime, env, Path(__file__).resolve().parents[1],
                                   tmux_session="trellis-run-" + repo.name)
    pause = {"kind": "provider_budget", "armed_by": "supervisor", "armed_at_cycle": 7,
             "reason": "allowance exhausted; resets tomorrow", "detail": {
                 "provider": "test", "request_id": 42, "request_kind": "Worker"},
             "launch_attempt": os.environ.get("TRELLIS_LAUNCH_ATTEMPT"),
             "runtime_root": str(runtime.resolve()), "supervisor_executable": str(Path(executable).resolve())}
    if failure == "stale":
        pause["launch_attempt"] = "previous launch"
    if failure == "halt":
        write("runtime_error_halt.json", {"kind": "runtime_error", "error": "real failure"})
    write("pause_request.json", pause)


if __name__ == "__main__":
    write_pause(*sys.argv[1:4], delay=float(os.environ.get("FIRST_PAUSE_DELAY", "0")),
                failure=os.environ.get("FIRST_PAUSE_FAILURE"))
