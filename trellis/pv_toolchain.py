"""Resolve setup choices against the supplied PV tool and library catalog."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

from trellis.pv_execution import execution_timeout, semantic_tools


def effective_tools(root: Path) -> dict:
    frozen = root / "modeling-input/manifest.json"
    if frozen.is_file():
        manifest = json.loads(frozen.read_text())
        tools = manifest.get("effective_tools")
        if not isinstance(tools, dict):
            raise ValueError("Native modeling manifest must contain effective_tools")
        return tools
    config = json.loads((root / "config.json").read_text())
    plan = json.loads((root / "work/extraction.json").read_text())
    supplied = config["tools"]
    tools = {key: value for key, value in supplied.items() if key != "profiles"}
    profile = plan.get("resource_profile", "default")
    if not isinstance(profile, str):
        raise ValueError("resource_profile must name a supplied tool profile")
    profiles = supplied.get("profiles", {})
    if profile != "default":
        if not isinstance(profiles, dict) or profile not in profiles or not isinstance(profiles[profile], dict):
            raise ValueError(f"Unknown supplied resource profile: {profile}")
        tools.update(profiles[profile])
    return tools


def lean_imports(root: Path) -> list[str]:
    plan = json.loads((root / "work/extraction.json").read_text())
    names = plan.get("lean_imports", [])
    if not isinstance(names, list) or any(not isinstance(name, str) or not re.fullmatch(
        r"[A-Za-z_][A-Za-z0-9_']*(?:\.[A-Za-z_][A-Za-z0-9_']*)*", name
    ) for name in names):
        raise ValueError("lean_imports must contain installed Lean module names")
    # These are installed resource selections, not imports of candidate code.
    if any(name == "Tablet" or name.startswith("Tablet.") for name in names):
        raise ValueError("Authored Tablet definitions are supplied during Modeling")
    return list(dict.fromkeys(names))


def setup_basis(root: Path, *, resource_identity: str | None = None) -> str:
    from trellis.modeling.environment import environment_signature

    # Ownership/rollback checks also run on incomplete or malformed edits.
    # Fingerprinting must remain defined; validation belongs to the check.
    path = root / "work/extraction.json"
    payload = {"plan_sha256": hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else "missing"}
    try:
        tools = effective_tools(root)
        resources = resource_identity if resource_identity is not None else environment_signature(
            tools, cache_path=root / ".trellis/modeling/cache/resource-digests.json")
        payload.update(tools=semantic_tools(tools), resources=resources, imports=lean_imports(root))
    except (ValueError, KeyError, OSError) as error:
        payload["invalid_configuration"] = str(error)
    return hashlib.sha256(json.dumps(payload, sort_keys=True).encode()).hexdigest()


def native_resources(tools: dict) -> dict:
    """Resolve the installed resources used by the ordinary Lake bootstrap."""
    from trellis.host_runtime import worker_elan_home

    supplied = tools.get("lean_paths", [])
    if not isinstance(supplied, list) or not supplied or any(not isinstance(p, str) or not p for p in supplied):
        raise ValueError("lean_paths must list installed Lean library directories")
    paths = list(dict.fromkeys(str(Path(p).resolve()) for p in supplied))
    if any(not Path(p).is_dir() for p in paths):
        raise ValueError("Every selected lean_paths directory must exist")
    aeneas = next((Path(p) for p in paths if (Path(p) / "Aeneas.olean").is_file()), None)
    if aeneas is None:
        raise ValueError("The selected libraries must contain compiled Aeneas.olean")
    package_path = tools.get("aeneas_lean_lib")
    if package_path is None:
        if len(aeneas.parents) < 4:
            raise ValueError("aeneas_lean_lib must identify the selected Aeneas Lake package")
        package_path = aeneas.parents[3]
    package = Path(package_path).resolve()
    if (package / ".lake/build/lib/lean").resolve() != aeneas or not any(
        (package / name).is_file() for name in ("lakefile.lean", "lakefile.toml")
    ):
        raise ValueError("The selected Aeneas library must identify its Lake package through aeneas_lean_lib")
    lean = Path(tools["lean"]).resolve()
    toolchain = tools.get("lean_toolchain", lean.parent.parent.name.replace("--", "/", 1).replace("---", ":", 1))
    if not isinstance(toolchain, str) or not toolchain.strip():
        raise ValueError("lean_toolchain must name the installed selected Lean toolchain")
    capability = os.environ.get("TRELLIS_COMPILER_CAPABILITY")
    if capability == "none":
        raise ValueError("this task has no Lean capability; report the need for Lean to the reviewer")
    if capability == "managed":
        from trellis.check_job import tool_info

        job = tool_info("elan", "which-lean")
        result = job.get("result") or {}
        if (job.get("state") != "succeeded" or job.get("current_inputs_match") is not True
                or result.get("returncode") != 0):
            raise ValueError("Managed toolchain metadata query did not succeed for the current workspace inputs")
        matches = result.get("toolchain") == toolchain and Path(result.get("stdout", "").strip()).resolve() == lean
    else:
        result = subprocess.run([str(worker_elan_home() / "bin/elan"), "which", "lean"],
                                env={**os.environ, "ELAN_HOME": str(worker_elan_home()), "ELAN_TOOLCHAIN": toolchain},
                                text=True, capture_output=True, timeout=execution_timeout(tools, "tool_probe", default=30))
        matches = result.returncode == 0 and Path(result.stdout.strip()).resolve() == lean
    if not matches:
        raise ValueError("lean_toolchain must resolve to the configured lean executable")
    return {"lean_paths": paths, "aeneas_lean_lib": str(package), "lean_toolchain": toolchain}


def native_resource_environment(tools: dict) -> dict[str, str]:
    from trellis.modeling.environment import tool_readonly_paths

    resources = native_resources(tools)
    return {"LEAN_PATH": os.pathsep.join(resources["lean_paths"]),
            "TRELLIS_PINNED_LEAN_LIBRARIES": os.pathsep.join(resources["lean_paths"]), "ELAN_TOOLCHAIN": resources["lean_toolchain"],
            "TRELLIS_SANDBOX_EXTRA_RO_PATHS": os.pathsep.join(
                dict.fromkeys([resources["aeneas_lean_lib"], *resources["lean_paths"], *tool_readonly_paths(tools)]))}


class NativeLibraryEnvironmentError(RuntimeError):
    """A frozen compiled-library environment failed, independently of authored nodes."""


def pinned_library_mounts(repo: Path, environment: dict | None = None) -> dict[Path, Path]:
    """Lake package aliases of the selected immutable compiled libraries."""
    roots = (environment or {}).get("TRELLIS_PINNED_LEAN_LIBRARIES", "")
    if not roots and (repo / "modeling-input/manifest.json").is_file():
        roots = os.pathsep.join(effective_tools(repo).get("lean_paths", []))
    mounts = {}
    for raw in roots.split(os.pathsep):
        if not raw:
            continue
        source = Path(raw).resolve()
        if len(source.parents) < 4 or source.parts[-4:] != (".lake", "build", "lib", "lean"):
            continue
        destination = repo / ".lake/packages" / source.parents[3].name / ".lake/build/lib/lean"
        if destination.is_dir() and destination.resolve() != source:
            mounts[destination.resolve()] = source
    return mounts


def check_library_resolution(repo: Path, tools: dict, environment: dict[str, str], *,
                             bwrap_role: str | None = None, timeout_secs: float | None = None) -> dict:
    """Reject Lake dependencies that shadow a selected compiled library."""
    from trellis.atomic_actions.observations import _require_socket_path, _run_lake_command

    timeout_secs = execution_timeout(tools, "library_resolution", timeout_secs, default=60)

    if bwrap_role is None:
        from trellis.atomic_actions.checker_client import client_native_library_resolution

        result = dict(client_native_library_resolution(_require_socket_path(), timeout_secs=timeout_secs))
        result.pop("request_id", None)
        if not result.get("matches"):
            raise NativeLibraryEnvironmentError(result.get("error", "Native library resolution failed"))
        return result

    resources = native_resources(tools)
    result = _run_lake_command(repo, ["env", "printenv", "LEAN_PATH"], timeout_secs=timeout_secs,
                               bwrap_role=bwrap_role, environment=environment)
    if result["returncode"] != 0:
        raise NativeLibraryEnvironmentError("Native Lake resource check failed: " + (result["stderr"] or result["stdout"]))
    paths = [Path(p).resolve() if Path(p).is_absolute() else (repo / p).resolve()
             for p in result["stdout"].strip().split(os.pathsep) if p]
    # Inspect the same library mounts used by the compiler sandbox.
    mounts = pinned_library_mounts(repo, environment)
    paths = [mounts.get(path, path) for path in paths]
    selected = {}
    for root in map(Path, resources["lean_paths"]):
        for path in sorted(root.rglob("*.olean")):
            selected.setdefault(path.relative_to(root), path)
    checked = 0
    for module, expected in selected.items():
        actual = next((root / module for root in paths if (root / module).is_file()), None)
        if actual is None:
            raise NativeLibraryEnvironmentError(f"Native Lake cannot resolve selected library module {module}")
        if not actual.samefile(expected):
            if actual.stat().st_size != expected.stat().st_size or actual.read_bytes() != expected.read_bytes():
                raise NativeLibraryEnvironmentError(f"Native Lake shadows selected library module {module}: {actual} differs from {expected}")
        checked += 1
    return {"lean_path": [str(p) for p in paths], "selected_modules_checked": checked,
            "approved_imports": sorted(".".join(module.with_suffix("").parts) for module in selected),
            "matches": True}


def bootstrap_configuration_errors(root: Path, observation: dict | None = None) -> list[str]:
    """Expose the existing exporter constraints before approving setup."""
    from trellis.pv_extraction_config import extraction_arguments

    try:
        tools = effective_tools(root)
        lean_imports(root)
        plan = json.loads((root / "work/extraction.json").read_text())
        args = extraction_arguments(plan)["aeneas_args"]
        if tools.get("lean"):
            native_resources(tools)
    except (ValueError, KeyError, OSError, subprocess.SubprocessError) as error:
        return [str(error)]
    unsupported = next((arg for arg in args if arg.split("=", 1)[0] in ("-split-files", "-subdir")), None)
    if unsupported:
        return [f"The current native exporter requires one root Lean module; omit {unsupported.split('=', 1)[0]}"]
    modules = (observation or {}).get("raw_models", {})
    if len(modules) > 1:
        return ["The current native exporter requires a single generated Lean module"]
    if any(name != Path(path).stem for name, path in modules.items()):
        return ["The current native exporter requires the generated Lean module at the extraction output root"]
    return []
