"""Durable evidence for an initial supervisor that pauses before a liveness poll."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import time

HALT_MARKERS = ("runtime_error_halt.json", "checker_disagreement_halt.json", "system_feedback_halt.json")


def capture_launch_environment(runtime: Path, environment: dict[str, str], trellis_root: Path,
                               *, tmux_session: str | None = None,
                               trellis_head: str | None = None,
                               max_steps: str | None = None, cwd: str | None = None) -> None:
    """Record the complete nonsecret launch recipe, including for unlaunched holds."""
    env = {}
    for name, value in environment.items():
        if name.startswith("TRELLIS_LAUNCH_") and name != "TRELLIS_LAUNCH_ATTEMPT":
            continue
        if any(marker in name.upper() for marker in ("TOKEN", "KEY", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL", "AUTH")):
            continue
        if name.startswith(("TRELLIS_", "LEAN_", "LAKE_", "ELAN_", "RUST_", "CARGO_")) or name in {
                "PATH", "HOME", "LANG", "LC_ALL", "SHELL", "USER", "LOGNAME", "PYTHONPATH"}:
            env[name] = value
    payload = {"captured_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
               "captured_at_epoch": int(time.time()), "runtime_root": str(runtime.resolve()),
               "trellis_root": str(trellis_root), "trellis_sh": str(trellis_root / "scripts/trellis.sh"),
               "max_steps": max_steps, "trellis_head": trellis_head,
               "cwd": cwd or os.getcwd(), "tmux_socket": environment.get("TRELLIS_TMUX_SOCKET", "trellis"),
               "tmux_session": tmux_session, "env": env}
    executable = Path(env.get("TRELLIS_TRELLIS_KERNEL_CMD", ""))
    if executable.is_absolute() and executable.is_file():
        payload["runtime_cli_sha256"] = hashlib.sha256(executable.read_bytes()).hexdigest()
    _write_json(runtime / "launch_env.json", payload)


def _write_json(path: Path, value: dict) -> None:
    temporary = path.with_name(f"{path.name}.tmp-{os.getpid()}")
    with temporary.open("w") as stream:
        json.dump(value, stream)
        stream.flush()
        os.fsync(stream.fileno())
    temporary.replace(path)
    descriptor = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def arm_initial_launch(runtime: Path, repo: Path, runtime_cli: str | None = None) -> str:
    """Persist a fresh attempt before spawning; never clear a previous pause."""
    attempt = secrets.token_hex(24)
    receipt = {"version": 1, "attempt": attempt, "runtime_root": str(runtime.resolve()),
               "repo_path": str(repo.resolve())}
    if runtime_cli:
        executable = Path(runtime_cli).resolve(strict=True)
        receipt.update(runtime_cli=str(executable),
                       runtime_cli_sha256=hashlib.sha256(executable.read_bytes()).hexdigest())
    _write_json(runtime / "initial-launch.json", receipt)
    return attempt


def initial_budget_pause(runtime: Path, repo: Path, runtime_cli: str | None = None) -> bool:
    """Accept only this launch's kernel-written pause for the retained request.

    Callers still distinguish a live supervisor from a stopped one. A receipt
    allows a stopped process to prove its launch without observing a live PID;
    its nonce must cross both trellis.sh's env capture and the actual kernel.
    """
    try:
        if any((runtime / name).exists() for name in HALT_MARKERS):
            return False
        receipt, pause, state, metadata, launch = (
            json.loads((runtime / name).read_text()) for name in (
                "initial-launch.json", "pause_request.json", "protocol_state.json",
                "runtime_metadata.json", "launch_env.json"))
        request = state["in_flight_request"]
        detail, env = pause["detail"], launch["env"]
        same_path = lambda value, path: isinstance(value, str) and Path(value).is_absolute() and Path(value).resolve() == path.resolve()
        if (receipt["version"] != 1 or not receipt["attempt"]
                or pause["kind"] != "provider_budget" or pause["armed_by"] != "supervisor"
                or pause["launch_attempt"] != receipt["attempt"]
                or env["TRELLIS_LAUNCH_ATTEMPT"] != receipt["attempt"]
                or not pause["reason"] or not detail["provider"]
                or pause["armed_at_cycle"] != state["cycle"]
                or request["cycle"] != state["cycle"]
                or detail["request_id"] != request["id"]
                or detail["request_kind"] != request["kind"]
                or not same_path(receipt["repo_path"], repo)
                or not same_path(metadata["repo_path"], repo)
                or not all(same_path(value, runtime) for value in (
                    receipt["runtime_root"], pause["runtime_root"], launch["runtime_root"],
                    env["TRELLIS_KERNEL_CACHE_ROOT"]))
                or not same_path(env["TRELLIS_CHECKER_SOCKET"], runtime / "sockets/checker.sock")
                or not Path(launch["trellis_sh"]).is_file()):
            return False
        executable = receipt.get("runtime_cli")
        if runtime_cli and not same_path(executable, Path(runtime_cli)):
            return False
        if executable and (not same_path(pause["supervisor_executable"], Path(executable))
                or hashlib.sha256(Path(executable).read_bytes()).hexdigest() != receipt["runtime_cli_sha256"]):
            return False
        return True
    except (OSError, ValueError, KeyError, TypeError):
        return False


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["arm", "paused"])
    parser.add_argument("runtime", type=Path)
    parser.add_argument("repo", type=Path)
    parser.add_argument("--runtime-cli")
    args = parser.parse_args()
    if args.action == "arm":
        print(arm_initial_launch(args.runtime, args.repo, args.runtime_cli))
        return 0
    return 0 if initial_budget_pause(args.runtime, args.repo, args.runtime_cli) else 1


if __name__ == "__main__":
    raise SystemExit(main())
