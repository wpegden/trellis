#!/usr/bin/env python3
"""Run the framework-owned Phase-0 source checker in sealed source copies."""

from __future__ import annotations

import base64
import hashlib
import json
import os
import resource
import shlex
import shutil
import stat
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import Any

from trellis.config import SandboxConfig
from trellis.sandbox import wrap_command

try:
    from scripts.pv_model_opacity import (
        read_model_bodiless_declarations,
        unscanned_model,
    )
    from scripts.pv_source_tree import (
        SourceTreeError,
        canonical_json,
        git_checkout_digest,
        readonly_closure_digest,
        source_tree_digest,
        source_tree_manifest,
    )
except ModuleNotFoundError:  # direct script execution
    from pv_model_opacity import (  # type: ignore[no-redef]
        read_model_bodiless_declarations,
        unscanned_model,
    )
    from pv_source_tree import (  # type: ignore[no-redef]
        SourceTreeError,
        canonical_json,
        git_checkout_digest,
        readonly_closure_digest,
        source_tree_digest,
        source_tree_manifest,
    )


REQUEST_SCHEMA = "trellis-pv-phase0-checker-request/v1"
OUTPUT_SCHEMA = "trellis-pv-phase0-checker-output/v1"
MAX_REQUEST_BYTES = 4 * 1024 * 1024
CHECK_KINDS = {"candidate", "ratification_replay", "entry_ablation"}
_REQUEST_KEYS = {
    "schema",
    "phase0_genesis_sha256",
    "check_id",
    "check_kind",
    "ledger_generation",
    "ledger_root_sha256",
    "candidate_root",
    "candidate_manifest",
    "candidate_tree_sha256",
    "repository_root",
    "goal_path",
    "goal_sha256",
    "reverted_entry_id",
    "runner_path",
    "runner_sha256",
    "extractor_path",
    "extractor_sha256",
    "filespec_checker_path",
    "filespec_checker_sha256",
    "prescribed_region_checker_path",
    "prescribed_region_checker_sha256",
    "cargo_path",
    "cargo_sha256",
    "rustc_path",
    "rustc_sha256",
    "toolchain_root",
    "charon_toolchain_root",
    "charon_toolchain_tree_sha256",
    "charon_path",
    "charon_sha256",
    "charon_source_root",
    "charon_source_tree_sha256",
    "aeneas_path",
    "aeneas_sha256",
    "aeneas_source_root",
    "aeneas_source_tree_sha256",
    "dependency_cache_path",
    "dependency_cache_tree_sha256",
    "opam_switch_prefix",
    "opam_switch_prefix_tree_sha256",
    "opam_switch_environment",
    "output_root",
    "output_root_binding_sha256",
    "target_triple",
    "extraction_profile",
    "opam_switch",
    "closed_environment",
    "run_id",
    "limits",
}
_LIMIT_KEYS = {
    "timeout_seconds",
    "cpu_seconds",
    "address_space_bytes",
    "file_size_bytes",
    "process_count",
    "stdout_bytes",
    "stderr_bytes",
}
_ALLOWED_ENV = {"LANG", "LC_ALL", "RUSTUP_TOOLCHAIN"}
_OPAM_SWITCH_ENV_KEYS = {
    "OPAM_SWITCH_PREFIX",
    "OPAMSWITCH",
    "CAML_LD_LIBRARY_PATH",
    "OCAML_TOPLEVEL_PATH",
    "OCAMLTOP_INCLUDE_PATH",
}
_OPAM_SWITCH_PATH_KEYS = _OPAM_SWITCH_ENV_KEYS - {"OPAMSWITCH"}
_LIMIT_MAXIMUMS = {
    "timeout_seconds": 3_600,
    "cpu_seconds": 3_600,
    "address_space_bytes": 64 * 1024 * 1024 * 1024,
    "file_size_bytes": 4 * 1024 * 1024 * 1024,
    "process_count": 4_096,
    "stdout_bytes": 1024 * 1024,
    "stderr_bytes": 1024 * 1024,
}


class CheckerFailure(ValueError):
    pass


class SandboxUnavailable(RuntimeError):
    pass


def _strict_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    output: dict[str, Any] = {}
    for key, value in pairs:
        if key in output:
            raise CheckerFailure(f"duplicate JSON member: {key}")
        output[key] = value
    return output


def _sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _string(value: dict[str, Any], field: str) -> str:
    item = value.get(field)
    if not isinstance(item, str) or not item:
        raise CheckerFailure(f"{field} must be a non-empty string")
    return item


def _digest(value: dict[str, Any], field: str) -> str:
    item = _string(value, field)
    if len(item) != 64 or any(character not in "0123456789abcdef" for character in item):
        raise CheckerFailure(f"{field} must be a lowercase SHA-256 digest")
    if item == "0" * 64:
        raise CheckerFailure(f"{field} must not be the zero digest")
    return item


def _positive(value: dict[str, Any], field: str) -> int:
    item = value.get(field)
    if not isinstance(item, int) or isinstance(item, bool) or item <= 0:
        raise CheckerFailure(f"limits.{field} must be a positive integer")
    return item


def _regular(path: Path, label: str) -> Path:
    try:
        mode = path.lstat().st_mode
    except OSError as exc:
        # No path in the message: it would carry the check id, and a quoted
        # diagnostic must recur verbatim when the entry is ablated in a later
        # check. The receipt's cwd already records where the file was sought.
        raise CheckerFailure(f"{label} is unavailable: {exc.strerror or 'cannot stat'}") from exc
    if stat.S_ISLNK(mode) or not stat.S_ISREG(mode):
        raise CheckerFailure(f"{label} must be a non-symlink regular file")
    return path.resolve(strict=True)


def _directory(path: Path, label: str) -> Path:
    try:
        mode = path.lstat().st_mode
    except OSError as exc:
        # No path in the message: it would carry the check id, and a quoted
        # diagnostic must recur verbatim when the entry is ablated in a later
        # check. The receipt's cwd already records where the file was sought.
        raise CheckerFailure(f"{label} is unavailable: {exc.strerror or 'cannot stat'}") from exc
    if stat.S_ISLNK(mode) or not stat.S_ISDIR(mode):
        raise CheckerFailure(f"{label} must be a non-symlink directory")
    return path.resolve(strict=True)


def _file_digest(path: Path, expected: str, label: str) -> Path:
    resolved = _regular(path, label)
    if _sha(resolved.read_bytes()) != expected:
        raise CheckerFailure(f"{label} differs from its pinned digest")
    return resolved


def _tree_digest(path: Path, expected: str, label: str) -> Path:
    resolved = _directory(path, label)
    try:
        actual = source_tree_digest(resolved)
    except SourceTreeError as exc:
        raise CheckerFailure(f"{label} is invalid: {exc}") from exc
    if actual != expected:
        raise CheckerFailure(f"{label} differs from its pinned tree digest")
    return resolved


def _readonly_tree_digest(path: Path, expected: str, label: str) -> Path:
    resolved = _directory(path, label)
    try:
        actual = readonly_closure_digest(resolved)
    except SourceTreeError as exc:
        raise CheckerFailure(f"{label} is invalid: {exc}") from exc
    if actual != expected:
        raise CheckerFailure(f"{label} differs from its pinned tree digest")
    return resolved


def output_root_binding(path: Path) -> str:
    return _sha(str(path.resolve()).encode("utf-8"))


def _paths_overlap(left: Path, right: Path) -> bool:
    return left == right or left.is_relative_to(right) or right.is_relative_to(left)


def _validate_opam_switch_environment(
    raw: Any, prefix: Path, switch: str
) -> dict[str, str]:
    if not isinstance(raw, dict) or set(raw) != _OPAM_SWITCH_ENV_KEYS:
        raise CheckerFailure(
            "opam_switch_environment has missing or unapproved variables"
        )
    if any(
        not isinstance(key, str)
        or not isinstance(value, str)
        or "\0" in key + value
        for key, value in raw.items()
    ):
        raise CheckerFailure("opam_switch_environment contains an invalid entry")
    if raw["OPAM_SWITCH_PREFIX"] != str(prefix) or raw["OPAMSWITCH"] != switch:
        raise CheckerFailure(
            "opam_switch_environment disagrees with the mounted switch prefix"
        )
    for key in sorted(_OPAM_SWITCH_PATH_KEYS):
        values = raw[key].split(os.pathsep)
        if not values or any(not value for value in values):
            raise CheckerFailure(f"opam switch variable {key} has an empty path")
        for value in values:
            try:
                resolved = Path(value).resolve(strict=True)
            except OSError as exc:
                raise CheckerFailure(
                    f"opam switch variable {key} names an unavailable path: {value}"
                ) from exc
            if not resolved.is_relative_to(prefix):
                raise CheckerFailure(
                    f"opam switch variable {key} escapes the mounted switch prefix"
                )
    return {key: raw[key] for key in sorted(raw)}


def _task_count() -> int:
    uid = os.getuid()
    total = 0
    for entry in os.scandir("/proc"):
        if not entry.name.isdigit():
            continue
        try:
            if entry.stat().st_uid != uid:
                continue
            with open(f"/proc/{entry.name}/status", "rb") as stream:
                for line in stream:
                    if line.startswith(b"Threads:"):
                        total += int(line.split()[1])
                        break
        except (OSError, ValueError, IndexError):
            continue
    return total


def _resource_limits(limits: dict[str, int]):
    # RLIMIT_NPROC is uid-wide and counts tasks, including the baseline outside
    # this check. The request therefore limits only the additional task delta.
    task_limit = _task_count() + limits["process_count"]

    def apply() -> None:
        resource.setrlimit(resource.RLIMIT_CPU, (limits["cpu_seconds"], limits["cpu_seconds"]))
        resource.setrlimit(resource.RLIMIT_AS, (limits["address_space_bytes"], limits["address_space_bytes"]))
        resource.setrlimit(resource.RLIMIT_FSIZE, (limits["file_size_bytes"], limits["file_size_bytes"]))
        resource.setrlimit(resource.RLIMIT_NPROC, (task_limit, task_limit))

    return apply


def _base_output(request: dict[str, Any]) -> dict[str, Any]:
    return {
        "schema": OUTPUT_SCHEMA,
        "status": "failed",
        "attempted": True,
        "phase0_genesis_sha256": request.get("phase0_genesis_sha256", ""),
        "check_id": request.get("check_id", ""),
        "check_kind": request.get("check_kind", ""),
        "ledger_generation": request.get("ledger_generation", 0),
        "ledger_root_sha256": request.get("ledger_root_sha256", ""),
        "candidate_tree_sha256": request.get("candidate_tree_sha256", ""),
        "goal_sha256": request.get("goal_sha256", ""),
        "reverted_entry_id": request.get("reverted_entry_id"),
        "sandbox_mounts": {"read_only": [], "writable": [], "network": "unshared"},
        "stages": [],
        "first_failing_stage": None,
        "llbc_root_sha256": None,
        "generated_lean_root_sha256": None,
        "tablet_root_sha256": None,
        "output_tree_sha256": None,
        "cargo_lock_sha256": None,
        "source_facts": {
            "candidate_tree_sha256": request.get("candidate_tree_sha256", ""),
            "compile_initial_tree_sha256": None,
            "compile_final_tree_sha256": None,
            "compile_cargo_lock_sha256": None,
            "extraction_initial_tree_sha256": None,
            "extraction_final_tree_sha256": None,
            "extraction_cargo_lock_sha256": None,
        },
        "a_b_generated_lean_roots": [],
        "a_b_tablet_roots": [],
        # What the extracted model leaves without a definition. Derived from
        # the model the extraction just produced, never from the ledger's
        # prose or from any list of source markings, so a computation that was
        # skipped rather than modelled is a checker fact like any other.
        "model_bodiless_declarations": unscanned_model(
            "not_extracted", "no extraction produced a model in this check"
        ),
        "goal_binding_report_sha256": None,
        "reason": "",
    }


def _internal_stage(name: str, cwd: Path, binary_digest: str, *, passed: bool, detail: bytes = b"") -> dict[str, Any]:
    return {
        "name": name,
        "tool": "phase0-checker",
        "command": [],
        "cwd": str(cwd),
        "environment": {},
        "binary_sha256": binary_digest,
        "sandbox_argv": [],
        "status": "passed" if passed else "failed",
        "exit_code": 0 if passed else None,
        "timed_out": False,
        "stdout_base64": "",
        "stdout_byte_length": 0,
        "stdout_sha256": _sha(b""),
        "stdout_truncated": False,
        "stderr_base64": base64.b64encode(detail).decode("ascii"),
        "stderr_byte_length": len(detail),
        "stderr_sha256": _sha(detail),
        "stderr_truncated": False,
    }


def _read_capped(path: Path, limit: int) -> tuple[bytes, bool]:
    with path.open("rb") as stream:
        data = stream.read(limit + 1)
    return data[:limit], len(data) > limit


def _run_command(
    *,
    name: str,
    tool: str,
    command: list[str],
    cwd: Path,
    environment: dict[str, str],
    binary_sha256: str,
    writable: Path,
    readonly: list[Path],
    limits: dict[str, int],
    log_root: Path,
    ordinal: int,
    sandbox_enabled: bool,
) -> dict[str, Any]:
    previous = {key: os.environ.get(key) for key in (
        "TRELLIS_SANDBOX_EXTRA_RO_PATHS", "PATH", "HOME", "CARGO_HOME", "RUSTC", "RUSTDOC",
        "TMPDIR", "TMP", "TEMP", "LANG", "LC_ALL", "RUSTUP_TOOLCHAIN",
        "OPAM_SWITCH_PREFIX", "OPAMSWITCH", "CAML_LD_LIBRARY_PATH",
        "OCAML_TOPLEVEL_PATH", "OCAMLTOP_INCLUDE_PATH", "CHARON_TOOLCHAIN_IS_IN_PATH",
        "LD_LIBRARY_PATH", "CARGO_NET_OFFLINE",
    )}
    try:
        # wrap_command serializes the allowed host variables into bwrap's
        # --setenv arguments.  Clear the whole controlled set first so an
        # ambient value cannot enter the sandbox without appearing in the
        # stage's recorded environment.
        for key in previous:
            if key != "TRELLIS_SANDBOX_EXTRA_RO_PATHS":
                os.environ.pop(key, None)
        os.environ["TRELLIS_SANDBOX_EXTRA_RO_PATHS"] = os.pathsep.join(str(path) for path in readonly)
        for key, value in environment.items():
            os.environ[key] = value
        sandbox_argv = wrap_command(
            command,
            sandbox=SandboxConfig(enabled=sandbox_enabled, backend="bwrap"),
            work_dir=cwd,
            burst_home=writable,
            role="source_adaptation_checker",
        )
    finally:
        for key, value in previous.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
    stdout_path = log_root / f"{ordinal:02d}-{name}.stdout"
    stderr_path = log_root / f"{ordinal:02d}-{name}.stderr"
    with stdout_path.open("xb") as stdout_file, stderr_path.open("xb") as stderr_file:
        process = subprocess.Popen(
            sandbox_argv,
            cwd=str(cwd),
            stdin=subprocess.DEVNULL,
            stdout=stdout_file,
            stderr=stderr_file,
            close_fds=True,
            env={} if sandbox_enabled else environment,
            preexec_fn=_resource_limits(limits),
        )
        try:
            exit_code = process.wait(timeout=limits["timeout_seconds"])
            timed_out = False
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
            exit_code = None
            timed_out = True
    stdout, stdout_truncated = _read_capped(stdout_path, limits["stdout_bytes"])
    stderr, stderr_truncated = _read_capped(stderr_path, limits["stderr_bytes"])
    if sandbox_enabled and exit_code not in (0, None) and b"bwrap:" in stderr and (
        b"namespace" in stderr or b"permission" in stderr.lower() or b"not permitted" in stderr.lower()
    ):
        raise SandboxUnavailable("source-adaptation checker sandbox is unavailable")
    passed = exit_code == 0 and not timed_out and not stdout_truncated and not stderr_truncated
    return {
        "name": name,
        "tool": tool,
        "command": command,
        "cwd": str(cwd),
        "environment": dict(sorted(environment.items())),
        "binary_sha256": binary_sha256,
        "sandbox_argv": sandbox_argv if sandbox_enabled else [],
        "status": "passed" if passed else "failed",
        "exit_code": exit_code,
        "timed_out": timed_out,
        "stdout_base64": base64.b64encode(stdout).decode("ascii"),
        "stdout_byte_length": len(stdout),
        "stdout_sha256": _sha(stdout),
        "stdout_truncated": stdout_truncated,
        "stderr_base64": base64.b64encode(stderr).decode("ascii"),
        "stderr_byte_length": len(stderr),
        "stderr_sha256": _sha(stderr),
        "stderr_truncated": stderr_truncated,
    }


def _typed_records(path: Path, sandbox_argv: list[str], limits: dict[str, int]) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    value = json.loads(path.read_bytes(), object_pairs_hook=_strict_object)
    if not isinstance(value, dict) or value.get("schema") != "trellis-pv-extractor-typed-result/v1":
        raise CheckerFailure("extractor typed result has the wrong schema")
    records = []
    mapping = {
        "rustc-version": "rust_identity",
        "charon": "charon",
        "cargo-metadata": "cargo_metadata_cfg",
        "cargo-rustc-cfg": "cargo_metadata_cfg",
        "aeneas": "aeneas",
        "cargo-build-kernel-example": "filespec_region_gates",
        "kernel-scan-tablet-filespec": "filespec_region_gates",
        "kernel-print-prescribed-region": "filespec_region_gates",
    }
    for raw in value.get("steps", []):
        if not isinstance(raw, dict):
            raise CheckerFailure("extractor typed result has a malformed step")
        stdout = base64.b64decode(raw["stdout_base64"], validate=True)
        stderr = base64.b64decode(raw["stderr_base64"], validate=True)
        stdout_truncated = len(stdout) > limits["stdout_bytes"]
        stderr_truncated = len(stderr) > limits["stderr_bytes"]
        stdout = stdout[: limits["stdout_bytes"]]
        stderr = stderr[: limits["stderr_bytes"]]
        argv = raw.get("argv")
        if not isinstance(argv, list) or not argv or not all(isinstance(item, str) for item in argv):
            raise CheckerFailure("extractor typed step has invalid argv")
        executable = _regular(Path(argv[0]), "typed step executable")
        status = "passed" if raw.get("exit_status") == 0 and not stdout_truncated and not stderr_truncated else "failed"
        records.append({
            "name": mapping.get(raw.get("tool"), "filespec_region_gates"),
            "tool": raw.get("tool"),
            "command": argv,
            "cwd": raw.get("cwd"),
            "environment": raw.get("environment"),
            "binary_sha256": _sha(executable.read_bytes()),
            "sandbox_argv": sandbox_argv,
            "status": status,
            "exit_code": raw.get("exit_status"),
            "timed_out": False,
            "stdout_base64": base64.b64encode(stdout).decode("ascii"),
            "stdout_byte_length": len(stdout),
            "stdout_sha256": _sha(stdout),
            "stdout_truncated": stdout_truncated,
            "stderr_base64": base64.b64encode(stderr).decode("ascii"),
            "stderr_byte_length": len(stderr),
            "stderr_sha256": _sha(stderr),
            "stderr_truncated": stderr_truncated,
        })
    return records, value


def _second_extraction_records(
    evidence_root: Path,
    sandbox_argv: list[str],
    limits: dict[str, int],
) -> list[dict[str, Any]]:
    command = json.loads((evidence_root / "COMMAND.json").read_bytes(), object_pairs_hook=_strict_object)
    result = json.loads((evidence_root / "RESULT.json").read_bytes(), object_pairs_hook=_strict_object)
    stdout_all = (evidence_root / "STDOUT.bin").read_bytes()
    stderr_all = (evidence_root / "STDERR.bin").read_bytes()
    command_runs = command.get("runs", [])
    result_runs = result.get("runs", [])
    records: list[dict[str, Any]] = []
    for command_run, result_run in zip(command_runs, result_runs, strict=True):
        if command_run.get("run_role") != "determinism_replay":
            continue
        for command_step, result_step in zip(command_run.get("steps", []), result_run.get("steps", []), strict=True):
            stdout_offset = result_step["stdout_offset"]
            stderr_offset = result_step["stderr_offset"]
            stdout = stdout_all[stdout_offset:stdout_offset + result_step["stdout_byte_length"]]
            stderr = stderr_all[stderr_offset:stderr_offset + result_step["stderr_byte_length"]]
            stdout_truncated = len(stdout) > limits["stdout_bytes"]
            stderr_truncated = len(stderr) > limits["stderr_bytes"]
            stdout = stdout[:limits["stdout_bytes"]]
            stderr = stderr[:limits["stderr_bytes"]]
            argv = command_step["argv"]
            executable = _regular(Path(argv[0]), "second extraction executable")
            status = "passed" if result_step["exit_status"] == 0 and not stdout_truncated and not stderr_truncated else "failed"
            records.append({
                "name": "second_clean_extraction",
                "tool": command_step["tool"],
                "command": argv,
                "cwd": command_step["working_directory"],
                "environment": command_step["effective_environment"],
                "binary_sha256": _sha(executable.read_bytes()),
                "sandbox_argv": sandbox_argv,
                "status": status,
                "exit_code": result_step["exit_status"],
                "timed_out": False,
                "stdout_base64": base64.b64encode(stdout).decode("ascii"),
                "stdout_byte_length": len(stdout),
                "stdout_sha256": _sha(stdout),
                "stdout_truncated": stdout_truncated,
                "stderr_base64": base64.b64encode(stderr).decode("ascii"),
                "stderr_byte_length": len(stderr),
                "stderr_sha256": _sha(stderr),
                "stderr_truncated": stderr_truncated,
            })
    return records


def _standalone_package(root: Path) -> str:
    manifest = root / "Cargo.toml"
    try:
        manifest = _regular(manifest, "standalone Cargo.toml")
        value = tomllib.loads(manifest.read_text(encoding="utf-8"))
    except (CheckerFailure, OSError, UnicodeError, tomllib.TOMLDecodeError) as exc:
        return str(exc)
    if not isinstance(value.get("package"), dict):
        return "Cargo.toml does not define one standalone [package]"
    if not (root / "Cargo.lock").is_file():
        # Without a lockfile cargo would generate one into the sealed source
        # copy, which is read-only, and fail with an opaque EROFS. Say what
        # the candidate is missing instead.
        return (
            "standalone Cargo.lock is unavailable: the sealed source copy is "
            "read-only, so the candidate must carry the package's Cargo.lock"
        )
    package = value["package"]
    if "build" in package or "build" in value or (root / "build.rs").exists():
        return "standalone package must not define or contain a build script"
    if (root / ".cargo").exists():
        return "standalone package must not contain .cargo configuration"
    if any(key in value for key in ("patch", "replace", "workspace")):
        return "standalone package must not define patch, replace, or workspace tables"
    def dependency_table(item: Any) -> bool:
        return isinstance(item, dict) and any(
            key == "dependencies"
            or key.endswith("-dependencies")
            or dependency_table(child)
            for key, child in item.items()
        )

    if dependency_table(value):
        return "standalone package must not define dependencies"
    lib = value.get("lib") if isinstance(value.get("lib"), dict) else {}
    crate_types = lib.get("crate-type", [])
    if lib.get("proc-macro") is True or (
        isinstance(crate_types, list) and "proc-macro" in crate_types
    ):
        return "standalone package must not define a proc-macro crate"
    lib_path = lib.get("path", "src/lib.rs")
    if not isinstance(lib_path, str) or not lib_path or "\\" in lib_path or Path(lib_path).is_absolute() or ".." in Path(lib_path).parts:
        return "Cargo.toml has an invalid standalone library path"
    try:
        _regular(root / lib_path, "standalone library source")
    except CheckerFailure as exc:
        return str(exc)
    return ""


def _dep_info_outside_closure(target: Path, source: Path, sysroot: Path) -> str:
    """Return the first rustc input outside sealed source or the pinned sysroot."""

    dep_files = sorted(target.rglob("*.d"), key=lambda path: os.fsencode(path))
    if not dep_files:
        return "cargo check produced no dep-info files"
    for dep_file in dep_files:
        try:
            logical = dep_file.read_text(encoding="utf-8").replace("\\\n", "")
        except (OSError, UnicodeError) as exc:
            return f"cannot read cargo dep-info {dep_file}: {exc}"
        for line in logical.splitlines():
            if ":" not in line:
                continue
            _targets, dependencies = line.split(":", 1)
            try:
                paths = shlex.split(dependencies, posix=True)
            except ValueError as exc:
                return f"cannot parse cargo dep-info {dep_file}: {exc}"
            for raw in paths:
                path = Path(raw)
                resolved = (path if path.is_absolute() else source / path).resolve()
                if not resolved.is_relative_to(source) and not resolved.is_relative_to(sysroot):
                    return f"cargo dep-info names input outside sealed source/sysroot: {resolved}"
    return ""


def run(request: dict[str, Any], *, sandbox_enabled: bool = True) -> dict[str, Any]:
    result = _base_output(request)
    if set(request) != _REQUEST_KEYS or request.get("schema") != REQUEST_SCHEMA:
        raise CheckerFailure("checker request has missing/unknown fields or the wrong schema")
    for field in ("phase0_genesis_sha256", "ledger_root_sha256", "candidate_tree_sha256", "goal_sha256", "runner_sha256", "extractor_sha256", "filespec_checker_sha256", "prescribed_region_checker_sha256", "cargo_sha256", "rustc_sha256", "charon_toolchain_tree_sha256", "charon_sha256", "charon_source_tree_sha256", "aeneas_sha256", "aeneas_source_tree_sha256", "dependency_cache_tree_sha256", "opam_switch_prefix_tree_sha256", "output_root_binding_sha256"):
        _digest(request, field)
    for field in ("check_id", "run_id", "target_triple", "extraction_profile", "opam_switch"):
        value = _string(request, field)
        if len(value) > 160 or any(not (character.isalnum() or character in "-_.") for character in value):
            raise CheckerFailure(f"{field} is not a safe closed identifier")
    if request["check_kind"] not in CHECK_KINDS:
        raise CheckerFailure("check_kind is invalid")
    reverted = request["reverted_entry_id"]
    if (request["check_kind"] == "entry_ablation") != isinstance(reverted, str):
        raise CheckerFailure("reverted_entry_id must be present exactly for entry_ablation")
    if not isinstance(request["ledger_generation"], int) or isinstance(request["ledger_generation"], bool) or request["ledger_generation"] < 0:
        raise CheckerFailure("ledger_generation must be a natural number")
    limits_raw = request["limits"]
    if not isinstance(limits_raw, dict) or set(limits_raw) != _LIMIT_KEYS:
        raise CheckerFailure("limits has missing or unknown fields")
    limits = {field: _positive(limits_raw, field) for field in _LIMIT_KEYS}
    for field, value in limits.items():
        if value > _LIMIT_MAXIMUMS[field]:
            raise CheckerFailure(f"limits.{field} exceeds the checker maximum")
    closed = request["closed_environment"]
    if not isinstance(closed, dict) or not set(closed) <= _ALLOWED_ENV:
        raise CheckerFailure("closed_environment contains an unapproved key")
    if any(not isinstance(key, str) or not isinstance(value, str) or "\0" in key + value for key, value in closed.items()):
        raise CheckerFailure("closed_environment contains an invalid entry")

    runner = _file_digest(Path(_string(request, "runner_path")), request["runner_sha256"], "checker runner")
    if runner != Path(__file__).resolve():
        raise CheckerFailure("runner_path does not name this exact checker runner")
    extractor = _file_digest(Path(_string(request, "extractor_path")), request["extractor_sha256"], "extractor")
    filespec_checker = _file_digest(
        Path(_string(request, "filespec_checker_path")),
        request["filespec_checker_sha256"],
        "filespec checker",
    )
    prescribed_region_checker = _file_digest(
        Path(_string(request, "prescribed_region_checker_path")),
        request["prescribed_region_checker_sha256"],
        "prescribed-region checker",
    )
    if not os.access(filespec_checker, os.X_OK) or not os.access(
        prescribed_region_checker, os.X_OK
    ):
        raise CheckerFailure("pinned kernel gate checker is not executable")
    repository = _directory(Path(_string(request, "repository_root")), "pinned repository")
    if not runner.is_relative_to(repository) or not extractor.is_relative_to(repository):
        raise CheckerFailure("checker runner and extractor must lie inside the pinned repository")
    toolchain = _directory(Path(_string(request, "toolchain_root")), "Rust toolchain")
    cargo = _file_digest(Path(_string(request, "cargo_path")), request["cargo_sha256"], "Cargo")
    rustc = _file_digest(Path(_string(request, "rustc_path")), request["rustc_sha256"], "rustc")
    if not cargo.is_relative_to(toolchain) or not rustc.is_relative_to(toolchain):
        raise CheckerFailure("Cargo and rustc must be inside the pinned toolchain root")
    charon_toolchain = _readonly_tree_digest(
        Path(_string(request, "charon_toolchain_root")),
        request["charon_toolchain_tree_sha256"],
        "Charon Rust toolchain",
    )
    if not (charon_toolchain / "bin/rustc").is_file():
        raise CheckerFailure("Charon Rust toolchain lacks bin/rustc")
    charon_root = _directory(Path(_string(request, "charon_source_root")), "Charon source root")
    if git_checkout_digest(charon_root) != request["charon_source_tree_sha256"]:
        raise CheckerFailure("Charon source checkout drifted")
    charon = _file_digest(Path(_string(request, "charon_path")), request["charon_sha256"], "Charon")
    aeneas_root = _directory(Path(_string(request, "aeneas_source_root")), "Aeneas source root")
    if git_checkout_digest(aeneas_root) != request["aeneas_source_tree_sha256"]:
        raise CheckerFailure("Aeneas source checkout drifted")
    aeneas = _file_digest(Path(_string(request, "aeneas_path")), request["aeneas_sha256"], "Aeneas")
    if not charon.is_relative_to(charon_root) or not aeneas.is_relative_to(aeneas_root):
        raise CheckerFailure("extractor executable lies outside its pinned source root")
    dependency_raw = request["dependency_cache_path"]
    if not isinstance(dependency_raw, str):
        raise CheckerFailure("dependency_cache_path must be a string")
    if dependency_raw:
        dependency = _tree_digest(Path(dependency_raw), request["dependency_cache_tree_sha256"], "dependency cache")
    else:
        dependency = None
        if request["dependency_cache_tree_sha256"] != _sha(b""):
            raise CheckerFailure("empty dependency cache has the wrong identity")
    opam_switch_prefix = _directory(
        Path(_string(request, "opam_switch_prefix")), "opam switch prefix"
    )
    if not (opam_switch_prefix / "bin").is_dir():
        raise CheckerFailure("opam switch prefix lacks bin")
    if (
        readonly_closure_digest(opam_switch_prefix)
        != request["opam_switch_prefix_tree_sha256"]
    ):
        raise CheckerFailure("opam switch read-only closure drifted")
    opam_switch_environment = _validate_opam_switch_environment(
        request["opam_switch_environment"],
        opam_switch_prefix,
        request["opam_switch"],
    )
    goal = _file_digest(Path(_string(request, "goal_path")), request["goal_sha256"], "GOAL")
    candidate = _directory(Path(_string(request, "candidate_root")), "candidate source")
    manifest = source_tree_manifest(candidate)
    if manifest != request["candidate_manifest"] or manifest["source_tree_sha256"] != request["candidate_tree_sha256"]:
        raise CheckerFailure("candidate manifest/root differs from the closed request")
    output_root = _directory(Path(_string(request, "output_root")), "checker output root")
    if output_root_binding(output_root) != request["output_root_binding_sha256"]:
        raise CheckerFailure("checker output root path binding mismatch")
    pinned_inputs = [
        candidate,
        goal,
        runner,
        extractor,
        filespec_checker,
        prescribed_region_checker,
        toolchain,
        charon_toolchain,
        charon_root,
        aeneas_root,
        opam_switch_prefix,
    ]
    if dependency is not None:
        pinned_inputs.append(dependency)
    if any(_paths_overlap(output_root, path) for path in pinned_inputs):
        raise CheckerFailure("checker output root overlaps a pinned read-only input")

    run_root = output_root / request["run_id"]
    try:
        run_root.mkdir(mode=0o700)
    except FileExistsError as exc:
        raise CheckerFailure("checker run root already exists") from exc
    compile_writable = run_root / "compile-writable"
    extraction_writable = run_root / "extraction-writable"
    logs = run_root / "logs"
    compile_writable.mkdir(mode=0o700)
    extraction_writable.mkdir(mode=0o700)
    logs.mkdir(mode=0o700)
    opam_environment_json = run_root / "opam-switch-environment.json"
    with opam_environment_json.open("xb") as handle:
        handle.write(canonical_json(opam_switch_environment))
    compile_source = compile_writable / "source"
    extraction_source = extraction_writable / "source"
    shutil.copytree(candidate, compile_source, symlinks=False)
    shutil.copytree(candidate, extraction_source, symlinks=False)
    if source_tree_digest(compile_source) != request["candidate_tree_sha256"] or source_tree_digest(extraction_source) != request["candidate_tree_sha256"]:
        raise CheckerFailure("fresh checker source copy differs from the candidate root")
    result["source_facts"]["compile_initial_tree_sha256"] = request["candidate_tree_sha256"]
    result["source_facts"]["extraction_initial_tree_sha256"] = request["candidate_tree_sha256"]

    readonly = [
        candidate,
        goal,
        repository,
        toolchain,
        charon_toolchain,
        charon_root,
        aeneas_root,
        opam_switch_prefix,
        opam_environment_json,
        filespec_checker,
        prescribed_region_checker,
    ]
    if dependency is not None:
        readonly.append(dependency)
    result["sandbox_mounts"] = {
        "read_only": [str(path) for path in readonly],
        "writable": [str(compile_writable), str(extraction_writable)],
        "network": "unshared",
    }
    standalone_error = _standalone_package(compile_source)
    result["stages"].append(_internal_stage(
        "standalone_package", compile_source, request["runner_sha256"],
        passed=not standalone_error, detail=standalone_error.encode("utf-8"),
    ))
    if standalone_error:
        result["first_failing_stage"] = "standalone_package"
        result["reason"] = standalone_error
        return result

    compile_cargo_home = (
        dependency if dependency is not None else compile_writable / "cargo-home"
    )
    if dependency is None:
        compile_cargo_home.mkdir()
    compile_tmp = compile_writable / "tmp"
    compile_tmp.mkdir()
    compile_environment = dict(closed)
    compile_environment.update({
        "PATH": os.pathsep.join((str(toolchain / "bin"), str(charon.parent), str(aeneas.parent), "/usr/bin", "/bin")),
        "HOME": str(compile_writable),
        "CARGO_HOME": str(compile_cargo_home),
        "RUSTC": str(rustc),
        "RUSTDOC": str(rustc.parent / "rustdoc"),
        "TMPDIR": str(compile_tmp),
        "TMP": str(compile_tmp),
        "TEMP": str(compile_tmp),
        "CARGO_NET_OFFLINE": "true",
    })
    command = [
        str(cargo), "check", "--manifest-path", str(compile_source / "Cargo.toml"),
        "--offline", "--profile", request["extraction_profile"],
        "--target", request["target_triple"], "--target-dir", str(compile_writable / "target"),
    ]
    if (compile_source / "Cargo.lock").is_file():
        command.append("--locked")
    compile_stage = _run_command(
        name="cargo_check", tool="cargo", command=command, cwd=compile_source,
        environment=compile_environment, binary_sha256=request["cargo_sha256"],
        writable=compile_writable,
        readonly=readonly, limits=limits, log_root=logs, ordinal=1,
        sandbox_enabled=sandbox_enabled,
    )
    result["stages"].append(compile_stage)
    result["source_facts"]["compile_final_tree_sha256"] = source_tree_digest(compile_source)
    compile_lock = compile_source / "Cargo.lock"
    if compile_lock.is_file():
        result["source_facts"]["compile_cargo_lock_sha256"] = _sha(compile_lock.read_bytes())
    if compile_stage["status"] != "passed":
        result["first_failing_stage"] = "cargo_check"
        result["reason"] = "offline cargo check failed, timed out, or exceeded a capture limit"
        return result
    if result["source_facts"]["compile_final_tree_sha256"] != request["candidate_tree_sha256"]:
        detail = "cargo check changed the sealed source copy"
        result["stages"].append(_internal_stage(
            "sealed_source_unchanged", compile_source, request["runner_sha256"],
            passed=False, detail=detail.encode("utf-8"),
        ))
        result["first_failing_stage"] = "sealed_source_unchanged"
        result["reason"] = detail
        return result

    dep_info_error = _dep_info_outside_closure(
        compile_writable / "target", compile_source, toolchain
    )
    result["stages"].append(_internal_stage(
        "cargo_dep_info_closure", compile_source, request["runner_sha256"],
        passed=not dep_info_error, detail=dep_info_error.encode("utf-8"),
    ))
    if dep_info_error:
        result["first_failing_stage"] = "cargo_dep_info_closure"
        result["reason"] = dep_info_error
        return result

    # This second check is intentionally after Cargo.  The real bwrap leg cannot
    # address the extraction home at all; the recheck also fails closed in a
    # non-bwrap diagnostic harness if the supposedly sealed copy drifted.
    if source_tree_digest(extraction_source) != request["candidate_tree_sha256"]:
        raise CheckerFailure("fresh extraction source copy changed before extraction")

    extraction_cargo_home = (
        dependency if dependency is not None else extraction_writable / "cargo-home"
    )
    if dependency is None:
        extraction_cargo_home.mkdir()
    extraction_tmp = extraction_writable / "tmp"
    extraction_tmp.mkdir()
    extraction_environment = dict(closed)
    extraction_environment.update(opam_switch_environment)
    extraction_environment.update({
        "PATH": os.pathsep.join((str(charon_toolchain / "bin"), str(charon.parent), str(aeneas.parent), str(opam_switch_prefix / "bin"), "/usr/bin", "/bin")),
        "HOME": str(extraction_writable),
        "CARGO_HOME": str(extraction_cargo_home),
        "RUSTC": str(rustc),
        "RUSTDOC": str(rustc.parent / "rustdoc"),
        "TMPDIR": str(extraction_tmp),
        "TMP": str(extraction_tmp),
        "TEMP": str(extraction_tmp),
        "CHARON_TOOLCHAIN_IS_IN_PATH": "1",
        # charon-driver links librustc_driver from its own toolchain; rustup
        # would have set this, and the toolchain is on PATH instead.
        "LD_LIBRARY_PATH": str(charon_toolchain / "lib"),
        "CARGO_NET_OFFLINE": "true",
    })
    generated = extraction_writable / "extracted"
    typed = extraction_writable / "extractor-result.json"
    extract_target = extraction_writable / "target"
    work = extraction_writable / "work"
    work.mkdir()
    extract_command = [
        str(sys.executable), str(extractor), "--crate", str(extraction_source),
        "--out", str(generated), "--charon-bin", str(charon), "--aeneas-bin", str(aeneas),
        "--opam-switch", request["opam_switch"], "--target", request["target_triple"],
        "--opam-env-json", str(opam_environment_json),
        "--filespec-checker", str(filespec_checker),
        "--prescribed-region-checker", str(prescribed_region_checker),
        "--extraction-profile", request["extraction_profile"], "--cargo-target-dir", str(extract_target),
        "--work-root", str(work), "--phase0-result", str(typed),
    ]
    if request["check_kind"] == "ratification_replay":
        extract_command.append("--gate")
    driver = _run_command(
        name="extractor_driver", tool="extractor", command=extract_command, cwd=extraction_source,
        environment=extraction_environment, binary_sha256=request["extractor_sha256"],
        writable=extraction_writable,
        readonly=readonly, limits=limits, log_root=logs, ordinal=2,
        sandbox_enabled=sandbox_enabled,
    )
    typed_records: list[dict[str, Any]] = []
    typed_value: dict[str, Any] = {}
    gate_records: list[dict[str, Any]] = []
    if typed.is_file():
        typed_records, typed_value = _typed_records(typed, driver["sandbox_argv"], limits)
        gate_records = [record for record in typed_records if record["name"] == "filespec_region_gates"]
        result["stages"].extend(
            record for record in typed_records if record["name"] != "filespec_region_gates"
        )
    result["source_facts"]["extraction_final_tree_sha256"] = source_tree_digest(extraction_source)
    extraction_lock = extraction_source / "Cargo.lock"
    if extraction_lock.is_file():
        result["source_facts"]["extraction_cargo_lock_sha256"] = _sha(extraction_lock.read_bytes())
    if result["source_facts"]["extraction_final_tree_sha256"] != request["candidate_tree_sha256"]:
        detail = "extraction changed the sealed source copy"
        result["stages"].append(_internal_stage(
            "sealed_source_unchanged", extraction_source, request["runner_sha256"],
            passed=False, detail=detail.encode("utf-8"),
        ))
        result["first_failing_stage"] = "sealed_source_unchanged"
        result["reason"] = detail
        return result
    if driver["status"] != "passed" or typed_value.get("status") != "passed":
        failing = typed_value.get("first_failing_stage")
        if not isinstance(failing, str) or not failing:
            failing = "filespec_region_gates" if request["check_kind"] == "ratification_replay" else "generated_model_split"
        result["first_failing_stage"] = failing
        result["reason"] = str(typed_value.get("detail") or "extractor failed, timed out, or exceeded a capture limit")
        if failing == "filespec_region_gates":
            result["stages"].extend(gate_records)
        # The driver capture is retained when no internal raw record exists.
        if not typed_records:
            driver["name"] = failing
            result["stages"].append(driver)
        elif not any(
            stage["name"] == failing and stage["status"] == "failed"
            for stage in result["stages"]
        ):
            result["stages"].append(
                _internal_stage(
                    failing,
                    extraction_source,
                    request["extractor_sha256"],
                    passed=False,
                    detail=result["reason"].encode("utf-8"),
                )
            )
        return result

    result["stages"].append(_internal_stage(
        "generated_model_split", generated, request["extractor_sha256"], passed=True
    ))
    result["model_bodiless_declarations"] = read_model_bodiless_declarations(generated)
    result["llbc_root_sha256"] = typed_value.get("llbc_sha256") or None
    result["generated_lean_root_sha256"] = typed_value.get("generated_lean_sha256") or None
    result["tablet_root_sha256"] = typed_value.get("tablet_root_sha256") or None
    result["cargo_lock_sha256"] = result["source_facts"]["extraction_cargo_lock_sha256"]
    try:
        result["output_tree_sha256"] = source_tree_digest(generated)
    except SourceTreeError as exc:
        result["first_failing_stage"] = "generated_model_split"
        result["reason"] = str(exc)
        return result
    if request["check_kind"] == "ratification_replay":
        determinism_path = generated / "extraction-evidence" / "DETERMINISM.json"
        try:
            determinism = json.loads(determinism_path.read_bytes(), object_pairs_hook=_strict_object)
        except (OSError, ValueError) as exc:
            result["first_failing_stage"] = "second_clean_extraction"
            result["reason"] = f"missing or invalid A/B receipt: {exc}"
            return result
        result["a_b_generated_lean_roots"] = determinism.get("generated_lean_sha256", [])
        result["a_b_tablet_roots"] = determinism.get("tablet_root_sha256", [])
        try:
            second_records = _second_extraction_records(
                generated / "extraction-evidence", driver["sandbox_argv"], limits
            )
        except (OSError, ValueError, KeyError, CheckerFailure) as exc:
            result["first_failing_stage"] = "second_clean_extraction"
            result["reason"] = f"cannot reconstruct second extraction stage records: {exc}"
            return result
        result["stages"].extend(second_records)
        if not second_records or any(stage["status"] != "passed" for stage in second_records):
            result["first_failing_stage"] = "second_clean_extraction"
            result["reason"] = "second extraction timed out, failed, or exceeded a capture limit"
            return result
        if determinism.get("status") != "passed" or not determinism.get("generated_lean_byte_identical") or not determinism.get("tablet_tree_byte_identical"):
            result["first_failing_stage"] = "second_clean_extraction"
            result["reason"] = "A/B extraction roots differ"
            return result
        result["stages"].extend(gate_records)
    result["status"] = "passed"
    result["reason"] = "completed"
    return result


def main() -> int:
    raw = sys.stdin.buffer.read(MAX_REQUEST_BYTES + 1)
    request: dict[str, Any] = {}
    result = _base_output(request)
    try:
        if len(raw) > MAX_REQUEST_BYTES:
            raise CheckerFailure("checker request exceeds its byte limit")
        parsed = json.loads(raw, object_pairs_hook=_strict_object)
        if not isinstance(parsed, dict):
            raise CheckerFailure("checker request must be an object")
        request = parsed
        result = run(request, sandbox_enabled=True)
    except SandboxUnavailable as exc:
        sys.stderr.write(f"{exc}\n")
        return 3
    except (CheckerFailure, OSError, SourceTreeError, subprocess.SubprocessError, UnicodeError) as exc:
        result = _base_output(request)
        result["reason"] = str(exc)
    sys.stdout.buffer.write(canonical_json(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
