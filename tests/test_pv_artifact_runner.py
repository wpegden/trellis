from __future__ import annotations

import base64
import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
from pathlib import Path

from trellis.config import SandboxConfig
from trellis.sandbox import wrap_command


ROOT = Path(__file__).resolve().parents[1]
RUNNER_PATH = ROOT / "scripts" / "pv_artifact_runner.py"
SPEC = importlib.util.spec_from_file_location("pv_artifact_runner", RUNNER_PATH)
assert SPEC and SPEC.loader
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)


def _sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _toolchain() -> tuple[Path, Path, Path, str]:
    root = Path(
        subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()
    ).resolve()
    rustc = (root / "bin" / "rustc").resolve()
    cargo = (root / "bin" / "cargo").resolve()
    rustc_vv = subprocess.check_output([rustc, "-vV"], text=True)
    return root, rustc, cargo, rustc_vv


def _request(crate: Path, goal: Path, runtime: Path, source: bytes) -> dict:
    toolchain, rustc, cargo, rustc_vv = _toolchain()
    lock = crate / "Cargo.lock"
    relative_path = (
        "reference/rust-witnesses/target-"
        + _sha(b"goal:synthetic")
        + "/witness.rs"
    )
    worker_source = goal.parent / relative_path
    worker_source.parent.mkdir(parents=True)
    worker_source.write_bytes(source)
    return {
        "schema": RUNNER.REQUEST_SCHEMA,
        "target_id": "goal:synthetic",
        "relative_path": relative_path,
        "artifact_source_base64": base64.b64encode(source).decode(),
        "artifact_sha256": _sha(source),
        "worker_source_path": str(worker_source),
        "pinned_crate_root": str(crate),
        "pinned_crate_tree_sha256": RUNNER._tree_digest(crate),
        "goal_path": str(goal),
        "goal_sha256": _sha(goal.read_bytes()),
        "cargo_lock_sha256": _sha(lock.read_bytes()) if lock.exists() else "0" * 64,
        "rustc_path": str(rustc),
        "rustc_vv": rustc_vv,
        "cargo_path": str(cargo),
        "toolchain_root": str(toolchain),
        "dependency_cache_path": "",
        "runtime_root": str(runtime),
        "run_id": "synthetic-run",
        "limits": {
            "timeout_seconds": 30,
            "cpu_seconds": 20,
            "address_space_bytes": 2_147_483_648,
            "file_size_bytes": 67_108_864,
            "process_count": 128,
            "stdout_bytes": 4096,
            "stderr_bytes": 4096,
        },
    }


def test_artifact_role_certifies_closed_mounts_and_exact_command(tmp_path, monkeypatch):
    crate = tmp_path / "crate"
    writable = tmp_path / "runtime" / "writable"
    goal = tmp_path / "GOAL.md"
    crate.mkdir()
    writable.mkdir(parents=True)
    goal.write_text("synthetic goal\n", encoding="utf-8")
    toolchain, rustc, cargo, _ = _toolchain()
    monkeypatch.setenv(
        "TRELLIS_SANDBOX_EXTRA_RO_PATHS",
        os.pathsep.join(map(str, (crate, goal, toolchain))),
    )
    monkeypatch.setenv("CARGO_HOME", str(writable / "cargo-home"))
    monkeypatch.setenv("RUSTC", str(rustc))
    monkeypatch.setenv("PATH", os.pathsep.join((str(toolchain / "bin"), "/usr/bin", "/bin")))
    monkeypatch.setenv("TMPDIR", str(writable / "target" / "tmp"))
    monkeypatch.setenv("TRELLIS_CHECKER_TOKEN", "must-not-cross")
    monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path / "worker-cache-must-not-mount"))
    inner = [str(cargo), "test", "--manifest-path", str(writable / "sealed-crate/Cargo.toml"), "--test", "trellis_witness", "--locked", "--offline", "--target-dir", str(writable / "target"), "--", "--nocapture"]
    wrapped = wrap_command(
        inner,
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=crate,
        burst_home=writable,
        role="rust_witness_artifact",
    )
    assert "--unshare-net" in wrapped
    assert "--clearenv" in wrapped
    assert "--tmpfs" not in wrapped
    assert "must-not-cross" not in wrapped
    assert str(tmp_path / "worker-cache-must-not-mount") not in wrapped
    setenv_names = {
        wrapped[index + 1]
        for index, value in enumerate(wrapped)
        if value == "--setenv"
    }
    assert setenv_names <= {
        "HOME",
        "TRELLIS_SANDBOX_ROLE",
        "PATH",
        "CARGO_HOME",
        "RUSTC",
        "RUSTDOC",
        "TMPDIR",
        "TMP",
        "TEMP",
    }
    assert {"HOME", "TRELLIS_SANDBOX_ROLE", "PATH", "CARGO_HOME", "RUSTC", "TMPDIR"} <= setenv_names
    assert wrapped[-len(inner) :] == inner
    assert ["--ro-bind", str(crate.resolve()), str(crate.resolve())] == wrapped[
        wrapped.index(str(crate.resolve())) - 1 : wrapped.index(str(crate.resolve())) + 2
    ]
    bind_index = wrapped.index("--bind")
    assert wrapped[bind_index + 1 : bind_index + 3] == [str(writable.resolve())] * 2
    bind_sources = [
        wrapped[index + 1]
        for index, value in enumerate(wrapped)
        if value in {"--bind", "--ro-bind"}
    ]
    assert not any(
        "/.codex" in value or "/.claude" in value or "/.gemini" in value
        for value in bind_sources
    )


def test_missing_lock_is_attempted_unavailable_before_sandbox(tmp_path):
    crate = tmp_path / "crate"
    runtime = tmp_path / "runtime"
    crate.mkdir()
    runtime.mkdir()
    (crate / "Cargo.toml").write_text(
        '[package]\nname="synthetic"\nversion="0.1.0"\nedition="2021"\n',
        encoding="utf-8",
    )
    goal = tmp_path / "GOAL.md"
    goal.write_text("synthetic goal\n", encoding="utf-8")
    request = _request(crate, goal, runtime, b"#[test] fn witness() {}\n")
    completed = subprocess.run(
        [RUNNER_PATH],
        input=json.dumps(request, sort_keys=True, separators=(",", ":")).encode(),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env={"PYTHONPATH": str(ROOT), "PYTHONDONTWRITEBYTECODE": "1"},
        check=False,
    )
    assert completed.returncode == 0
    output = json.loads(completed.stdout)
    assert output["attempted"] is True
    assert output["status"] == "unavailable"
    assert output["command"] == []
    assert "Cargo.lock" in output["reason"]


def test_compiler_crate_lock_and_request_digests_are_bound(tmp_path):
    crate = tmp_path / "crate"
    runtime = tmp_path / "runtime"
    crate.mkdir()
    runtime.mkdir()
    (crate / "Cargo.toml").write_text(
        '[package]\nname="synthetic"\nversion="0.1.0"\nedition="2021"\n',
        encoding="utf-8",
    )
    (crate / "Cargo.lock").write_text("version = 3\n", encoding="utf-8")
    goal = tmp_path / "GOAL.md"
    goal.write_text("synthetic goal\n", encoding="utf-8")
    baseline = _request(crate, goal, runtime, b"#[test] fn witness() {}\n")
    for field, changed in (
        ("pinned_crate_tree_sha256", "1" * 64),
        ("cargo_lock_sha256", "2" * 64),
        ("rustc_vv", baseline["rustc_vv"] + "drift"),
        ("artifact_sha256", "3" * 64),
    ):
        request = dict(baseline)
        request[field] = changed
        result = RUNNER._result_base(request)
        try:
            RUNNER.run(request)
        except RUNNER.RunnerUnavailable as exc:
            result["reason"] = str(exc)
        assert result["reason"]


def test_sealed_lock_is_rechecked_after_witness_installation(tmp_path, monkeypatch):
    crate = tmp_path / "crate"
    runtime = tmp_path / "runtime"
    crate.mkdir()
    runtime.mkdir()
    (crate / "Cargo.toml").write_text(
        '[package]\nname="synthetic"\nversion="0.1.0"\nedition="2021"\n',
        encoding="utf-8",
    )
    (crate / "Cargo.lock").write_text("version = 3\n", encoding="utf-8")
    goal = tmp_path / "GOAL.md"
    goal.write_text("synthetic goal\n", encoding="utf-8")
    request = _request(crate, goal, runtime, b"#[test] fn witness() {}\n")
    original_write_bytes = Path.write_bytes

    def write_and_drift_lock(path: Path, data: bytes) -> int:
        written = original_write_bytes(path, data)
        if path.name == "trellis_witness.rs":
            original_write_bytes(path.parents[1] / "Cargo.lock", b"version = 3\n# drift\n")
        return written

    monkeypatch.setattr(Path, "write_bytes", write_and_drift_lock)
    try:
        RUNNER.run(request)
    except RUNNER.RunnerUnavailable as exc:
        assert "sealed Cargo.lock" in str(exc)
    else:
        raise AssertionError("sealed lock drift reached command construction")


def test_supplemental_lock_cannot_replace_pinned_lock(tmp_path):
    crate = tmp_path / "crate"
    runtime = tmp_path / "runtime"
    crate.mkdir(); runtime.mkdir()
    (crate / "Cargo.toml").write_text('[package]\nname="synthetic"\nversion="0.1.0"\n')
    (crate / "Cargo.lock").write_text("version = 3\n")
    goal = tmp_path / "GOAL.md"; goal.write_text("synthetic goal\n")
    request = _request(crate, goal, runtime, b"#[test] fn witness() {}\n")
    request.update(schema=RUNNER.SUPPLEMENTAL_LOCK_REQUEST_SCHEMA,
                   supplemental_cargo_lock_base64=base64.b64encode(b"version = 3\n").decode())
    import pytest
    with pytest.raises(RUNNER.RunnerUnavailable, match="cannot replace"):
        RUNNER.run(request)


def test_supplemental_lock_is_hash_bound_before_execution(tmp_path):
    crate = tmp_path / "crate"; crate.mkdir()
    runtime = tmp_path / "runtime"; runtime.mkdir()
    (crate / "Cargo.toml").write_text('[package]\nname="synthetic"\nversion="0.1.0"\n')
    goal = tmp_path / "GOAL.md"; goal.write_text("synthetic goal\n")
    request = _request(crate, goal, runtime, b"#[test] fn witness() {}\n")
    request.update(schema=RUNNER.SUPPLEMENTAL_LOCK_REQUEST_SCHEMA,
                   supplemental_cargo_lock_base64=base64.b64encode(b"version = 3\n").decode())
    import pytest
    with pytest.raises(RUNNER.RunnerUnavailable, match="supplemental Cargo.lock differs"):
        RUNNER.run(request)
    assert not (crate / "Cargo.lock").exists()


def test_runner_bounds_linker_parallelism_before_resource_limits(monkeypatch):
    applied = []
    monkeypatch.setattr(RUNNER, "_current_uid_task_count", lambda: 100)
    monkeypatch.setattr(RUNNER.os, "sched_getaffinity", lambda _: {4, 7, 11, 15})
    monkeypatch.setattr(RUNNER.os, "sched_setaffinity", lambda pid, cpus: applied.append((pid, cpus)))
    monkeypatch.setattr(RUNNER.resource, "setrlimit", lambda *args: None)
    RUNNER._resource_limits(dict(cpu_seconds=1, address_space_bytes=100,
        file_size_bytes=100, process_count=10))()
    assert applied == [(0, [4, 7])]
