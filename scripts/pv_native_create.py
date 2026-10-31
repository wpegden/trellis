#!/usr/bin/env python3
"""Native modeling handoff for a durable viewer creation job.

Only the operator supplies tools and launch configuration. Uploaded inputs
remain a structured goal collection and unmodified source archives.
"""
from __future__ import annotations

import argparse
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time
import signal
from contextlib import contextmanager

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from scripts.adapt_pv_source import materialize_partitioned_source_tree
from scripts.pv_archive import extract_archive
from trellis.json_io import save_json
from trellis.modeling.goals import read_goal_collection, verify_inventory
from trellis.modeling.driver import check_halt
from trellis.modeling.native import failed_bootstrap_output
from trellis.modeling.reopening import capture, initialize_reopened, read_disposition


def active_paths(job_dir: Path, job: dict) -> tuple[Path, Path, Path]:
    generation = int(job.get("native_generation", 0))
    output = job_dir / "native"
    name = job["slug"]
    if generation:
        output = output / "generations" / f"{generation:04d}"
        name += f"-r{generation:04d}"
    return output / f"{name}-preparation", output / name, Path(job["runtime_root"])


def active_goal_file(job_dir: Path, job: dict) -> Path:
    """A ratified revision changes the active collection, never the upload."""
    return Path(job.get("goal_collection_path", job_dir / "GOAL.json"))


def active_config_file(job_dir: Path, job: dict) -> Path:
    return Path(job.get("config_template_path", job_dir / "config-template.json"))


@contextmanager
def job_lock(job_dir: Path):
    with (job_dir / "native-controller.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def runtime_job(runtime: Path) -> tuple[Path, dict, Path] | None:
    metadata = runtime / "runtime_metadata.json"
    if not metadata.exists():
        return None
    repo = Path(json.loads(metadata.read_text())["repo_path"]).resolve()
    if not (repo / "modeling-input/manifest.json").is_file():
        return None
    for parent in repo.parents:
        path = parent / "job.json"
        if path.is_file():
            job = json.loads(path.read_text())
            if job.get("goal_format") == "native" and repo.is_relative_to(parent / "native"):
                return parent, job, repo
    return None


def reopening_status(runtime: Path) -> dict | None:
    end = read_disposition(runtime)
    if not end:
        return None
    owner = runtime_job(runtime)
    active = owner and active_paths(owner[0], owner[1])[2].resolve() == runtime.resolve()
    pending = owner and owner[1].get("reopening", {})
    owned = active or (pending and pending.get("pending") and pending["record"]["runtime"] == str(runtime))
    return {**end, "recoverable": bool(owned and end["action"] == "reopen"),
            "job": str(owner[0]) if owner else None}


def validate_inputs(goals: Path, tools_file: Path) -> dict:
    _, collection = read_goal_collection(goals)
    tools = json.loads(tools_file.read_text())
    if not isinstance(tools, dict):
        raise ValueError("Native modeling tools must be an object")
    for key in ("cargo", "charon", "aeneas", "lean"):
        value = tools.get(key)
        if not isinstance(value, str) or not Path(value).is_absolute() or not os.access(value, os.X_OK):
            raise ValueError(f"Native modeling needs an installed {key} executable in the operator tools catalog")
    from trellis.pv_toolchain import native_resources
    native_resources(tools)
    return collection



def publish_alias(alias: Path, destination: Path, *, previous: Path | None = None) -> None:
    if previous is None:
        # symlink creation claims the public path without overwriting anything.
        alias.symlink_to(destination, target_is_directory=True)
        return
    if not alias.is_symlink() or alias.resolve() != previous.resolve():
        raise ValueError("The viewer project alias changed during preparation")
    temporary = alias.with_name(alias.name + f".handoff-{os.getpid()}")
    temporary.symlink_to(destination, target_is_directory=True)
    try:
        temporary.replace(alias)
    finally:
        temporary.unlink(missing_ok=True)


def validate_resume(job_dir: Path) -> None:
    """Check durable preparation ownership and stop boundaries before resuming."""
    job = json.loads((job_dir / "job.json").read_text())
    if job.get("goal_format") != "native":
        raise ValueError("Only native preparation supports resume")
    root, native, runtime = active_paths(job_dir, job)
    if job.get("revision", {}).get("pending"):
        # Validation is handled by the revision controller, which checks the
        # exact human receipt before it can publish or resume this generation.
        from trellis.pv_revision.driver import Driver
        driver = Driver(Path(job["revision"]["root"]))
        driver._current()
        return
    if job.get("reopening", {}).get("pending"):
        validate_reopening(job_dir, job)
        return
    state = json.loads((root / ".trellis/modeling/state.json").read_text())
    status = state.get("resume_status") if state.get("status") == "halted" else state.get("status")
    if state.get("verification_protocol") != "native-v1" or status not in {"active", "prepared"}:
        raise ValueError("Preparation is not resumable")
    alias = Path(job["repo"])
    if not alias.is_symlink() or alias.resolve() != root.resolve():
        raise ValueError("The viewer project alias no longer belongs to this preparation")
    if (root / "goals.json").read_bytes() != active_goal_file(job_dir, job).read_bytes():
        raise ValueError("Preparation goals differ from the original upload")
    config = json.loads((root / "config.json").read_text())
    if config.get("launch_config") != json.loads(active_config_file(job_dir, job).read_text()):
        raise ValueError("Preparation launch settings differ from the saved job")
    check_halt(root)
    # The run command acquires this lock again for the full driver lifetime.
    with (root / ".trellis/modeling/runner.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise ValueError("Preparation already has an active driver") from None
    failed_bootstrap_output(root, native, runtime)


def run_job(job_dir: Path, *, resume: bool = False) -> None:
    with job_lock(job_dir):
        if resume:
            (job_dir / "native-controller.pause").unlink(missing_ok=True)
        _run_job(job_dir, resume=resume)


def _run_job(job_dir: Path, *, resume: bool = False) -> None:
    job = json.loads((job_dir / "job.json").read_text())
    if job.get("goal_format") != "native":
        raise ValueError("This job is not a native goal-collection launch")
    slug = job["slug"]
    output = job_dir / "native"
    preparation, native, runtime = active_paths(job_dir, job)
    alias = Path(job["repo"])
    if resume:
        if job.get("revision", {}).get("pending"):
            from trellis.pv_revision.lifecycle import complete_successor
            complete_successor(job_dir, job)
            return
        if job.get("reopening", {}).get("pending"):
            complete_reopening(job_dir, job)
            return
        validate_resume(job_dir)
        launch_preparation(job_dir, job, preparation, native, alias, runtime)
        return
    if any(p.exists() or p.is_symlink() for p in (preparation, native, alias, runtime)):
        raise ValueError("Native creation requires fresh preparation, project, and runtime paths")
    validate_inputs(job_dir / "GOAL.json", job_dir / "native-tools.json")
    output.mkdir()
    limit = int(os.environ.get("TRELLIS_CREATE_UPLOAD_LIMIT_BYTES", 20971520))

    def unpack(name: str) -> Path:
        destination = output / ("unpacked-" + name)
        metadata = extract_archive(str(job_dir / (name + ".tar.gz")), destination, limit)
        return destination / str(metadata.get("source_root", ""))

    target = unpack("target")
    support = unpack("support") if (job_dir / "support.tar.gz").exists() else None
    source = output / "source"
    _, tree, partition = materialize_partitioned_source_tree(target, support, source)
    save_json(job_dir / "SOURCE_TREE_MANIFEST.json", tree)
    save_json(job_dir / "SOURCE_PARTITION_MANIFEST.json", partition)
    command = [sys.executable, str(ROOT / "scripts/pv_modeling.py")]
    subprocess.run([*command, "init", str(preparation), "--source", str(source),
                    "--goals", str(job_dir / "GOAL.json"), "--tools", str(job_dir / "native-tools.json"),
                    "--config-template", str(job_dir / "config-template.json")], check=True)
    publish_alias(alias, preparation)
    (preparation / ".trellis-creating").touch()
    launch_preparation(job_dir, job, preparation, native, alias, runtime)


def launch_preparation(job_dir: Path, job: dict, preparation: Path, native: Path,
                       alias: Path, runtime: Path) -> None:
    command = [sys.executable, str(ROOT / "scripts/pv_modeling.py")]
    slug = job["slug"]
    print(f"Native preparation is available in the {slug} run viewer.", flush=True)
    subprocess.run([*command, "run", str(preparation), "--repo", str(native),
                    "--runtime", str(runtime), "--no-viewer",
                    "--stop-file", str(job_dir / "native-controller.pause")], check=True)
    state = json.loads((preparation / ".trellis/modeling/state.json").read_text())
    if state.get("status") != "prepared" or state.get("native_repo") != str(native):
        raise ValueError(f"Native preparation did not launch a campaign: {state.get('status')}; {state.get('feedback', '')}")
    inventory = verify_inventory(preparation, native)
    save_json(job_dir / "goal-inventory.json", inventory)
    if not (runtime / "protocol_state.json").is_file():
        raise ValueError("Native launch did not produce runtime state")
    publish_alias(alias, native, previous=preparation)
    (preparation / ".trellis-creating").unlink(missing_ok=True)
    print(f"Native modeling launched with {len(inventory['registered'])} independent targets.", flush=True)


def stop_job(job_dir: Path) -> None:
    (job_dir / "native-controller.pause").touch()
    job = json.loads((job_dir / "job.json").read_text())
    preparation, _, _ = active_paths(job_dir, job)
    if (preparation / ".trellis/modeling/state.json").exists():
        # The shared stop command pins the driver identity and cancels only
        # its current preparation request, including its separate tmux burst.
        subprocess.run([sys.executable, str(ROOT / "scripts/pv_modeling.py"),
                        "stop", str(preparation)], check=True)


def startup_ready(job_dir: Path) -> bool:
    job = json.loads((job_dir / "job.json").read_text())
    runtime = Path(job["runtime_root"])
    failure = runtime / "runtime_error_halt.json"
    if failure.is_file():
        detail = json.loads(failure.read_text()).get("error", "unknown runtime error")
        raise ValueError(f"Native runtime failed during startup: {detail}")
    protocol = runtime / "protocol_state.json"
    if not protocol.is_file():
        return False
    state = json.loads(protocol.read_text())
    # A completed generation can already be parked in revision human review.
    # There is no next native dispatch to wait for on that predecessor.
    # Parallel adoption moves the ordinary request into its durable task;
    # there need not be a top-level reviewer request while that worker runs.
    dispatched = bool((state.get("dispatch") or {}).get("tasks"))
    return (bool(state.get("in_flight_request")) or dispatched or bool(read_disposition(runtime))
            or bool(state.get("pv_revision_handoff"))) and state.get("cycle", 0) > 0


def write_reopening_status(job_dir: Path, state: str, error: str = "") -> None:
    path = job_dir / "status.json"
    status = json.loads(path.read_text()) if path.exists() else {}
    status.update(state=state, stage="preparation" if state != "done" else "done",
                  phase="genesis", updated_ts=int(time.time()), error=error)
    save_json(path, status)


@contextmanager
def reopening_heartbeat(job_dir: Path):
    stopped = threading.Event()
    def beat():
        while not stopped.wait(10):
            write_reopening_status(job_dir, "resolving")
    write_reopening_status(job_dir, "resolving")
    thread = threading.Thread(target=beat, daemon=True)
    thread.start()
    try:
        yield
    finally:
        stopped.set()
        thread.join()


def validate_reopening(job_dir: Path, job: dict) -> None:
    """Check predecessor identity and child stop boundaries without starting work."""
    record = job["reopening"]["record"]
    preparation, native, runtime = active_paths(job_dir, job)
    alias = Path(job["repo"])
    parent_repo = Path(record["repo"])
    if not alias.is_symlink() or alias.resolve() not in {parent_repo.resolve(), preparation.resolve(), native.resolve()}:
        raise ValueError("The viewer project alias changed during preparation reopening")
    old = Path(record["preparation"])
    current = capture(old, parent_repo, Path(record["runtime"]), resume=True)
    if current != record:
        raise ValueError("The recorded reopening predecessor has changed")
    if (old / "goals.json").read_bytes() != active_goal_file(job_dir, job).read_bytes():
        raise ValueError("Preparation goals differ from the original upload")
    if json.loads((old / "config.json").read_text()).get("launch_config") != json.loads(active_config_file(job_dir, job).read_text()):
        raise ValueError("Preparation launch settings differ from the saved job")
    if (preparation / ".trellis/modeling/reopened-from.json").exists():
        if (preparation / "goals.json").read_bytes() != active_goal_file(job_dir, job).read_bytes():
            raise ValueError("Reopened preparation goals differ from the original upload")
        if json.loads((preparation / "config.json").read_text()).get("launch_config") != json.loads(active_config_file(job_dir, job).read_text()):
            raise ValueError("Reopened preparation launch settings differ from the saved job")
    check_halt(preparation)
    from trellis.runtime.bridge import any_halt_marker_present
    if any_halt_marker_present(runtime):
        raise ValueError("Resolve the reopened runtime halt before continuing")


def runtime_running(repo: Path, runtime: Path) -> bool:
    result = subprocess.run([str(ROOT / "scripts/trellis_pause.sh"), "status", str(runtime), str(repo)],
                            capture_output=True, text=True, check=True)
    return bool(json.loads(result.stdout).get("wrapper_pid"))


def finish_native_launch(job_dir: Path, preparation: Path, native: Path, runtime: Path) -> None:
    """Finish only a sealed bootstrap, using the normal runtime launcher."""
    from trellis.modeling.native import export_directory
    receipt = json.loads((export_directory(preparation, native) / "prepared.json").read_text())
    if receipt.get("repo") != str(native) or receipt.get("runtime") != str(runtime):
        raise ValueError("The completed native bootstrap receipt names different output")
    save_json(job_dir / "goal-inventory.json", verify_inventory(preparation, native))
    # A published child is a normal native runtime. Recovery cannot override a
    # subsequent operator pause, halt, or audited terminal outcome.
    if (runtime / "pause_request.json").exists() or (native / ".trellis-stop-after-checkpoint").exists():
        raise ValueError("The reopened runtime has a stop request; use its runtime controls")
    if not runtime_running(native, runtime) and not read_disposition(runtime):
        subprocess.run([str(ROOT / "scripts/restart_configured_run.sh"),
                        "--no-reset", "--no-current", "--no-viewer",
                        str(native / "trellis.config.json"), str(runtime)],
                       cwd=ROOT, check=True, env={**os.environ, **receipt["launch_environment"]})
    state_path = preparation / ".trellis/modeling/state.json"
    state = json.loads(state_path.read_text())
    state.update(status="prepared", native_repo=str(native), native_runtime=str(runtime))
    state.pop("host_failure", None)
    save_json(state_path, state)


def complete_reopening(job_dir: Path, job: dict) -> None:
    """Retry the same generation after interruption; never manufacture another."""
    def check_controller_pause():
        if (job_dir / "native-controller.pause").exists():
            raise RuntimeError("Preparation controller stopped by operator; explicit Resume is required")
    check_controller_pause()
    validate_reopening(job_dir, job)
    record = job["reopening"]["record"]
    preparation, native, runtime = active_paths(job_dir, job)
    alias = Path(job["repo"])
    parent_repo = Path(record["repo"])
    # Pausing a published child still uses the existing preparation driver.
    # Explicit resume continues it; an automatic parent callback cannot do so.
    with reopening_heartbeat(job_dir):
        initialize_reopened(preparation, record)
        check_halt(preparation)
        from trellis.runtime.bridge import any_halt_marker_present
        if any_halt_marker_present(runtime):
            raise ValueError("Resolve the reopened runtime halt before continuing")
        if alias.resolve() == parent_repo.resolve():
            publish_alias(alias, preparation, previous=parent_repo)
        from trellis.modeling.native import export_directory
        ready = export_directory(preparation, native) / "prepared.json"
        check_controller_pause()
        if ready.is_file() and (runtime / "protocol_state.json").is_file():
            finish_native_launch(job_dir, preparation, native, runtime)
            if alias.resolve() == preparation.resolve():
                publish_alias(alias, native, previous=preparation)
        elif alias.resolve() != native.resolve():
            (preparation / ".trellis-creating").touch()
            launch_preparation(job_dir, job, preparation, native, alias, runtime)
        else:
            raise ValueError("Published native generation has no sealed bootstrap receipt")
        (preparation / ".trellis-creating").unlink(missing_ok=True)
        # An interrupted publication after native launch must not initialize it again.
        while not startup_ready(job_dir):
            check_controller_pause()
            result = subprocess.run(["tmux", "-L", os.environ.get("TRELLIS_TMUX_SOCKET", "trellis"),
                                     "has-session", "-t", f"trellis-run-{native.name}"],
                                    capture_output=True)
            if result.returncode:
                raise RuntimeError("Reopened native runtime exited before its first dispatch")
            time.sleep(2)
    job["reopening"]["pending"] = False
    save_json(job_dir / "job.json", job)
    write_reopening_status(job_dir, "done")


def export_completed_campaign(runtime: Path) -> bool:
    """Publish retained evidence after completion without changing a verdict."""
    from trellis.pv_revision.archive import campaign_status, export_campaign
    status = campaign_status(runtime)
    if not status or status.get("terminal_outcome") not in {"complete", "all_requirements_retired"}:
        return False
    error_path = runtime / "campaign-archives/last-error.json"
    try:
        # Export performs the authoritative evidence checks again. A previous
        # publication error must not prevent retry after its cause is repaired.
        export_campaign(runtime)
    except Exception as error:
        error_path.parent.mkdir(parents=True, exist_ok=True)
        save_json(error_path, {"error": str(error), "terminal_outcome": status["terminal_outcome"],
                               "recorded_at": time.time()})
        raise
    error_path.unlink(missing_ok=True)
    return True


def continue_native(runtime: Path, *, resume: bool = False) -> bool:
    """Consume a committed native reopening through the existing driver/launcher."""
    from trellis.pv_revision.lifecycle import continue_runtime
    if continue_runtime(runtime, resume=resume):
        export_completed_campaign(runtime)
        return True
    if export_completed_campaign(runtime):
        return True
    end = read_disposition(runtime)
    if not end or end["action"] != "reopen":
        return False
    owner = runtime_job(runtime)
    if not owner:
        return False
    job_dir, _, repo = owner
    with job_lock(job_dir):
        if not resume and (job_dir / "native-controller.pause").exists():
            return False
        job = json.loads((job_dir / "job.json").read_text())
        if Path(job["runtime_root"]).resolve() != runtime.resolve():
            # Duplicate/stale callback. Only an explicit resume may continue a
            # partially initialized child; automatic callbacks never undo pauses.
            pending = job.get("reopening", {})
            if resume and pending.get("pending") and pending["record"]["runtime"] == str(runtime):
                (job_dir / "native-controller.pause").unlink(missing_ok=True)
                complete_reopening(job_dir, job)
            return True
        preparation, native, _ = active_paths(job_dir, job)
        if native.resolve() != repo.resolve():
            raise ValueError("The creation job no longer owns this native repository")
        if (runtime / "pause_request.json").exists() and not resume:
            return False
        alias = Path(job["repo"])
        if not alias.is_symlink() or alias.resolve() != native.resolve():
            raise ValueError("The project alias no longer belongs to this native repository")
        record = capture(preparation, native, runtime, resume=resume)
        if resume:
            (job_dir / "native-controller.pause").unlink(missing_ok=True)
        if (preparation / "goals.json").read_bytes() != active_goal_file(job_dir, job).read_bytes():
            raise ValueError("Preparation goals differ from the original upload")
        if json.loads((preparation / "config.json").read_text()).get("launch_config") != json.loads(active_config_file(job_dir, job).read_text()):
            raise ValueError("Preparation launch settings differ from the saved job")
        generation = int(job.get("native_generation", 0)) + 1
        new_runtime = runtime.parent / f"{job['slug']}-r{generation:04d}-runtime"
        if new_runtime.exists() or new_runtime.is_symlink():
            raise ValueError("The next native runtime path is already occupied")
        output = job_dir / "native/generations" / f"{generation:04d}"
        output.mkdir(parents=True, exist_ok=True)
        # An interruption before job.json publishes the intent can leave just
        # these receipts. Reuse them only for the identical transition.
        for name, value in (("predecessor-job.json", job), ("reopened-from.json", record)):
            path = output / name
            if path.exists() and json.loads(path.read_text()) != value:
                raise ValueError("Generation directory belongs to a different reopening")
        receipt_names = {"predecessor-job.json", "reopened-from.json"}
        allowed = receipt_names | {name + ".lock" for name in receipt_names}
        if any(p.name not in allowed and not any(
                p.name.startswith(name + ".") and p.name.endswith(".tmp") for name in receipt_names)
                for p in output.iterdir()):
            raise ValueError("Unpublished generation directory contains unexpected work")
        stop_retired_services(runtime)
        save_json(output / "predecessor-job.json", job)
        save_json(output / "reopened-from.json", record)
        # Every generation uses distinct paths and process names; the old
        # supervisor can finish its exit without being killed by the new launch.
        job.update(native_generation=generation,
                   runtime_root=str(new_runtime),
                   reopening={"pending": True, "record": record})
        save_json(job_dir / "job.json", job)  # Durable intent precedes all child work.
        try:
            complete_reopening(job_dir, job)
        except BaseException as error:
            write_reopening_status(job_dir, "resolve_failed", str(error) or type(error).__name__)
            raise
    return True


def stop_retired_services(runtime: Path) -> None:
    """Quiesce this generation through the services' existing stop mechanisms."""
    pid_path = runtime / "checker-state/server.pid"
    if not pid_path.exists():
        return
    with pid_path.open() as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_SH | fcntl.LOCK_NB)
            return  # No server owns this pid file.
        except BlockingIOError:
            pid = int(lock.read().strip())
        try:
            fd = os.pidfd_open(pid)
        except ProcessLookupError:
            return
        try:
            argv = Path(f"/proc/{pid}/cmdline").read_bytes().decode().split("\0")
            if "trellis.checker.server" not in argv or str(runtime) not in argv:
                raise RuntimeError("Predecessor checker pid does not identify this runtime")
            signal.pidfd_send_signal(fd, signal.SIGTERM)
        finally:
            os.close(fd)


def schedule_reopening(runtime: Path) -> None:
    owner = runtime_job(runtime)
    end = read_disposition(runtime)
    if not owner or not end or not reopening_status(runtime)["recoverable"]:
        raise ValueError("No current managed, audit-approved preparation reopening")
    job_dir, job, _ = owner
    from trellis.runtime.bridge import any_halt_marker_present
    if any_halt_marker_present(runtime):
        raise ValueError("Resolve the recorded native halt before reopening preparation")
    # The creation launcher already captures the caller's environment before
    # crossing into tmux. Reuse it: tmux's server environment may name an older deployment.
    subprocess.run(["bash", str(ROOT / "scripts/pv_create_run.sh"), "reopen", job["slug"]],
                   check=True, env={**os.environ, "TRELLIS_PROJECTS_ROOT": str(job_dir.parent.parent.parent)})
    print("Resuming through audited preparation reopening; prior runtime is preserved.")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="action", required=True)
    preflight = commands.add_parser("preflight")
    preflight.add_argument("goals", type=Path)
    preflight.add_argument("tools", type=Path)
    for name in ("run", "resume", "reopen", "resume-check", "started", "stop"):
        commands.add_parser(name).add_argument("job", type=Path)
    for name in ("continue", "reopening-status", "resume-native"):
        command = commands.add_parser(name)
        command.add_argument("runtime", type=Path)
        if name == "continue":
            command.add_argument("--resume", action="store_true")
    args = parser.parse_args()
    try:
        if args.action == "preflight":
            collection = validate_inputs(args.goals, args.tools)
            print(json.dumps({"goal_count": len(collection["goals"])}))
        elif args.action in {"run", "resume"}:
            run_job(args.job.resolve(), resume=args.action == "resume")
        elif args.action == "reopen":
            job = json.loads((args.job / "job.json").read_text())
            pending = job.get("reopening", {})
            revision = job.get("revision", {})
            runtime = (revision["predecessor_runtime"] if revision.get("pending") else
                       pending["record"]["runtime"] if pending.get("pending") else job["runtime_root"])
            if not continue_native(Path(runtime).resolve(), resume=True):
                raise ValueError("No committed preparation reopening to resume")
        elif args.action == "resume-check":
            validate_resume(args.job.resolve())
        elif args.action == "started":
            return 0 if startup_ready(args.job.resolve()) else 2
        elif args.action == "continue":
            continue_native(args.runtime.resolve(), resume=args.resume)
        elif args.action == "reopening-status":
            print(json.dumps(reopening_status(args.runtime.resolve())))
        elif args.action == "resume-native":
            schedule_reopening(args.runtime.resolve())
        else:
            stop_job(args.job.resolve())
        return 0
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"Native modeling creation failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
