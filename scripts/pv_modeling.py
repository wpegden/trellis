#!/usr/bin/env python3
"""Run the opt-in modeling and goal-stating prototype."""
from pathlib import Path
import argparse
import fcntl
import json
import os
import signal
import subprocess
import sys
import time
import traceback

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from trellis.modeling.driver import Driver, initialize, check_halt
from trellis.modeling.observations import observe, passed, read_json
from trellis.modeling.inspection import cli_preview, inspect_ir, inspect_observation, observation_summary
from trellis.modeling.comparison import compare
from trellis.modeling.probe import probe_source
from trellis.modeling.environment import run_source_command, source_environment
from trellis.pv_toolchain import effective_tools
from trellis.pv_execution import execution_budget, execution_timeout, positive_timeout


def _log_preview(path: Path) -> str:
    """Keep both log ends within a 2,000-character JSON preview."""
    def edge(text: str, *, reverse: bool = False) -> str:
        selected, size = [], 0
        for char in reversed(text) if reverse else text:
            size += len(json.dumps(char)) - 2
            if size > 950:
                break
            selected.append(char)
        return "".join(reversed(selected) if reverse else selected)

    with path.open(errors="replace") as stream:
        start = stream.read(2001)
        if len(start) <= 2000 and len(json.dumps(start)) - 2 <= 2000:
            return start
        size, end = len(start), start[-950:]
        while chunk := stream.read(65536):
            size += len(chunk)
            end = (end + chunk)[-950:]
    start, end = edge(start), edge(end, reverse=True)
    return start + f"\n... [{size - len(start) - len(end)} characters omitted] ...\n" + end


def runner_pid(root: Path) -> int | None:
    lock = root / ".trellis/modeling/runner.lock"
    if not lock.exists():
        return None
    info = lock.stat()
    for line in Path("/proc/locks").read_text().splitlines():
        fields = line.split()
        if len(fields) < 6 or fields[1:4] != ["FLOCK", "ADVISORY", "WRITE"]:
            continue
        major, minor, inode = fields[5].split(":")
        if (int(major, 16), int(minor, 16), int(inode)) == (os.major(info.st_dev), os.minor(info.st_dev), info.st_ino):
            return int(fields[4])
    return None


def runner_status(root: Path) -> dict:
    """Observe preparation's own driver, independently of a native supervisor."""
    preparation = read_json(root / ".trellis/modeling/state.json")
    pid = runner_pid(root)
    paused = (root / ".trellis/modeling/pause").exists()
    from trellis.modeling.engineering import native_status
    engineering = native_status(root, preparation, runner_active=pid is not None)
    if engineering is not None and engineering.get('status') == 'unresolved':
        state = 'engineering_unresolved'
    elif (engineering is not None and engineering.get('status') == 'engineering_ready'
          or engineering is None and preparation.get('status') == 'engineering_ready'):
        state = 'engineering_ready'
    elif pid is not None:
        state = "preparation_pausing" if paused else "preparing"
    elif paused:
        state = "preparation_paused"
    elif preparation.get("status") == "prepared":
        state = "prepared"
    else:
        state = "preparation_stopped"
    return {
        "controller": "preparation", "state": state, "wrapper_pid": pid,
        "sentinel_present": paused, "request": None,
        "resumable": False, "launch_env": None,
        "preparation": engineering if engineering is not None else {
            "stage": preparation.get("stage"), "status": preparation.get("status"),
            "sequence": preparation.get("sequence"),
            "role": (preparation.get("next") or {}).get("role"),
        },
    }


def stop_runner(root: Path) -> None:
    # Interrupt the lock-owning driver before its agent, so transport retry
    # cannot restart an intentionally stopped burst. pidfd pins its identity.
    pid = runner_pid(root)
    if pid is not None:
        try:
            fd = os.pidfd_open(pid)
            try:
                if runner_pid(root) == pid:
                    signal.pidfd_send_signal(fd, signal.SIGINT)
            finally:
                os.close(fd)
        except ProcessLookupError:
            pass
    from trellis.burst import tmux_kill_session
    state = read_json(root / ".trellis/modeling/state.json")
    request_file = root / ".trellis/modeling/requests" / (
        f"preparation-{state['sequence']:05d}-{state['next']['role']}.request.json")
    if request_file.exists():
        tmux_kill_session(read_json(request_file)["session_name"])
    deadline = time.monotonic() + 10
    while runner_pid(root) == pid and pid is not None:
        if time.monotonic() >= deadline:
            raise RuntimeError("The interrupted Modeling driver has not exited")
        time.sleep(0.05)


def _main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    init = sub.add_parser("init")
    init.add_argument("root", type=Path)
    init.add_argument("--source", type=Path, required=True)
    init.add_argument("--goals", type=Path, required=True)
    init.add_argument("--tools", type=Path, required=True)
    init.add_argument("--config-template", type=Path, help="Operator launch settings, including preparation roles")
    init.add_argument("--model", default="gpt-6.1-sol")
    init.add_argument("--effort", default="high")
    run = sub.add_parser("run")
    run.add_argument("root", type=Path)
    run.add_argument("--prepare-only", action="store_true")
    run.add_argument("--no-viewer", action="store_true", help="Keep the existing viewer server running")
    run.add_argument("--stop-file", type=Path, help="Optional controller stop boundary; only the controller clears it")
    run.add_argument("--prewarm-from", type=Path)
    run.add_argument("--repo", type=Path)
    run.add_argument("--runtime", type=Path)
    for command in ("step", "show", "status", "pause", "stop"):
        sub.add_parser(command).add_argument("root", type=Path)
    check = sub.add_parser("check")
    check.add_argument("root", type=Path)
    check.add_argument("--through", choices=("cargo", "charon", "aeneas", "lean"), default="lean")
    check.add_argument("--fresh", action="store_true")
    check.add_argument("--output", type=Path, help="Writable output directory for checks of a read-only preparation")
    native_check = sub.add_parser("check-native", help="Check an exported repository against its audited preparation")
    native_check.add_argument("root", type=Path)
    native_check.add_argument("--fresh", action="store_true", help="Also re-extract and compare the generated Lean")
    native_check.add_argument("--output", type=Path)
    probe = sub.add_parser("probe")
    probe.add_argument("root", type=Path)
    probe.add_argument("--source", type=Path, required=True)
    probe.add_argument("--through", choices=("cargo", "charon", "aeneas", "lean"), default="lean")
    inspect = sub.add_parser("inspect")
    inspect.add_argument("root", type=Path)
    inspect.add_argument("--observation", type=Path)
    inspect.add_argument("--symbol", default="")
    inspect.add_argument("--module", default="")
    inspect.add_argument("--source", default="")
    inspect.add_argument("--ir", action="store_true", help="Locate --symbol in Charon's readable intermediate code")
    comparison = sub.add_parser("compare", description=(
        "Compare exact stdout from successful command runs in each side's scratch directory. "
        "{source} names its writable source copy; {scratch} names that directory."))
    comparison.add_argument("root", type=Path)
    comparison.add_argument("--baseline", type=Path, required=True)
    comparison.add_argument("--candidate", type=Path, required=True)
    comparison.add_argument("--command", dest="observation_command", nargs=argparse.REMAINDER, required=True)
    execute = sub.add_parser("exec", help="Run a source diagnostic with the configured tools; full output is retained in scratch")
    execute.add_argument("root", type=Path)
    execute.add_argument("--cwd", type=Path, help="Working directory (default: current directory)")
    execute.add_argument("--output", type=Path, help="Writable parent for per-execution logs and build storage")
    execute.add_argument("--command", dest="execution_command", nargs=argparse.REMAINDER, required=True)
    compose = sub.add_parser("compose-native", help="Regenerate the model-support Preamble from its authored definitions")
    compose.add_argument("root", type=Path)
    compose.add_argument("--check", action="store_true", help="Check that the composed Preamble is current without writing it")
    inventory = sub.add_parser("inspect-native", help="Inspect the frozen extraction boundary; full identities are retained in scratch")
    inventory.add_argument("root", type=Path)
    argv = list(sys.argv[1:] if argv is None else argv)
    command_tail = None
    if argv and argv[0] in {"exec", "compare"} and "--command" in argv:
        boundary = argv.index("--command")
        command_tail = argv[boundary + 1:]
        argv = argv[:boundary + 1]
    for command in ("init", "run", "step", "check", "check-native", "probe", "inspect", "compare", "exec"):
        sub.choices[command].add_argument("--timeout", type=positive_timeout,
            help="Per-command seconds; overrides configured budgets without an upper cap (run/step also apply it to child agents and bootstrap)")
    args = parser.parse_args(argv)
    if command_tail is not None:
        if not command_tail:
            parser.error("--command requires an executable and its arguments")
        setattr(args, "execution_command" if args.command == "exec" else "observation_command", command_tail)
    with execution_budget(getattr(args, "timeout", None)):
        return _execute(args)


def _execute(args):
    root = args.root.resolve()
    if args.command == "init":
        initialize(root, args.source.resolve(), args.goals.resolve(), read_json(args.tools), model=args.model, effort=args.effort,
                   config_template=args.config_template.resolve() if args.config_template else None)
    elif args.command in {"compose-native", "inspect-native"}:
        from trellis.json_io import save_json
        from trellis.pv_model_support import boundary_inventory, sync_model_support
        output = root / ".trellis/scratch" / f"{args.command}-{time.time_ns()}.json"
        if args.command == "compose-native":
            manifest = read_json(root / "modeling-input/manifest.json")
            result = sync_model_support(root, write=not args.check, lean_imports=manifest.get("lean_imports", []))
            save_json(output, result)
            # The per-module hash map scales with the extraction. Keep that
            # complete receipt on disk: preserving all its named keys cannot
            # fit a bounded preview, even after shortening every string.
            evidence = dict(result["evidence"])
            if "extracted_modules" in evidence:
                evidence["extracted_module_count"] = len(evidence.pop("extracted_modules"))
            print(cli_preview({"full_result": str(output), **result, "evidence": evidence},
                              details="the full result file"))
            return 0 if result["matches"] else 1
        boundaries = boundary_inventory((root / "generated.lean").read_text())
        save_json(output, {"declarations": boundaries})
        preview = [{key: value for key, value in item.items() if key != "source_utf8"} for item in boundaries]
        print(cli_preview({"full_index": str(output), "declaration_count": len(boundaries),
                           "declarations": preview}, details="the full index file"))
    elif args.command == "check-native":
        from trellis.modeling.native import check_provenance
        result = check_provenance(root, fresh=args.fresh, output=args.output.resolve() if args.output else None)
        preview = dict(result)
        if isinstance(preview.get("model_support"), dict):
            support = preview["model_support"]
            preview["model_support"] = {k: support[k] for k in ("matches", "changed", "errors") if k in support}
        print(cli_preview(preview, details="the check evidence files"))
        return 0 if result["passed"] else 1
    elif args.command == "check":
        output = args.output.resolve() if args.output else root / ".trellis/scratch" / f"check-{time.time_ns()}"
        result = observe(root, output, through=args.through,
                         history_root=output,
                         cache_root=output / "cache" if args.fresh or args.output else root / ".trellis/scratch/check-cache",
                         read_caches=[] if args.fresh else [root / ".trellis/modeling/cache"])
        print(cli_preview(observation_summary(result, diagnostic_details=False), details="the observation file"))
        return 0 if passed(result, args.through) else 1
    elif args.command == "probe":
        result = probe_source(root, args.source.resolve(), root / ".trellis/scratch" / f"probe-{time.time_ns()}", through=args.through)
        print(cli_preview(observation_summary(result, diagnostic_details=False), details="the observation file"))
        return 0 if passed(result, args.through) else 1
    elif args.command == "inspect":
        if args.ir and (args.source or args.module):
            raise ValueError("--ir supports --symbol; --source and --module filter generated Lean")
        observation = args.observation
        if observation is None:
            state = read_json(root / ".trellis/modeling/state.json")
            path = state["observation"].get("observation_file")
            if not path:
                raise ValueError("Run a check and supply its --observation path")
            observation = Path(path)
        if args.ir:
            recorded = read_json(observation)
            index = recorded.get("composed_index") or recorded.get("declaration_index")
            mapped = inspect_observation(observation, symbol=args.symbol) if index and Path(index).is_file() else {}
            result = inspect_ir(observation, tools=effective_tools(root),
                                output=root / ".trellis/scratch" / f"inspect-ir-{time.time_ns()}",
                                symbol=args.symbol, source_symbols=tuple(
                                    item["source_symbol"] for item in mapped.get("declarations", [])
                                    if item.get("source_symbol")))
        else:
            result = inspect_observation(observation, symbol=args.symbol, source=args.source, module=args.module)
        print(cli_preview(result, details="the full index and observation files"))
        if args.ir:
            return 0 if result["status"] == "available" else 1
    elif args.command == "compare":
        result = compare(args.baseline.resolve(), args.candidate.resolve(), args.observation_command,
                         root / ".trellis/scratch" / f"comparison-{time.time_ns()}", timeout=args.timeout,
                         tools=effective_tools(root))
        for side in ("baseline", "candidate"):
            result[side].pop("environment", None)
        print(cli_preview(result, details="the comparison log files"))
        return 0 if result["same_observation"] else 1
    elif args.command == "exec":
        from trellis.json_io import save_json
        output = ((args.output or root / ".trellis/scratch") / f"exec-{time.time_ns()}").resolve()
        result = run_source_command(args.execution_command, cwd=(args.cwd or Path.cwd()).resolve(),
                                    env=source_environment(effective_tools(root), output),
                                    stdout=output / "stdout", stderr=output / "stderr",
                                    timeout=execution_timeout(effective_tools(root), "exec"))
        save_json(output / "execution.json", result)
        result.pop("environment", None)
        result["record"] = str(output / "execution.json")
        for field in ("stdout", "stderr"):
            result[field + "_preview"] = _log_preview(Path(result[field]))
        print(cli_preview(result, details="the execution log files"))
        return 0 if result["exit_code"] == 0 and not result["timeout"] else 1
    elif args.command == "status":
        print(json.dumps(runner_status(root), indent=2))
    elif args.command == "show":
        state = read_json(root / ".trellis/modeling/state.json")
        print(json.dumps({k: v for k, v in state.items() if k != "history"}, indent=2))
    elif args.command in {"pause", "stop"}:
        (root / ".trellis/modeling/pause").touch()
        if args.command == "stop":
            stop_runner(root)
    else:
        with (root / ".trellis/modeling/runner.lock").open("w") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise ValueError("This run already has an active driver")
            pause = root / ".trellis/modeling/pause"
            controller_stop = getattr(args, "stop_file", None)
            def paused():
                return pause.exists() or bool(controller_stop and controller_stop.exists())
            if controller_stop and controller_stop.exists():
                return 0
            pause.unlink(missing_ok=True)
            driver = Driver(root)
            check_halt(root)
            if driver.state["status"] == "halted":
                driver.state["status"] = driver.state.pop("resume_status")
                driver.save()
            while driver.state["status"] == "active" and not paused():
                driver.step()
                if args.command == "step":
                    break
            if args.command == "run" and driver.state["status"] == "prepared" and not args.prepare_only and not paused():
                from trellis.modeling.native import prepare, export_directory, REPO
                repo = args.repo.resolve() if args.repo else root.with_name(root.name + "-development")
                runtime = args.runtime.resolve() if args.runtime else root.with_name(root.name + "-runtime")
                try:
                    config = prepare(root, repo, runtime,
                                     prewarm_from=args.prewarm_from.resolve() if args.prewarm_from else None)
                    prepared = read_json(export_directory(root, repo) / "prepared.json")
                    if paused():
                        raise KeyboardInterrupt("Native bootstrap stopped before campaign launch")
                    subprocess.run([str(REPO / "scripts/restart_configured_run.sh"), "--no-reset", "--no-current",
                                    *(["--no-viewer"] if args.no_viewer else []), str(config), str(runtime)], cwd=REPO, check=True,
                                   env=dict(os.environ, **prepared["launch_environment"]))
                except (ValueError, OSError, RuntimeError, subprocess.SubprocessError, SystemExit, KeyboardInterrupt) as error:
                    evidence = export_directory(root, repo) / "bootstrap-error.json"
                    from trellis.json_io import save_json
                    save_json(evidence, {"error": str(error) or type(error).__name__, "interrupted": isinstance(error, KeyboardInterrupt), "traceback": traceback.format_exc(),
                                         "repo": str(repo), "runtime": str(runtime)})
                    if isinstance(error, KeyboardInterrupt):
                        error = RuntimeError("Native bootstrap interrupted; partial output is preserved for retry")
                    driver.halt("native_bootstrap", error, evidence)
                driver.state["native_runtime"] = str(runtime)
                driver.state["native_repo"] = str(repo)
                driver.state.pop("host_failure", None)
                driver.save()
            return {"failed": 1, "declined": 2}.get(driver.state["status"], 0)


def main():
    try:
        return _main()
    except (ValueError, OSError, RuntimeError) as error:
        print(json.dumps({"error": str(error)}))
        return 1


if __name__ == "__main__":
    sys.exit(main())
