"""Empirical measurements of preparation snapshots, outside proof authority.

Commands run in a dedicated credential-free, network-disabled namespace. Host
receipts establish inputs and observations; workload quality and whether a
harness exercises the intended implementation remain review obligations.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import platform
import posixpath
import re
import selectors
import shutil
import subprocess
import time
import uuid

from trellis.paper_implementation import canonical_json, evidence_path, relative_path
from trellis.pv_execution import owned_process, positive_timeout, semantic_tools

STORE = Path(".trellis/modeling/performance")
DEFAULT_TIMEOUT = 120.0
MAX_TIMEOUT = 3600.0
MAX_COMMANDS = 32
MAX_LOG_BYTES = 4 * 1024 * 1024
MAX_OUTPUT_BYTES = 16 * 1024 * 1024
MAX_EXECUTABLE_BYTES = 128 * 1024 * 1024
MAX_GENERATED_BYTES = 512 * 1024 * 1024
MAX_GENERATED_FILES = 50000
_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}\Z")
_RUN_ID = re.compile(r"[0-9a-f]{32}\Z")


def _hash(value) -> str:
    return hashlib.sha256(canonical_json(value)).hexdigest()


def sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def _json(path: Path) -> dict:
    value = json.loads(path.read_text())
    if not isinstance(value, dict):
        raise ValueError(f"Expected an object: {path}")
    return value


def _directory(path: Path) -> None:
    if any(p.is_symlink() for p in [path, *path.parents]):
        raise ValueError(f"Measurement paths cannot use aliases: {path}")
    path.mkdir(parents=True, exist_ok=True)


def _inventory(root: Path, *, ignored: set[str] = frozenset()) -> dict[str, str]:
    if not root.is_dir() or root.is_symlink():
        raise ValueError(f"Measurement input directory is absent or aliased: {root}")
    result = {}
    for directory, dirs, names in os.walk(root):
        dirs[:] = sorted(d for d in dirs if d not in ignored)
        if any((Path(directory) / d).is_symlink() for d in dirs):
            raise ValueError("Measurement input directories cannot be aliases")
        for name in sorted(names):
            if name in ignored:
                continue
            item = Path(directory) / name
            relative = item.relative_to(root).as_posix()
            result[relative] = sha256(evidence_path(root, relative))
    return result


def harness_fingerprint(root: Path) -> str:
    """Reports may change after measurements; executable harness inputs may not."""
    directory = root / "benchmarks/harness"
    inventory = _inventory(directory)
    return _hash({"files": inventory, "modes": _modes(directory, inventory)})


def _modes(root: Path, names) -> dict[str, int]:
    return {name: evidence_path(root, name).stat().st_mode & 0o777 for name in names}


def _copy_input(source: Path, destination: Path) -> None:
    shutil.copyfile(source, destination)
    destination.chmod(source.stat().st_mode & 0o777)


def _selected_tools(root: Path) -> tuple[dict, list[Path]]:
    from trellis.pv_toolchain import effective_tools
    config = _json(root / "config.json")
    route = config.get("paper_input", {}).get("route")
    if route not in {"rust_aeneas", "lean_native"}:
        raise ValueError("Empirical paper measurements require an implementation route")
    tools = effective_tools(root)
    selected = {}
    if route == "rust_aeneas":
        cargo = Path(tools["cargo"]).resolve()
        selected = {"cargo": str(cargo), "rustc": str(Path(tools.get("rustc", cargo.with_name("rustc"))).resolve())}
        rustdoc = Path(tools.get("rustdoc", cargo.with_name("rustdoc"))).resolve()
        if rustdoc.is_file():
            selected["rustdoc"] = str(rustdoc)
    else:
        lean = Path(tools["lean"]).resolve()
        selected = {name: str(lean.with_name(name)) for name in ("lean", "lake", "leanc", "clang", "llvm-ar")
                    if lean.with_name(name).is_file()}
        if not {"lean", "lake", "leanc"} <= set(selected):
            raise ValueError("Lean measurements require installed Lean, Lake and leanc")
    roots = set()
    for name, path in selected.items():
        binary = Path(path)
        if not binary.is_file() or binary.parent.name != "bin" or not (binary.parent.parent / "lib").is_dir():
            raise ValueError(f"Measurements require a direct installed compiler toolchain, not a credential-bearing proxy: {name}")
        roots.add(binary.parent.parent)
    return {"route": route, "executables": selected, "selection_sha256": _hash(semantic_tools(tools)),
            "lean_toolchain": tools.get("lean_toolchain")}, sorted(roots)


def _tool_identity(root: Path, selection: dict, roots: list[Path]) -> dict:
    """Reuse the existing protected stat/content cache for loadable resources."""
    from trellis.modeling.environment import environment_signature
    cache = root / STORE / "resource-digests.json"
    _directory(cache.parent)
    if cache.is_symlink():
        raise ValueError("Measurement resource cache cannot be an alias")
    selected = selection["executables"]
    resources = environment_signature({**{k: v for k, v in selected.items() if k in {"cargo", "rustc", "rustdoc", "lean"}},
                                       "lean_paths": [str(p / "lib") for p in roots]}, cache_path=cache)
    # Rust archives are not Lean-loadable artifacts, so add their identities.
    archive_cache = cache.with_name("rust-archive-digests.json")
    if archive_cache.is_symlink():
        raise ValueError("Measurement resource cache cannot be an alias")
    previous = _json(archive_cache) if archive_cache.is_file() else {}
    archives, updated = {}, {}
    for directory in roots:
        for path in sorted((directory / "lib").rglob("*.rlib")):
            stat = path.stat()
            stamp = [stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns]
            old = previous.get(str(path), {})
            digest = old.get("sha256") if old.get("stat") == stamp else sha256(path)
            archives[str(path)] = digest
            updated[str(path)] = {"stat": stamp, "sha256": digest}
    archive_cache.write_bytes(canonical_json(updated) + b"\n")
    system = {}
    for name in ("bwrap", "sh", "python3", "cc", "ld", "as", "ar"):
        path = shutil.which(name, path="/usr/bin:/bin")
        if path:
            actual = Path(path).resolve()
            system[name] = {"path": str(actual), "sha256": sha256(actual)}
    return {**selection, "executables": {k: {"path": v, "sha256": sha256(Path(v)), "mode": Path(v).stat().st_mode & 0o777} for k, v in selected.items()},
            "resource_sha256": resources, "rust_archives_sha256": _hash(archives), "system_tools": system}


def _implementation_inputs(root: Path, route: str) -> tuple[dict[str, Path], dict]:
    """Map virtual implementation files to current sources, without cache inputs."""
    from scripts.extract_pv_model import source_input_manifest
    from trellis.paper_implementation import lean_lakefile, lean_lake_manifest, NATIVE_LAKEFILE, NATIVE_LOCK
    config = _json(root / "config.json")
    if route == "rust_aeneas":
        source = root / "work/crate"
        inventory = source_input_manifest(source)
        return {item["relative_path"]: evidence_path(source, item["relative_path"]) for item in inventory["files"]}, {}
    source = root / "work/lean"
    inputs = {"Tablet/" + name: evidence_path(source / "Tablet", name)
              for name in _inventory(source / "Tablet") if name.endswith(".lean")}
    if not inputs:
        raise ValueError("No Lean implementation source is available to measure")
    nodes = [entry["node_id"] for entry in config["paper_input"]["entry_points"]]
    proof = "proof_project" in config
    descriptors = {"lakefile.toml": NATIVE_LAKEFILE if proof else "lakefile.toml",
                   "lake-manifest.json": NATIVE_LOCK if proof else "lake-manifest.json",
                   "lean-toolchain": "lean-toolchain"}
    for name, original in descriptors.items():
        inputs[name] = evidence_path(source, original)
    if (inputs["lakefile.toml"].read_text() != lean_lakefile(nodes)
            or json.loads(inputs["lake-manifest.json"].read_text()) != json.loads(lean_lake_manifest())):
        raise ValueError("Lean measurements require the canonical core-only native assembly")
    extra = {name: sha256(evidence_path(source, name)) for name in (
        "paper-implementation/proof-dependencies.json", "paper-implementation/mathlib-lake-manifest.json")
        if proof}
    return inputs, extra


def _basis(root: Path) -> tuple[dict, dict[str, Path], list[Path]]:
    from trellis.paper_implementation import preparation_paper_digest
    selection, roots = _selected_tools(root)
    inputs, proof = _implementation_inputs(root, selection["route"])
    config = _json(root / "config.json")
    setup = {name: sha256(evidence_path(root, name)) for name in ("work/extraction.json", "work/SETUP.md", "trellis.config.json")
             if (root / name).exists()}
    lean_source = root / "work/lean"
    if selection["route"] == "lean_native":
        setup.update({"work/lean/" + name: sha256(evidence_path(lean_source, name)) for name in
                      ("lakefile.toml", "lake-manifest.json")})
        setup.update({"work/lean/Tablet/" + name: digest for name, digest in _inventory(lean_source / "Tablet").items()})
    payload = {"schema": "trellis-paper-performance-basis/v1", "paper_sha256": preparation_paper_digest(root),
               "runner_sha256": sha256(Path(__file__)),
               "configuration_sha256": _hash({k: v for k, v in config.items() if k not in {"model", "effort"}}),
               "implementation_files": {name: sha256(path) for name, path in sorted(inputs.items())},
               "implementation_modes": {name: path.stat().st_mode & 0o777 for name, path in sorted(inputs.items())},
               "work_configuration": setup, "proof_metadata": proof, "tools": _tool_identity(root, selection, roots)}
    return payload, inputs, roots


def current_basis(root: Path) -> str:
    return _hash(_basis(root.resolve())[0])


def _machine() -> dict:
    cpu = Path("/proc/cpuinfo")
    model = next((line.split(":", 1)[1].strip() for line in cpu.read_text().splitlines()
                  if line.startswith("model name") and ":" in line), "unknown") if cpu.is_file() else "unknown"
    return {"platform": platform.platform(), "cpu_count": os.cpu_count(), "cpu_model": model,
            "cpu_affinity": sorted(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None,
            "host_clock": "perf_counter_ns elapsed time includes sandbox/process startup and output collection"}


def _commands(raw: dict) -> list[dict]:
    if (not isinstance(raw, dict) or raw.get("action") != "measure"
            or not isinstance(raw.get("reason"), str) or not raw["reason"].strip()
            or set(raw) - {"action", "reason", "commands", "system_feedback"}):
        raise ValueError("Expected a performance measure request")
    commands = raw.get("commands")
    if not isinstance(commands, list) or not 1 <= len(commands) <= MAX_COMMANDS:
        raise ValueError("Measurements require 1 to 32 commands")
    result, names = [], set()
    for command in commands:
        if not isinstance(command, dict) or set(command) - {"name", "argv", "cwd", "kind", "timeout_seconds"}:
            raise ValueError("Unsupported measurement command fields")
        name, argv, cwd = command.get("name"), command.get("argv"), command.get("cwd")
        if not isinstance(name, str) or not _NAME.fullmatch(name) or name in names:
            raise ValueError("Measurement command names must be distinct simple labels")
        names.add(name)
        if (not isinstance(argv, list) or not 1 <= len(argv) <= 256 or not argv[0]
                or any(not isinstance(arg, str) or "\x00" in arg for arg in argv)):
            raise ValueError("Measurement argv must preserve a nonempty executable and NUL-free arguments")
        if cwd != ".":
            relative_path(cwd)
            if Path(cwd).parts[0] not in {"implementation", "harness", "build", "output"}:
                raise ValueError("Measurement cwd must use implementation, harness, build or output")
        if command.get("kind") not in {"build", "run"}:
            raise ValueError("Measurement command kind must be build or run")
        seconds = positive_timeout(command.get("timeout_seconds", DEFAULT_TIMEOUT))
        if seconds > MAX_TIMEOUT:
            raise ValueError("Measurement command timeout exceeds 3600 seconds")
        result.append({**command, "timeout_seconds": seconds})
    return result


def _environment(selection: dict) -> dict[str, str]:
    executables = selection["executables"]
    directories = sorted({str(Path(item["path"]).parent) for item in executables.values()})
    env = {"PATH": ":".join([*directories, "/usr/bin", "/bin"]), "HOME": "/home/benchmark",
           "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8", "TMPDIR": "/tmp", "XDG_CACHE_HOME": "/build/cache",
           "CARGO_HOME": "/build/cargo-home", "CARGO_TARGET_DIR": "/build/cargo-target", "CARGO_NET_OFFLINE": "true"}
    for name in ("rustc", "rustdoc"):
        if name in executables:
            env[name.upper()] = executables[name]["path"]
    if selection.get("lean_toolchain"):
        env["ELAN_TOOLCHAIN"] = selection["lean_toolchain"]
    return env


def _sandbox(workspace: Path, roots: list[Path], env: dict, command: dict) -> list[str]:
    binary = shutil.which("bwrap", path="/usr/bin:/bin")
    if not binary:
        raise ValueError("bwrap is required for empirical measurements; no host fallback is allowed")
    argv = [binary, "--die-with-parent", "--unshare-pid", "--unshare-ipc", "--unshare-uts", "--unshare-net",
            "--cap-drop", "ALL", "--clearenv", "--proc", "/proc", "--dev", "/dev",
            "--tmpfs", "/tmp", "--dir", "/home", "--tmpfs", "/home/benchmark"]
    for name in ("/usr", "/bin", "/lib", "/lib64", "/etc/ld.so.cache"):
        if Path(name).exists():
            argv += ["--ro-bind", name, name]
    # Debian's compiler names point through /etc/alternatives. Recreate only
    # these exact public aliases, without exposing the host's /etc tree.
    for name in ("cc", "c++"):
        alias = Path("/usr/bin") / name
        target = Path("/etc/alternatives") / name
        if alias.is_symlink() and os.readlink(alias) == str(target):
            resolved = alias.resolve()
            if not resolved.is_relative_to(Path("/usr")):
                raise ValueError("System compiler aliases must resolve inside the mounted /usr toolchain")
            argv += ["--symlink", str(resolved), str(target)]
    for root in roots:
        if not any(root.is_relative_to(Path(p)) for p in ("/usr", "/bin", "/lib", "/lib64")):
            argv += ["--ro-bind", str(root), str(root)]
    for name in ("implementation", "harness"):
        argv += ["--ro-bind", str(workspace / name), "/" + name]
    for name in ("build", "output"):
        argv += ["--bind", str(workspace / name), "/" + name]
    if (workspace / "implementation/.lake").is_dir():
        argv += ["--bind", str(workspace / "build/implementation-lake"), "/implementation/.lake"]
    for key, value in sorted(env.items()):
        argv += ["--setenv", key, value]
    return [*argv, "--chdir", "/" if command["cwd"] == "." else "/" + command["cwd"], "--", *command["argv"]]


def _executable(command: dict, workspace: Path, env: dict, roots: list[Path]) -> tuple[dict, Path | None]:
    name = command["argv"][0]
    virtual = name if name.startswith("/") else ("/" + command["cwd"].strip(".") + "/" + name if "/" in name else None)
    if virtual is None:
        path = shutil.which(name, path=env["PATH"])
        path = Path(path).resolve() if path else None
    else:
        virtual = "/" + posixpath.normpath(virtual).lstrip("/")
        parts = Path(virtual).parts
        path = workspace.joinpath(*parts[1:]) if len(parts) > 1 and parts[1] in {"implementation", "harness", "build", "output"} else Path(virtual)
        if path.is_relative_to(workspace):
            if any(p.is_symlink() for p in [path, *path.parents] if p.is_relative_to(workspace)):
                raise ValueError("Measurement entrypoints cannot use snapshot/build symlinks")
        path = path.resolve()
    allowed = [workspace, *roots, *map(Path, ("/usr", "/bin", "/lib", "/lib64"))]
    if path is not None and not any(path.is_relative_to(base) for base in allowed):
        # The process will receive ENOENT inside its closed namespace. Do not
        # inspect an agent-named host path while describing that failure.
        path = None
    present = path is not None and path.is_file()
    if present and path.stat().st_size > MAX_EXECUTABLE_BYTES:
        return {"argv0": name, "sha256": None, "identity_error": "entrypoint exceeds the 128MiB identity limit"}, None
    identity = {"argv0": name, "sha256": sha256(path) if present else None,
                "scope": "command entrypoint; child executable/source invocation requires review"}
    if present and not path.is_relative_to(workspace):
        identity["external_path"] = str(path)
    return identity, path if present else None


def _build_inventory(workspace: Path) -> dict:
    """Describe generated artifacts without following their symlinks."""
    result = {}
    pending = [workspace / name for name in ("build", "output")]
    consumed = count = 0
    while pending:
        with os.scandir(pending.pop()) as entries:
            for item in entries:
                count += 1
                if count > MAX_GENERATED_FILES:
                    result["__inventory_limit__"] = {"unhashed": "generated file-count budget exceeded"}
                    return result
                path = Path(item.path)
                relative = path.relative_to(workspace).as_posix()
                if item.is_symlink():
                    result[relative] = {"symlink": os.readlink(path)}
                elif item.is_dir(follow_symlinks=False):
                    pending.append(path)
                elif item.is_file(follow_symlinks=False):
                    size = item.stat(follow_symlinks=False).st_size
                    if consumed + size > MAX_GENERATED_BYTES:
                        result[relative] = {"size": size, "unhashed": "generated-data hash budget exceeded"}
                    else:
                        result[relative] = {"sha256": sha256(path), "size": size}
                        consumed += size
                else:
                    result[relative] = {"unhashed": "unsupported generated special file"}
    return result


def _run_bounded(argv: list[str], *, cwd: Path, env: dict, stdout: Path, stderr: Path, timeout: float) -> dict:
    """Stream bounded raw output while owning and cancelling the whole process."""
    class OutputLimit(Exception):
        pass
    timed_out, exceeded, error = False, False, ""
    deadline = time.monotonic() + timeout
    total = 0
    process = None
    with stdout.open("wb") as out, stderr.open("wb") as err:
        try:
            with owned_process(argv, compute_timeout=timeout, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
                deadline = time.monotonic() + timeout  # Admission is not execution time.
                with selectors.DefaultSelector() as selector:
                    selector.register(process.stdout, selectors.EVENT_READ, out)
                    selector.register(process.stderr, selectors.EVENT_READ, err)
                    while selector.get_map():
                        remaining = deadline - time.monotonic()
                        if remaining <= 0:
                            raise subprocess.TimeoutExpired(argv, timeout)
                        for key, _ in selector.select(min(.1, remaining)):
                            chunk = key.fileobj.read1(65536)
                            if not chunk:
                                selector.unregister(key.fileobj)
                                continue
                            available = MAX_LOG_BYTES - total
                            key.data.write(chunk[:available])
                            total += min(len(chunk), available)
                            if len(chunk) > available:
                                raise OutputLimit()
                    process.wait(timeout=max(.001, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            timed_out = True
        except OutputLimit:
            exceeded = True
        except OSError as exc:
            error = str(exc)
    return {"exit_code": process.returncode if process is not None else -1, "timeout": timed_out,
            "output_limit_exceeded": exceeded, "execution_error": error}


def run_measurement(root: Path, state: dict, response: dict, *, operation: Path | None = None,
                    identity: dict | None = None) -> dict:
    """Retain a native operation before execution; partial operations require recovery."""
    if operation is None:
        if identity is not None:
            raise ValueError('Measurement identity requires its operation record')
        return _run_measurement(root, state, response)
    from trellis.modeling import history
    if not identity:
        raise ValueError('Native measurement requires its request and candidate identity')
    commands = _commands(response)
    admitted = dict(identity=identity, commands=commands, reason=response['reason'],
                    basis=current_basis(root), harness_sha256=harness_fingerprint(root))
    try:
        saved = history._json(operation)
    except FileNotFoundError:
        saved = dict(admitted=admitted, run_id=uuid.uuid4().hex)
        history._put(operation, canonical_json(saved))
    else:
        if saved.get('admitted') != admitted:
            raise RuntimeError('Retained measurement operation changed')
        try:
            record = recorded_evidence(root, saved['run_id'])
        except (OSError, ValueError, KeyError):
            raise RuntimeError('Retained measurement operation is unresolved') from None
        if (record['basis'] != admitted['basis'] or record['harness_sha256'] != admitted['harness_sha256']
                or record['reason'] != admitted['reason']
                or [{key: item.get(key) for key in command} for item, command in zip(record['commands'], commands)] != commands
                or len(record['commands']) != len(commands)):
            raise RuntimeError('Retained measurement receipt differs from its operation')
        return record
    return _run_measurement(root, state, response, run_id=saved['run_id'], admitted=admitted)


def _run_measurement(root: Path, state: dict, response: dict, *, run_id=None, admitted=None) -> dict:
    """Execute one bounded batch; callers retain all failures for review."""
    root = root.resolve()
    commands = _commands(response)
    basis, inputs, tool_roots = _basis(root)
    harness = _inventory(root / "benchmarks/harness")
    harness_modes = _modes(root / "benchmarks/harness", harness)
    if not harness:
        raise ValueError("Write the benchmark harness before requesting a measurement")
    if admitted is not None and (admitted['basis'] != _hash(basis)
            or admitted['harness_sha256'] != _hash({'files': harness, 'modes': harness_modes})):
        raise RuntimeError('Measurement inputs changed after operation admission')
    run_id = run_id or uuid.uuid4().hex
    if not isinstance(run_id, str) or not _RUN_ID.fullmatch(run_id):
        raise ValueError('Invalid measurement operation ID')
    run = root / STORE / "runs" / run_id
    _directory(run.parent)
    run.mkdir()
    workspace = run / "workspace"
    for name in ("implementation", "harness", "build", "output"):
        _directory(workspace / name)
    for name, path in inputs.items():
        destination = workspace / "implementation" / relative_path(name)
        _directory(destination.parent)
        _copy_input(path, destination)
        if (sha256(destination) != basis["implementation_files"][name]
                or destination.stat().st_mode & 0o777 != basis["implementation_modes"][name]):
            raise ValueError("Implementation changed while snapshotting measurement inputs")
    for name, digest in harness.items():
        destination = workspace / "harness" / name
        _directory(destination.parent)
        _copy_input(evidence_path(root / "benchmarks/harness", name), destination)
        if sha256(destination) != digest or destination.stat().st_mode & 0o777 != harness_modes[name]:
            raise ValueError("Harness changed while snapshotting measurement inputs")
    if basis["tools"]["route"] == "lean_native":
        _directory(workspace / "implementation/.lake")
        _directory(workspace / "build/implementation-lake")
    for name, value in {"request.json": response, "basis.json": basis}.items():
        (run / name).write_bytes(canonical_json(value) + b"\n")
    env = _environment(basis["tools"])
    record = {"schema": "trellis-paper-performance-run/v1", "id": run_id, "basis": _hash(basis),
              "harness_sha256": _hash({"files": harness, "modes": harness_modes}), "sequence": state.get("sequence"), "reason": response["reason"],
              "environment": env, "machine": _machine(),
              "commands": [], "successful_run": False,
              "limits": {"batch_seconds": MAX_TIMEOUT, "command_log_bytes": MAX_LOG_BYTES,
                         "retained_output_bytes": MAX_OUTPUT_BYTES, "retained_executable_bytes": MAX_EXECUTABLE_BYTES,
                         "generated_hash_bytes": MAX_GENERATED_BYTES, "generated_files": MAX_GENERATED_FILES},
              "authority": "Host observations only; workload adequacy and intended-source invocation require performance review."}
    artifacts = {}
    deadline = time.monotonic() + MAX_TIMEOUT
    for index, command in enumerate(commands):
        stdout, stderr = run / f"{index:02d}-{command['name']}.stdout", run / f"{index:02d}-{command['name']}.stderr"
        build_inventory = _build_inventory(workspace)
        inventory_name = f"{index:02d}-generated-inputs.json"
        (run / inventory_name).write_bytes(canonical_json(build_inventory) + b"\n")
        artifacts[inventory_name] = sha256(run / inventory_name)
        if any("unhashed" in value for value in build_inventory.values()):
            record["evidence_error"] = "Generated inputs exceed the bounded inventory policy; inspect the generated-inputs report"
            break
        entry = {**command, "generated_inputs": inventory_name, "generated_inputs_sha256": _hash(build_inventory)}
        try:
            entry["executable"], executable = _executable(command, workspace, env, tool_roots)
            if entry["executable"].get("identity_error"):
                record["evidence_error"] = entry["executable"]["identity_error"]
        except ValueError as error:
            stdout.write_text("")
            stderr.write_text(str(error) + "\n")
            entry.update(exit_code=-1, timeout=False, host_elapsed_ns=0, stdout=stdout.name, stderr=stderr.name,
                         execution_error=str(error))
            record["commands"].append(entry)
            break
        if command["kind"] == "run" and executable is not None and executable.is_relative_to(workspace):
            name = f"artifacts/{index:02d}-entrypoint"
            if executable.stat().st_size > MAX_EXECUTABLE_BYTES:
                record["evidence_error"] = "Executed entrypoint exceeds the 128MiB retained-artifact limit"
            else:
                _directory((run / name).parent)
                _copy_input(executable, run / name)
                artifacts[name] = sha256(run / name)
                entry["retained_entrypoint"] = name
        started = time.perf_counter_ns()
        result = _run_bounded(_sandbox(workspace, tool_roots, env, command), cwd=workspace, env={},
                                    stdout=stdout, stderr=stderr, timeout=max(.001, min(command["timeout_seconds"], deadline - time.monotonic())))
        entry.update(exit_code=result["exit_code"], timeout=result["timeout"], host_elapsed_ns=time.perf_counter_ns() - started,
                     stdout=stdout.name, stderr=stderr.name, output_limit_exceeded=result.get("output_limit_exceeded", False),
                     execution_error=result.get("execution_error", ""))
        record["commands"].append(entry)
        if command["kind"] == "run" and result["exit_code"] == 0 and not result["timeout"] and not result.get("output_limit_exceeded"):
            record["successful_run"] = True
        if result["exit_code"] or result["timeout"] or result.get("output_limit_exceeded") or time.monotonic() >= deadline:
            break
    generated = _build_inventory(workspace)
    if any("unhashed" in value for value in generated.values()):
        record["evidence_error"] = "Generated artifacts exceed the bounded inventory policy; inspect generated-final.json"
    (run / "generated-final.json").write_bytes(canonical_json(generated) + b"\n")
    artifacts["generated-final.json"] = sha256(run / "generated-final.json")
    output_files = {name: value for name, value in generated.items() if name.startswith("output/")}
    if any("unhashed" in value for value in output_files.values()):
        record["evidence_error"] = "Measurement output exceeds the bounded inventory policy"
    elif any("symlink" in value for value in output_files.values()):
        record["evidence_error"] = "Measurement output contains aliases; use ordinary output files"
    elif sum((workspace / name).stat().st_size for name in output_files) > MAX_OUTPUT_BYTES:
        record["evidence_error"] = "Measurement output exceeds the 16MiB retained-output limit"
    else:
        for name in output_files:
            destination = run / "artifacts" / name
            _directory(destination.parent)
            _copy_input(workspace / name, destination)
            artifacts["artifacts/" + name] = sha256(destination)
    record["fresh_after_measurement"] = current_basis(root) == record["basis"] and harness_fingerprint(root) == record["harness_sha256"]
    evidence = {"request.json": sha256(run / "request.json"), "basis.json": sha256(run / "basis.json")}
    evidence.update(artifacts)
    evidence.update({"workspace/implementation/" + name: sha256(workspace / "implementation" / name) for name in inputs})
    evidence.update({"workspace/harness/" + name: sha256(workspace / "harness" / name) for name in harness})
    for entry in record["commands"]:
        for name in (entry["stdout"], entry["stderr"]):
            evidence[name] = sha256(run / name)
    record["evidence_files"] = evidence
    record["evidence_modes"] = {name: (run / name).stat().st_mode & 0o777 for name in evidence}
    path = run / "record.json"
    path.write_bytes(canonical_json(record) + b"\n")
    digest = sha256(path)
    (run / "record.sha256").write_text(digest + "\n")
    return {**record, "record_path": path.relative_to(root).as_posix(), "record_sha256": digest}


def recorded_evidence(root: Path, run_id: str) -> dict:
    """Verify a sealed observation, including failed or historical measurements."""
    if not isinstance(run_id, str) or not _RUN_ID.fullmatch(run_id):
        raise ValueError("Invalid recorded measurement ID")
    directory = root / STORE / "runs" / run_id
    path = evidence_path(root, (directory / "record.json").relative_to(root).as_posix())
    digest = sha256(path)
    if evidence_path(directory, "record.sha256").read_text().strip() != digest:
        raise ValueError("Recorded performance evidence changed")
    record = _json(path)
    if record.get("schema") != "trellis-paper-performance-run/v1" or record.get("id") != run_id:
        raise ValueError("Recorded measurement identity changed")
    for name, expected in record["evidence_files"].items():
        evidence = evidence_path(directory, name)
        if sha256(evidence) != expected or evidence.stat().st_mode & 0o777 != record["evidence_modes"][name]:
            raise ValueError("Performance inputs or raw observations changed")
    if _hash(_json(evidence_path(directory, "basis.json"))) != record["basis"]:
        raise ValueError("Recorded measurement basis changed")
    for entry in record["commands"]:
        if entry.get("retained_entrypoint") and record["evidence_files"][entry["retained_entrypoint"]] != entry.get("executable", {}).get("sha256"):
            raise ValueError("Retained measured executable does not match the command identity")
    actual_success = any(item["kind"] == "run" and item["exit_code"] == 0 and not item["timeout"]
                         and not item.get("output_limit_exceeded") for item in record["commands"])
    if actual_success != record["successful_run"]:
        raise ValueError("Performance receipt has inconsistent run completion")
    return {**record, "record_path": path.relative_to(root).as_posix(), "record_sha256": digest}


def validate_evidence(root: Path, state: dict, run_ids: list[str]) -> list[dict]:
    """Check current host receipts; an agent's acceptance flag supplies no evidence."""
    if (not isinstance(run_ids, list) or not run_ids or len(set(run_ids)) != len(run_ids)
            or any(not isinstance(name, str) or not _RUN_ID.fullmatch(name) for name in run_ids)):
        raise ValueError("Performance evidence needs distinct recorded measurement IDs")
    payload, _, tool_roots = _basis(root)
    basis, harness, env = _hash(payload), harness_fingerprint(root), _environment(payload["tools"])
    records = []
    for run_id in run_ids:
        directory = root / STORE / "runs" / run_id
        record = recorded_evidence(root, run_id)
        if (record.get("basis") != basis or record.get("harness_sha256") != harness
                or not record.get("fresh_after_measurement")):
            raise ValueError("Performance evidence is stale for the current implementation or harness")
        if record.get("evidence_error"):
            raise ValueError(f"Performance evidence is incomplete: {record['evidence_error']}")
        for entry in record["commands"]:
            executable = entry.get("executable", {})
            if executable.get("external_path"):
                observed, _ = _executable(entry, directory / "workspace", env, tool_roots)
                if any(observed.get(key) != executable.get(key) for key in ("sha256", "external_path")):
                    raise ValueError("Measured external executable changed")
        records.append(record)
    if not any(record["successful_run"] for record in records):
        raise ValueError("Performance acceptance requires at least one successful measured run")
    return records
