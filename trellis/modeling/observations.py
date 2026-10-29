"""Mechanical extraction observations for setup and adaptation."""
from __future__ import annotations

import difflib
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess

from trellis.pv_execution import execution_timeout, semantic_tools

from scripts.extract_pv_model import (_strip_lean_comments, SOURCE_INPUT_IGNORED,
                                    copy_source_inputs, source_input_manifest)
from trellis.json_io import save_json as write_json

IGNORED = SOURCE_INPUT_IGNORED


def files(root: Path) -> dict[str, bytes]:
    return {
        item["relative_path"]: (root / item["relative_path"]).read_bytes()
        for item in source_input_manifest(root)["files"]
    }


def fingerprint(root: Path) -> str:
    digest = hashlib.sha256()
    for item in source_input_manifest(root)["files"]:
        digest.update(item["relative_path"].encode() + b"\0" + bytes.fromhex(item["sha256_of_raw_bytes"]))
    return digest.hexdigest()


def read_json(path: Path) -> dict:
    value = json.loads(path.read_text())
    if not isinstance(value, dict):
        raise ValueError(f"Expected an object in {path}")
    return value


def inside(root: Path, name: str) -> Path:
    p = (root / name).resolve()
    if not p.is_relative_to(root.resolve()):
        raise ValueError(f"Path leaves its workspace: {name}")
    return p


def source_diff(root: Path) -> str:
    old, new = files(root / "upstream"), files(root / "work" / "crate")
    return "".join(
        "".join(difflib.unified_diff(
            old.get(n, b"").decode(errors="replace").splitlines(True),
            new.get(n, b"").decode(errors="replace").splitlines(True),
            fromfile="upstream/" + n, tofile="work/crate/" + n,
        )) for n in sorted(old.keys() | new.keys()) if old.get(n) != new.get(n)
    )


def log_diagnostics(log: Path, *, stage: str, module: str | None = None) -> list[dict]:
    from trellis.modeling.modules import attach_diagnostics

    clean = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", log.read_text(errors="replace"))
    lines = clean.splitlines()
    starts = [i for i, line in enumerate(lines)
              if re.match(r"\s*(?:\[(?:error|warn(?:ing)?)\s*\]|"
                          r"(?:.*?:\d+:\d+:\s*)?(?:error|warning)\b|Could not translate\b)", line, re.I)]
    result = []
    for i, end in zip(starts, [*starts[1:], len(lines)]):
        item = {"stage": stage, "module": module, "log": str(log),
                "line": i + 1, "text": lines[i][:1200]}
        context = "\n".join(lines[i + 1:end]).strip()
        if context:
            item["context"] = context
        result.append(item)
    # Retain source metadata even when it follows details omitted from the preview.
    attach_diagnostics(result, [])
    for item in result:
        if "context" in item:
            item["context"] = "\n".join(item["context"].splitlines()[:12])[:1200]
    return result


def observe(root: Path, output: Path, *, cache_root: Path | None = None,
            read_caches: list[Path] = (), through: str = "lean", timeout: float | None = None,
            history_root: Path | None = None) -> dict:
    from trellis.modeling.cache import Cache, key
    from trellis.pv_extraction_config import extraction_arguments
    from trellis.pv_lean_resources import generated_elaboration_options
    from trellis.modeling.environment import environment_signature, identify_tools, run_source_command, source_environment
    from trellis.pv_toolchain import effective_tools
    from trellis.modeling.modules import assemble, attach_diagnostics, declaration_index, imports, module_files, order_modules
    from trellis.modeling.axioms import SCHEMA, observe_module, probe_identity
    import time

    from trellis.modeling.history import HISTORY, capture_inputs, record_observation, start_attempt

    history_inputs = capture_inputs(root)
    tools = effective_tools(root)
    work = root / "work"
    history_inputs.update({f"inputs/source/{name}": raw for name, raw in files(work / "crate").items()})
    history_attempt = start_attempt((history_root or root) / HISTORY, "observation",
                                    inputs=history_inputs, metadata={"output_path": str(output), "through": through})
    output.mkdir(parents=True, exist_ok=True)
    cache = Cache(cache_root or root / ".trellis/modeling/cache", read_caches)
    identities = identify_tools(tools, timeout=timeout)
    environment = key(semantic_tools(tools), identities, environment_signature(
        tools, cache_path=cache.writable / "resource-digests.json",
        read_caches=[path / "resource-digests.json" for path in read_caches]), "pv-preparation-v2")
    result = {"work_fingerprint": fingerprint(work), "preparation_checks": probe_identity(),
              "steps": [], "error": "", "stable": True,
              "source_diff": str(output / "source.diff"), "diagnostics": [], "environment": environment, "tool_identities": identities,
              "execution_timeouts": {name: execution_timeout(tools, name, timeout) for name in ("cargo", "charon", "aeneas", "lean")}}
    (output / "source.diff").write_text(source_diff(root))
    env = source_environment(tools, output)

    def run(name: str, argv: list[str], cwd: Path, *, identity: str, artifacts: dict[str, Path],
            module: str | None = None) -> bool:
        label = name + ("-" + module if module else "")
        log = output / f"{label}.log"
        stage = "lean" if module else name
        cached = cache.get(stage, identity)
        started = time.monotonic()
        if cached:
            for filename, destination in artifacts.items():
                destination.parent.mkdir(parents=True, exist_ok=True)
                if (cached / filename).is_dir():
                    shutil.copytree(cached / filename, destination, dirs_exist_ok=True)
                else:
                    shutil.copy2(cached / filename, destination)
            shutil.copy2(cached / "log", log)
            code = 0
        else:
            execution = run_source_command(argv, cwd=cwd, env=env, stdout=log,
                                           timeout=execution_timeout(tools, name, timeout))
            code = execution["exit_code"]
            if code == 0:
                cache.put(stage, identity, {**artifacts, "log": log})
        step = {"name": name, "exit_code": code, "log": str(log), "argv": argv,
                "cached": bool(cached), "seconds": round(time.monotonic() - started, 3), "key": identity,
                "timeout": False if cached else execution["timeout"],
                "timeout_seconds": execution_timeout(tools, name, timeout)}
        if not cached:
            step["execution"] = execution
        if module:
            step["module"] = module
        result["steps"].append(step)
        result["diagnostics"].extend(log_diagnostics(log, stage=name, module=module))
        return code == 0

    try:
        plan = read_json(work / "extraction.json")
        crate = inside(work, plan.get("crate", "crate"))
        if crate != (work / "crate").resolve():
            raise ValueError("The candidate crate root is work/crate")
        arguments = extraction_arguments(plan, emit_json=bool(identities.get("aeneas", {}).get("emit_json")))
        result["extraction_arguments"] = arguments
        snapshot = output / "source"
        copy_source_inputs(crate, snapshot)
        history_inputs.update({f"inputs/source/{name}": raw for name, raw in files(snapshot).items()})
        cargo_args = arguments["cargo_args"]
        cargo_key = key(environment, fingerprint(snapshot), cargo_args)
        if not run("cargo", [tools["cargo"], "check", "--offline", *cargo_args], snapshot,
                   identity=cargo_key, artifacts={}):
            return result
        if through == "cargo":
            return result
        generated = output / "Generated"
        generated.mkdir()
        llbc = generated / "m.llbc"
        charon_args = arguments["charon_args"]
        charon_key = key(cargo_key, charon_args)
        if not run("charon", [tools["charon"], "cargo", "--preset=aeneas", "--dest-file", str(llbc),
                              *charon_args, "--", "--offline", *cargo_args], snapshot,
                   identity=charon_key, artifacts={"m.llbc": llbc}):
            return result
        if through == "charon":
            return result
        aeneas_args = arguments["aeneas_args"]
        aeneas_key = key(environment, hashlib.sha256(llbc.read_bytes()).hexdigest(), aeneas_args)
        ok = run("aeneas", [tools["aeneas"], "-backend", "lean", "-dest", str(generated),
                             *aeneas_args, str(llbc)], snapshot,
                 identity=aeneas_key, artifacts={"Generated": generated})
        leans = sorted(generated.rglob("*.lean"))
        result["raw_models"] = {".".join(p.relative_to(generated).with_suffix("").parts): str(p) for p in leans}
        index_path = output / "declarations.json"
        write_json(index_path, declaration_index(generated))
        result["declaration_index"] = str(index_path)
        if leans:
            result["raw_model"] = str(leans[0])
            active = module_files(generated)
            code = _strip_lean_comments("\n".join(p.read_text() for p in active.values()).splitlines())
            result["axioms"] = re.findall(r"\baxiom\s+([^\s(:]+)", code)
            result["placeholders"] = len(re.findall(r"\b(?:sorry|admit)\b", code))
        if not ok or through == "aeneas":
            return result
        composed = output / "Lean"
        from trellis.pv_toolchain import lean_imports
        selected_imports = lean_imports(root)
        result["lean_imports"] = selected_imports
        modules = assemble(generated, composed, lean_imports=selected_imports)
        result["elaboration_options"] = {name: options for name, path in module_files(generated).items()
                                         if (options := generated_elaboration_options(path.read_text()))}
        result["combined_model"] = str(composed / "Combined.lean")
        result["modules"] = {name: str(path) for name, path in modules.items()}
        result["remaining_axioms"] = re.findall(r"\baxiom\s+([^\s(:]+)", _strip_lean_comments(
            "\n".join(path.read_text() for path in modules.values()).splitlines()))
        result["composed_index"] = str(output / "model-declarations.json")
        composed_index = declaration_index(composed)
        write_json(output / "model-declarations.json", composed_index)
        env["LEAN_PATH"] = os.pathsep.join([str(composed), *tools["lean_paths"]])
        module_keys = {}
        result["axiom_dependencies"] = {
            "schema": SCHEMA, "probe_sha256": probe_identity(), "environment": environment,
            "raw_sha256": {name: hashlib.sha256(Path(path).read_bytes()).hexdigest()
                           for name, path in result["raw_models"].items()}, "modules": {}}
        for name in order_modules(modules):
            path = modules[name]
            dependencies = {dep: module_keys[dep] for dep in imports(path.read_text()) if dep in modules}
            identity = key(environment, name, path.read_text(), dependencies, "maxErrors=0")
            module_keys[name] = identity
            olean = path.with_suffix(".olean")
            compiled = run("lean", [tools["lean"], "-DmaxErrors=0", "-o", str(olean), str(path)], composed,
                           identity=identity, artifacts={"module.olean": olean}, module=name)
            if name in result["raw_models"]:
                inventory, step = observe_module(path, name, output=output, tools=tools, env=env,
                                                 environment=environment, dependencies=dependencies,
                                                 cache=cache, timeout=timeout)
                result["axiom_dependencies"]["modules"][name] = inventory
                result["steps"].append(step)
            if not compiled:
                break
    except (ValueError, KeyError, OSError, json.JSONDecodeError) as error:
        result["error"] = str(error)
    finally:
        result["elaboration_placeholders"] = sum(
            diagnostic["stage"] == "lean" and bool(re.search(r"declaration uses .*sorry", diagnostic["text"]))
            for diagnostic in result["diagnostics"])
        index_path = result.get("composed_index") or result.get("declaration_index")
        if index_path and Path(index_path).is_file():
            attach_diagnostics(result["diagnostics"], read_json(Path(index_path))["declarations"])
        if fingerprint(work) != result["work_fingerprint"]:
            result["error"] = "Workspace changed during observation; repeat the check"
            result["stable"] = False
        result["observation_file"] = str(output / "observation.json")
        write_json(output / "observation.json", result)
        record_observation(history_root or root, output, result, inputs=history_inputs,
                           attempt_start_id=history_attempt)
    return result


def passed(observation: dict, stage: str) -> bool:
    # A later compilation error does not negate a successful earlier step.
    steps = [step for step in observation.get("steps", []) if step["name"] == stage]
    return (observation.get("stable", True) and (stage != "lean" or not observation.get("error"))
            and (stage != "lean" or not observation.get("elaboration_placeholders"))
            and bool(steps) and all(step["exit_code"] == 0 for step in steps)
            and (stage != "lean" or not observation.get("modules")
                 or {step.get("module") for step in steps} == set(observation["modules"])))
