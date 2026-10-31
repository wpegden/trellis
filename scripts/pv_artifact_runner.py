#!/usr/bin/env python3
"""Run the one framework-owned Rust corroboration test in a sealed copy."""

from __future__ import annotations

import base64
import hashlib
import json
import os
import resource
import shutil
import stat
import subprocess
import sys
from pathlib import Path
from typing import Any

from trellis.config import SandboxConfig
from trellis.sandbox import wrap_command

try:
    from scripts.pv_source_tree import (
        SourceTreeError,
        source_tree_entries,
        source_tree_root_from_entries,
    )
except ModuleNotFoundError:  # direct script execution
    from pv_source_tree import (  # type: ignore[no-redef]
        SourceTreeError,
        source_tree_entries,
        source_tree_root_from_entries,
    )


REQUEST_SCHEMA = "trellis-pv-rust-witness-runner-request/v1"
OUTPUT_SCHEMA = "trellis-pv-rust-witness-runner-output/v1"
SUPPLEMENTAL_LOCK_REQUEST_SCHEMA = "trellis-pv-rust-witness-runner-request/v2"
FIXED_TEST_NAME = "trellis_witness"
MAX_REQUEST_BYTES = 2 * 1024 * 1024
MAX_SOURCE_BYTES = 1024 * 1024
_REQUEST_KEYS = {
    "schema",
    "target_id",
    "relative_path",
    "artifact_source_base64",
    "artifact_sha256",
    "worker_source_path",
    "pinned_crate_root",
    "pinned_crate_tree_sha256",
    "goal_path",
    "goal_sha256",
    "cargo_lock_sha256",
    "rustc_path",
    "rustc_vv",
    "cargo_path",
    "toolchain_root",
    "dependency_cache_path",
    "runtime_root",
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


class RunnerUnavailable(Exception):
    pass


class SandboxUnavailable(Exception):
    pass


def _strict_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for key, value in pairs:
        if key in out:
            raise RunnerUnavailable(f"duplicate JSON member: {key}")
        out[key] = value
    return out


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _canonical(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()


def _tagged_hash(tag: str, payload: bytes) -> str:
    tag_bytes = tag.encode()
    framed = (
        b"trellis-trust-v1\0"
        + len(tag_bytes).to_bytes(4, "big")
        + tag_bytes
        + len(payload).to_bytes(8, "big")
        + payload
    )
    return _sha256(framed)


def _regular_file(path: Path, label: str) -> Path:
    try:
        mode = path.lstat().st_mode
    except OSError as exc:
        raise RunnerUnavailable(f"{label} is unavailable: {exc}") from exc
    if stat.S_ISLNK(mode) or not stat.S_ISREG(mode):
        raise RunnerUnavailable(f"{label} must be a non-symlink regular file")
    return path.resolve(strict=True)


def _directory(path: Path, label: str) -> Path:
    try:
        mode = path.lstat().st_mode
    except OSError as exc:
        raise RunnerUnavailable(f"{label} is unavailable: {exc}") from exc
    if stat.S_ISLNK(mode) or not stat.S_ISDIR(mode):
        raise RunnerUnavailable(f"{label} must be a non-symlink directory")
    return path.resolve(strict=True)


def _regular_file_beneath(root: Path, relative: str, label: str) -> Path:
    if "\\" in relative or relative.startswith("/"):
        raise RunnerUnavailable(f"{label} path is not normalized relative syntax")
    parts = relative.split("/")
    if not parts or any(part in ("", ".", "..") for part in parts):
        raise RunnerUnavailable(f"{label} path is not normalized relative syntax")
    canonical_root = _directory(root, f"{label} root")
    current = canonical_root
    for index, part in enumerate(parts):
        current = current / part
        try:
            mode = current.lstat().st_mode
        except OSError as exc:
            raise RunnerUnavailable(f"{label} is unavailable: {exc}") from exc
        if stat.S_ISLNK(mode):
            raise RunnerUnavailable(f"{label} path contains a symlink component")
        final = index + 1 == len(parts)
        if (final and not stat.S_ISREG(mode)) or (not final and not stat.S_ISDIR(mode)):
            raise RunnerUnavailable(f"{label} path has the wrong filesystem type")
    resolved = current.resolve(strict=True)
    if not resolved.is_relative_to(canonical_root):
        raise RunnerUnavailable(f"{label} resolves outside its root")
    return resolved


def _tree_entries(root: Path, *, ignored_relative_path: str | None = None) -> list[dict[str, Any]]:
    try:
        entries = source_tree_entries(root)
    except SourceTreeError as exc:
        raise RunnerUnavailable(str(exc)) from exc
    if ignored_relative_path is not None:
        entries = [
            entry for entry in entries
            if entry["relative_path"] != ignored_relative_path
        ]
        if not entries:
            raise RunnerUnavailable("pinned crate tree is empty")
    return entries


def _tree_digest(root: Path, *, ignored_relative_path: str | None = None) -> str:
    try:
        return source_tree_root_from_entries(
            _tree_entries(root, ignored_relative_path=ignored_relative_path)
        )
    except SourceTreeError as exc:
        raise RunnerUnavailable(str(exc)) from exc


def _string(obj: dict[str, Any], key: str) -> str:
    value = obj.get(key)
    if not isinstance(value, str) or not value:
        raise RunnerUnavailable(f"{key} must be a non-empty string")
    return value


def _digest(obj: dict[str, Any], key: str) -> str:
    value = _string(obj, key)
    if len(value) != 64 or any(char not in "0123456789abcdef" for char in value):
        raise RunnerUnavailable(f"{key} must be a lowercase SHA-256 digest")
    return value


def _positive_int(obj: dict[str, Any], key: str) -> int:
    value = obj.get(key)
    if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
        raise RunnerUnavailable(f"limits.{key} must be a positive integer")
    return value


def _read_capped(path: Path, limit: int) -> tuple[bytes, bool]:
    with path.open("rb") as handle:
        data = handle.read(limit + 1)
    return data[:limit], len(data) > limit


def _current_uid_task_count() -> int:
    """Tasks (threads) owned by this uid on the host, which is what RLIMIT_NPROC counts."""
    uid = os.getuid()
    count = 0
    for entry in os.scandir("/proc"):
        if not entry.name.isdigit():
            continue
        try:
            if entry.stat().st_uid != uid:
                continue
            with open(f"/proc/{entry.name}/status", "rb") as status:
                for line in status:
                    if line.startswith(b"Threads:"):
                        count += int(line.split()[1])
                        break
        except (OSError, ValueError, IndexError):
            continue
    return count


def _resource_limits(limits: dict[str, int]):
    # RLIMIT_NPROC is enforced per uid across the whole host and counts
    # tasks (threads), compared against the limit at every user-namespace
    # level, so an absolute limit below the uid's current task count makes
    # bwrap's first clone fail with EAGAIN. Bound the run to `process_count`
    # additional tasks above the baseline instead. The headroom must cover a
    # parallel cargo build (rustc and ld are multi-threaded) plus whatever
    # other host processes of this uid spawn meanwhile; callers pass
    # thousands, not hundreds.
    nproc_limit = _current_uid_task_count() + limits["process_count"]

    def apply() -> None:
        # LLVM sizes its linker thread pool from affinity. Bound it before
        # applying memory/task limits so large hosts do not exhaust either.
        if hasattr(os, "sched_getaffinity"):
            os.sched_setaffinity(0, sorted(os.sched_getaffinity(0))[:2])
        resource.setrlimit(resource.RLIMIT_CPU, (limits["cpu_seconds"], limits["cpu_seconds"]))
        resource.setrlimit(
            resource.RLIMIT_AS,
            (limits["address_space_bytes"], limits["address_space_bytes"]),
        )
        resource.setrlimit(
            resource.RLIMIT_FSIZE,
            (limits["file_size_bytes"], limits["file_size_bytes"]),
        )
        resource.setrlimit(resource.RLIMIT_NPROC, (nproc_limit, nproc_limit))

    return apply


def _result_base(request: dict[str, Any]) -> dict[str, Any]:
    return {
        "schema": OUTPUT_SCHEMA,
        "status": "unavailable",
        "attempted": True,
        "target_id": request.get("target_id", ""),
        "relative_path": request.get("relative_path", ""),
        "artifact_sha256": request.get("artifact_sha256", ""),
        "pinned_crate_tree_sha256": request.get("pinned_crate_tree_sha256", ""),
        "cargo_lock_sha256": request.get("cargo_lock_sha256", ""),
        "goal_sha256": request.get("goal_sha256", ""),
        "rustc_vv": request.get("rustc_vv", ""),
        "command": [],
        "sandbox_argv": [],
        "sandbox_mounts": {},
        "timed_out": False,
        "exit_code": None,
        "stdout_base64": "",
        "stderr_base64": "",
        "stdout_sha256": _sha256(b""),
        "stderr_sha256": _sha256(b""),
        "stdout_truncated": False,
        "stderr_truncated": False,
        "reason": "",
    }


def run(request: dict[str, Any]) -> dict[str, Any]:
    result = _result_base(request)
    supplemental_lock = request.get("schema") == SUPPLEMENTAL_LOCK_REQUEST_SCHEMA
    expected_keys = _REQUEST_KEYS | ({"supplemental_cargo_lock_base64"} if supplemental_lock else set())
    if set(request) != expected_keys:
        raise RunnerUnavailable("runner request has missing or unknown fields")
    if request["schema"] not in {REQUEST_SCHEMA, SUPPLEMENTAL_LOCK_REQUEST_SCHEMA}:
        raise RunnerUnavailable("runner request has the wrong schema")
    for key in ("target_id", "relative_path", "rustc_vv", "run_id"):
        _string(request, key)
    expected_relative_path = (
        "reference/rust-witnesses/target-"
        + _sha256(request["target_id"].encode())
        + "/witness.rs"
    )
    if request["relative_path"] != expected_relative_path:
        raise RunnerUnavailable("relative_path differs from the target-derived witness path")
    for key in (
        "artifact_sha256",
        "pinned_crate_tree_sha256",
        "goal_sha256",
        "cargo_lock_sha256",
    ):
        _digest(request, key)
    run_id = request["run_id"]
    if len(run_id) > 96 or any(not (c.isalnum() or c in "-_") for c in run_id):
        raise RunnerUnavailable("run_id is not a safe path component")
    limits_raw = request["limits"]
    if not isinstance(limits_raw, dict) or set(limits_raw) != _LIMIT_KEYS:
        raise RunnerUnavailable("limits has missing or unknown fields")
    limits = {key: _positive_int(limits_raw, key) for key in _LIMIT_KEYS}

    try:
        source = base64.b64decode(_string(request, "artifact_source_base64"), validate=True)
    except (ValueError, base64.binascii.Error) as exc:
        raise RunnerUnavailable("artifact source is not canonical base64") from exc
    if not source or len(source) > MAX_SOURCE_BYTES or _sha256(source) != request["artifact_sha256"]:
        raise RunnerUnavailable("artifact source bytes do not match their declared identity")

    crate_root = _directory(Path(_string(request, "pinned_crate_root")), "pinned crate")
    goal_path = _regular_file(Path(_string(request, "goal_path")), "GOAL")
    worker_source_path = _regular_file_beneath(
        goal_path.parent,
        request["relative_path"],
        "worker witness source",
    )
    requested_worker_source = Path(_string(request, "worker_source_path"))
    if requested_worker_source != worker_source_path:
        raise RunnerUnavailable("worker_source_path differs from the fixed repository path")
    if worker_source_path.read_bytes() != source:
        raise RunnerUnavailable("worker witness source changed after request freezing")
    cargo_path = _regular_file(Path(_string(request, "cargo_path")), "cargo executable")
    rustc_path = _regular_file(Path(_string(request, "rustc_path")), "rustc executable")
    toolchain_root = _directory(Path(_string(request, "toolchain_root")), "Rust toolchain")
    if not rustc_path.is_relative_to(toolchain_root) or not cargo_path.is_relative_to(toolchain_root):
        raise RunnerUnavailable("cargo and rustc must resolve inside the selected Rust toolchain")
    runtime_root = _directory(Path(_string(request, "runtime_root")), "runtime root")
    cache_raw = request["dependency_cache_path"]
    if not isinstance(cache_raw, str):
        raise RunnerUnavailable("dependency_cache_path must be a string")
    dependency_cache = _directory(Path(cache_raw), "dependency cache") if cache_raw else None

    if _tree_digest(crate_root) != request["pinned_crate_tree_sha256"]:
        raise RunnerUnavailable("pinned crate tree differs from extraction/bootstrap identity")
    lock_bytes = None
    if supplemental_lock:
        if (crate_root / "Cargo.lock").exists() or (crate_root / "Cargo.lock").is_symlink():
            raise RunnerUnavailable("supplemental lock cannot replace a pinned Cargo.lock")
        try:
            lock_bytes = base64.b64decode(_string(request, "supplemental_cargo_lock_base64"), validate=True)
        except (ValueError, base64.binascii.Error) as exc:
            raise RunnerUnavailable("supplemental Cargo.lock is not canonical base64") from exc
        if not lock_bytes or len(lock_bytes) > MAX_SOURCE_BYTES or _sha256(lock_bytes) != request["cargo_lock_sha256"]:
            raise RunnerUnavailable("supplemental Cargo.lock differs from its declared identity")
        result.update(schema="trellis-pv-rust-witness-runner-output/v2",
            cargo_lock_origin="supplemental; not part of historical source approval")
    else:
        lock_path = _regular_file(crate_root / "Cargo.lock", "pinned Cargo.lock")
        if _sha256(lock_path.read_bytes()) != request["cargo_lock_sha256"]:
            raise RunnerUnavailable("Cargo.lock differs from extraction provenance")
    if _sha256(goal_path.read_bytes()) != request["goal_sha256"]:
        raise RunnerUnavailable("GOAL differs from the frozen request")
    fixed_relative_path = f"tests/{FIXED_TEST_NAME}.rs"
    if (crate_root / fixed_relative_path).exists():
        raise RunnerUnavailable("pinned crate already contains the fixed witness test target")
    compiler = subprocess.run(
        [str(rustc_path), "-vV"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=min(30, limits["timeout_seconds"]),
        check=False,
        env={},
    )
    if compiler.returncode != 0 or compiler.stdout.decode("utf-8", "strict") != request["rustc_vv"]:
        raise RunnerUnavailable("resolved rustc -vV differs from extraction provenance")

    runs_root = runtime_root / "rust-witness-runs"
    runs_root.mkdir(mode=0o700, exist_ok=True)
    run_root = runs_root / run_id
    try:
        run_root.mkdir(mode=0o700)
    except FileExistsError as exc:
        raise RunnerUnavailable("run root already exists") from exc
    writable = run_root / "writable"
    writable.mkdir(mode=0o700)
    sealed = writable / "sealed-crate"
    shutil.copytree(crate_root, sealed, symlinks=False)
    if _tree_digest(sealed) != request["pinned_crate_tree_sha256"]:
        raise RunnerUnavailable("private crate copy changed before witness installation")
    if lock_bytes is not None:
        (sealed / "Cargo.lock").write_bytes(lock_bytes)
    tests_dir = sealed / "tests"
    tests_dir.mkdir(exist_ok=True)
    witness_path = tests_dir / f"{FIXED_TEST_NAME}.rs"
    witness_path.write_bytes(source)
    target_dir = writable / "target"
    tmp_dir = target_dir / "tmp"
    tmp_dir.mkdir(parents=True)
    cargo_home = dependency_cache
    if cargo_home is None:
        cargo_home = writable / "cargo-home"
        cargo_home.mkdir()

    # These are the bytes Cargo is about to consume.  The installed witness is
    # excluded from the crate-tree identity because it has its own frozen
    # digest; every extraction-owned crate member, the lock, and the witness
    # itself are rechecked together immediately before command construction.
    installed_source = witness_path.read_bytes()
    if installed_source != source or _sha256(installed_source) != request["artifact_sha256"]:
        raise RunnerUnavailable("installed witness differs from the frozen artifact")
    if _sha256((sealed / "Cargo.lock").read_bytes()) != request["cargo_lock_sha256"]:
        raise RunnerUnavailable("sealed Cargo.lock differs from extraction provenance")
    sealed_entries = _tree_entries(sealed, ignored_relative_path=fixed_relative_path)
    if supplemental_lock:
        sealed_entries = [entry for entry in sealed_entries if entry["relative_path"] != "Cargo.lock"]
    if source_tree_root_from_entries(sealed_entries) != request["pinned_crate_tree_sha256"]:
        raise RunnerUnavailable("sealed crate differs from extraction/bootstrap identity")

    command = [
        str(cargo_path),
        "test",
        "--manifest-path",
        str(sealed / "Cargo.toml"),
        "--test",
        FIXED_TEST_NAME,
        "--locked",
        "--offline",
        "--target-dir",
        str(target_dir),
        "--",
        "--nocapture",
    ]
    readonly = [crate_root, goal_path, worker_source_path, toolchain_root]
    if dependency_cache is not None:
        readonly.append(dependency_cache)
    previous = {name: os.environ.get(name) for name in (
        "TRELLIS_SANDBOX_EXTRA_RO_PATHS", "PATH", "CARGO_HOME", "RUSTC", "RUSTDOC", "TMPDIR", "TMP", "TEMP"
    )}
    try:
        os.environ["TRELLIS_SANDBOX_EXTRA_RO_PATHS"] = os.pathsep.join(str(path) for path in readonly)
        os.environ["PATH"] = os.pathsep.join((str(toolchain_root / "bin"), "/usr/bin", "/bin"))
        os.environ["CARGO_HOME"] = str(cargo_home)
        os.environ["RUSTC"] = str(rustc_path)
        os.environ["RUSTDOC"] = str(rustc_path.parent / "rustdoc")
        for name in ("TMPDIR", "TMP", "TEMP"):
            os.environ[name] = str(tmp_dir)
        sandbox_argv = wrap_command(
            command,
            sandbox=SandboxConfig(enabled=True, backend="bwrap"),
            work_dir=crate_root,
            burst_home=writable,
            role="rust_witness_artifact",
        )
    finally:
        for name, value in previous.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value

    stdout_path = run_root / "stdout"
    stderr_path = run_root / "stderr"
    result.update(
        {
            "command": command,
            "sandbox_argv": sandbox_argv,
            "sandbox_mounts": {
                "read_only": [str(path) for path in readonly],
                "writable": [str(writable)],
                "writable_members": [
                    str(sealed),
                    str(target_dir),
                ] + ([str(cargo_home)] if dependency_cache is None else []),
                "output_file_descriptors": [str(stdout_path), str(stderr_path)],
                "network": "unshared",
            },
        }
    )
    with stdout_path.open("xb") as stdout_file, stderr_path.open("xb") as stderr_file:
        process = subprocess.Popen(
            sandbox_argv,
            stdin=subprocess.DEVNULL,
            stdout=stdout_file,
            stderr=stderr_file,
            close_fds=True,
            env={},
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
    if exit_code not in (0, None) and not stdout and b"bwrap:" in stderr and (
        b"namespace" in stderr or b"not installed" in stderr or b"permission" in stderr.lower()
    ):
        raise SandboxUnavailable("Rust witness sandbox is unavailable")
    result.update(
        {
            "status": "available" if exit_code == 0 and not timed_out else "unavailable",
            "timed_out": timed_out,
            "exit_code": exit_code,
            "stdout_base64": base64.b64encode(stdout).decode(),
            "stderr_base64": base64.b64encode(stderr).decode(),
            "stdout_sha256": _sha256(stdout),
            "stderr_sha256": _sha256(stderr),
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
            "reason": "completed" if exit_code == 0 and not timed_out else "cargo test did not complete successfully",
        }
    )
    return result


def main() -> int:
    raw = sys.stdin.buffer.read(MAX_REQUEST_BYTES + 1)
    request: dict[str, Any] = {}
    result = _result_base(request)
    try:
        if len(raw) > MAX_REQUEST_BYTES:
            raise RunnerUnavailable("runner request exceeds its byte limit")
        parsed = json.loads(raw, object_pairs_hook=_strict_object)
        if not isinstance(parsed, dict):
            raise RunnerUnavailable("runner request must be an object")
        request = parsed
        result = run(request)
    except SandboxUnavailable as exc:
        sys.stderr.write(f"{exc}\n")
        return 3
    except (RunnerUnavailable, OSError, subprocess.SubprocessError, UnicodeError) as exc:
        result = _result_base(request)
        result["reason"] = str(exc)
    sys.stdout.buffer.write(_canonical(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
