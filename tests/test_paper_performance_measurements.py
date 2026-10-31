"""Empirical receipts bind snapshots while benchmark code stays confined."""
import copy
import json
import os
from pathlib import Path

import pytest

from trellis.modeling import performance as perf


def dump(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value))


def candidate(tmp_path, *, route="rust_aeneas", tools=None):
    root = tmp_path / "preparation"
    (root / "paper").mkdir(parents=True)
    (root / "paper/main.tex").write_text("The original semantic conclusion.\n")
    paper = {"route": route, "paper_path": "paper/main.tex", "paper_files": ["paper/main.tex"],
             "files": {"paper/main.tex": perf.sha256(root / "paper/main.tex")},
             "entry_points": [{"node_id": "Algorithm", "declaration": "Algorithm", "source_path": "Tablet/Algorithm.lean"}]}
    if tools is None:
        binary = tmp_path / "toolchain/bin"
        binary.mkdir(parents=True)
        (binary.parent / "lib").mkdir()
        for name in ("cargo", "rustc"):
            (binary / name).write_text("fixture")
            (binary / name).chmod(0o755)
        tools = {"cargo": str(binary / "cargo")}
    dump(root / "config.json", {"paper_input": paper, "tools": tools})
    dump(root / "work/extraction.json", {})
    if route == "rust_aeneas":
        (root / "work/crate/src").mkdir(parents=True)
        (root / "work/crate/Cargo.toml").write_text('[package]\nname="algorithm"\nversion="0.1.0"\nedition="2021"\n')
        (root / "work/crate/src/lib.rs").write_text("pub fn algorithm(n: u64) -> u64 { n.wrapping_mul(3).wrapping_add(1) }\n")
    else:
        from trellis.paper_implementation import lean_lakefile, lean_lake_manifest
        (root / "work/lean/Tablet").mkdir(parents=True)
        (root / "work/lean/Tablet/Algorithm.lean").write_text("-- [TABLET NODE: Algorithm]\ndef Algorithm (n : UInt64) : UInt64 :=\n-- BODY\n  n * 3 + 1\n")
        (root / "work/lean/lakefile.toml").write_text(lean_lakefile(["Algorithm"]))
        (root / "work/lean/lake-manifest.json").write_text(lean_lake_manifest())
        (root / "work/lean/lean-toolchain").write_text(tools["lean_toolchain"] + "\n")
    (root / "benchmarks/harness").mkdir(parents=True)
    (root / "benchmarks/harness/workload.txt").write_text("fixture workload\n")
    return root


@pytest.fixture
def prepared(tmp_path, monkeypatch):
    root = candidate(tmp_path)

    def identity(root, selection, roots):
        return {**selection, "resource_sha256": "fixture", "executables": {
            name: {"path": path, "sha256": perf.sha256(Path(path)), "mode": Path(path).stat().st_mode & 0o777}
            for name, path in selection["executables"].items()}}

    monkeypatch.setattr(perf, "_tool_identity", identity)
    return root


def measure(*commands):
    return {"action": "measure", "reason": "Measure the selected workload.", "commands": list(commands) or [
        {"name": "run", "argv": ["/usr/bin/true"], "cwd": ".", "kind": "run"}]}


def fake_execution(monkeypatch, *, code=0, timeout=False, hook=None):

    def run(argv, *, cwd, env, stdout, stderr, timeout):
        assert env == {}
        assert "--clearenv" in argv and "--unshare-net" in argv
        stdout.write_text('{"self_reported_metric": 17}\n')
        stderr.write_text("")
        if hook:
            hook(argv, cwd)
        return {"exit_code": code, "timeout": timed_out}

    timed_out = timeout
    monkeypatch.setattr(perf, "_run_bounded", run)


def test_receipt_binds_inputs_and_host_observations(prepared, monkeypatch):
    fake_execution(monkeypatch)
    record = perf.run_measurement(prepared, {"sequence": 4}, measure())
    checked = perf.validate_evidence(prepared, {}, [record["id"]])
    assert checked[0]["successful_run"]
    assert checked[0]["sequence"] == 4
    assert checked[0]["commands"][0]["host_elapsed_ns"] > 0
    assert checked[0]["commands"][0]["executable"]["sha256"]
    assert "self_reported_metric" not in checked[0]
    assert any(name.startswith("workspace/implementation/") for name in checked[0]["evidence_files"])


@pytest.mark.parametrize("command", [
    {"cwd": "../outside"}, {"cwd": "/build"}, {"cwd": "work/crate"}, {"cwd": "build//x"},
    {"timeout_seconds": 3601}, {"timeout_seconds": True}, {"timeout_seconds": float("nan")},
    {"argv": []}, {"argv": [""]}, {"argv": ["true", "\x00"]}, {"name": "../name"}, {"kind": "cost-proof"},
])
def test_invalid_command_contract_is_rejected(command):
    raw = measure()
    raw["commands"][0].update(command)
    with pytest.raises(ValueError):
        perf._commands(raw)


def test_exact_arguments_preserve_empty_nonfirst_values():
    raw = measure({"name": "run", "argv": ["program", "", "a b", "$(literal)"], "cwd": "build", "kind": "run"})
    assert perf._commands(raw)[0]["argv"] == raw["commands"][0]["argv"]


@pytest.mark.parametrize("change", ["source", "harness", "tool", "configuration", "mode"])
def test_source_harness_tool_and_work_configuration_drift_invalidate(prepared, monkeypatch, change):
    fake_execution(monkeypatch)
    record = perf.run_measurement(prepared, {}, measure())
    if change == "source":
        path = prepared / "work/crate/src/lib.rs"
    elif change == "harness":
        path = prepared / "benchmarks/harness/workload.txt"
    elif change == "tool":
        path = Path(json.loads((prepared / "config.json").read_text())["tools"]["cargo"])
    elif change == "configuration":
        path = prepared / "work/extraction.json"
        dump(path, {"cargo_args": ["--release"]})
    else:
        path = prepared / "benchmarks/harness/workload.txt"
        path.chmod(0o755)
    if change not in {"configuration", "mode"}:
        path.write_text(path.read_text() + "\nchanged")
    with pytest.raises(ValueError, match="stale"):
        perf.validate_evidence(prepared, {}, [record["id"]])


def test_report_authoring_does_not_change_harness_identity(prepared, monkeypatch):
    fake_execution(monkeypatch)
    record = perf.run_measurement(prepared, {}, measure())
    (prepared / "benchmarks/report.md").write_text("Interpretation and source-invocation analysis.\n")
    assert perf.validate_evidence(prepared, {}, [record["id"]])


@pytest.mark.parametrize("code,timeout", [(9, False), (-9, True)])
def test_failure_and_timeout_are_retained_but_cannot_satisfy_acceptance(prepared, monkeypatch, code, timeout):
    fake_execution(monkeypatch, code=code, timeout=timeout)
    record = perf.run_measurement(prepared, {}, measure())
    assert record["commands"][0]["exit_code"] == code
    assert record["commands"][0]["timeout"] == timeout
    assert not record["successful_run"]
    assert (prepared / record["record_path"]).exists()
    with pytest.raises(ValueError, match="successful measured run"):
        perf.validate_evidence(prepared, {}, [record["id"]])


def test_build_success_alone_is_not_a_measurement(prepared, monkeypatch):
    fake_execution(monkeypatch)
    raw = measure(); raw["commands"][0]["kind"] = "build"
    record = perf.run_measurement(prepared, {}, raw)
    with pytest.raises(ValueError, match="successful measured run"):
        perf.validate_evidence(prepared, {}, [record["id"]])


def test_stale_during_execution_is_retained(prepared, monkeypatch):
    fake_execution(monkeypatch, hook=lambda *_: (prepared / "work/crate/src/lib.rs").write_text("changed"))
    record = perf.run_measurement(prepared, {}, measure())
    assert not record["fresh_after_measurement"]
    with pytest.raises(ValueError, match="stale"):
        perf.validate_evidence(prepared, {}, [record["id"]])


def test_raw_log_tampering_is_detected(prepared, monkeypatch):
    fake_execution(monkeypatch)
    record = perf.run_measurement(prepared, {}, measure())
    (prepared / record["record_path"]).parent.joinpath(record["commands"][0]["stdout"]).write_text("fabricated")
    with pytest.raises(ValueError, match="observations changed"):
        perf.validate_evidence(prepared, {}, [record["id"]])


def test_host_paths_and_build_symlinks_are_never_hashed(tmp_path):
    workspace = tmp_path / "workspace"
    (workspace / "build").mkdir(parents=True)
    secret = tmp_path / "secret"
    secret.write_text("unmounted host material")
    command = {"argv": [str(secret)], "cwd": "."}
    assert perf._executable(command, workspace, {"PATH": "/usr/bin"}, [])[0]["sha256"] is None
    (workspace / "build/alias").symlink_to(secret)
    with pytest.raises(ValueError, match="symlinks"):
        perf._executable({"argv": ["/build/alias"], "cwd": "."}, workspace, {"PATH": "/usr/bin"}, [])


def test_direct_external_executable_drift_is_revalidated(prepared, monkeypatch):
    fake_execution(monkeypatch)
    binary = Path(json.loads((prepared / "config.json").read_text())["tools"]["cargo"]).with_name("other-tool")
    binary.write_text("original external executable")
    binary.chmod(0o755)
    record = perf.run_measurement(prepared, {}, measure({"name": "run", "argv": [str(binary)], "cwd": ".", "kind": "run"}))
    before = perf.current_basis(prepared)
    binary.write_text("changed external executable")
    assert perf.current_basis(prepared) == before
    with pytest.raises(ValueError, match="external executable changed"):
        perf.validate_evidence(prepared, {}, [record["id"]])


def test_snapshot_mounts_are_closed_and_host_credentials_not_forwarded(prepared, monkeypatch):
    monkeypatch.setenv("AWS_SECRET_ACCESS_KEY", "must-not-be-forwarded")
    observed = []
    fake_execution(monkeypatch, hook=lambda argv, cwd: observed.append((argv, cwd)))
    perf.run_measurement(prepared, {}, measure())
    argv, workspace = observed[0]
    assert "must-not-be-forwarded" not in argv
    assert ["--bind", str(workspace / "build"), "/build"] == argv[argv.index(str(workspace / "build")) - 1:argv.index(str(workspace / "build")) + 2]
    assert "--unshare-pid" in argv and "--cap-drop" in argv
    assert str(prepared) not in argv
    assert (workspace / "implementation/src/lib.rs").stat().st_ino != (prepared / "work/crate/src/lib.rs").stat().st_ino


def test_executed_workspace_artifact_and_generated_inventory_are_retained(prepared, monkeypatch):
    def compile(argv, workspace):
        if argv[-1] == "build":
            binary = workspace / "build/benchmark"
            binary.write_bytes(b"fixture machine executable")
            binary.chmod(0o755)
    fake_execution(monkeypatch, hook=compile)
    raw = measure({"name": "build", "argv": ["/usr/bin/true", "build"], "cwd": ".", "kind": "build"},
                  {"name": "run", "argv": ["/build/benchmark"], "cwd": ".", "kind": "run"})
    record = perf.run_measurement(prepared, {}, raw)
    entry = record["commands"][1]
    directory = (prepared / record["record_path"]).parent
    assert (directory / entry["retained_entrypoint"]).read_bytes() == b"fixture machine executable"
    assert "build/benchmark" in json.loads((directory / entry["generated_inputs"]).read_text())


def test_virtual_entrypoint_is_normalized_before_mount_mapping(tmp_path):
    workspace = tmp_path / "workspace"
    (workspace / "harness").mkdir(parents=True)
    (workspace / "harness/run").write_text("fixture")
    command = {"argv": ["/build/../usr/bin/true"], "cwd": "."}
    entry, path = perf._executable(command, workspace, {"PATH": "/usr/bin"}, [])
    assert entry["sha256"] == perf.sha256(Path("/usr/bin/true"))
    entry, path = perf._executable({"argv": ["../harness/run"], "cwd": "build"}, workspace, {"PATH": "/usr/bin"}, [])
    assert path == workspace / "harness/run"


def test_final_output_metrics_are_retained(prepared, monkeypatch):
    fake_execution(monkeypatch, hook=lambda argv, workspace: (workspace / "output/results.json").write_text('{"latency_ns":17}'))
    record = perf.run_measurement(prepared, {}, measure())
    assert "artifacts/output/results.json" in record["evidence_files"]
    assert perf.validate_evidence(prepared, {}, [record["id"]])


def test_oversized_output_is_explicit_and_cannot_be_accepted(prepared, monkeypatch):
    monkeypatch.setattr(perf, "MAX_OUTPUT_BYTES", 8)
    fake_execution(monkeypatch, hook=lambda argv, workspace: (workspace / "output/results.json").write_text("too much output"))
    record = perf.run_measurement(prepared, {}, measure())
    assert "retained-output limit" in record["evidence_error"]
    with pytest.raises(ValueError, match="incomplete"):
        perf.validate_evidence(prepared, {}, [record["id"]])


def test_sparse_generated_files_are_size_checked_before_hashing(tmp_path, monkeypatch):
    (tmp_path / "build").mkdir()
    (tmp_path / "output").mkdir()
    path = tmp_path / "build/huge"
    with path.open("wb") as stream:
        stream.truncate(perf.MAX_GENERATED_BYTES + 1)
    monkeypatch.setattr(perf, "sha256", lambda _: pytest.fail("Oversized generated data must not be read"))
    assert "unhashed" in perf._build_inventory(tmp_path)["build/huge"]
    entry, resolved = perf._executable({"argv": ["/build/huge"], "cwd": "."}, tmp_path, {"PATH": "/usr/bin"}, [])
    assert resolved is None and entry["identity_error"]


@pytest.mark.parametrize("kind", ["timeout", "output"])
def test_bounded_process_retains_timeout_or_output_limit(tmp_path, monkeypatch, kind):
    # A bounded host fixture tests only the process-owner utility; benchmark
    # callers always supply the closed bwrap command from _sandbox.
    monkeypatch.setattr(perf, "MAX_LOG_BYTES", 100)
    args = ["/usr/bin/python3", "-c", "import time;time.sleep(5)" if kind == "timeout" else "print('x'*10000)"]
    result = perf._run_bounded(args, cwd=tmp_path, env={}, stdout=tmp_path / "stdout", stderr=tmp_path / "stderr", timeout=.1)
    assert result["timeout"] == (kind == "timeout")
    assert result["output_limit_exceeded"] == (kind == "output")
    assert (tmp_path / "stdout").stat().st_size <= 100


@pytest.mark.skipif(os.environ.get("TRELLIS_TEST_PERFORMANCE_EXECUTION") != "1",
                    reason="Explicit opt-in required for installed compiler/bwrap probes")
@pytest.mark.parametrize("route", ["rust_aeneas", "lean_native"])
def test_real_compiled_measurement_and_namespace_isolation(tmp_path, monkeypatch, route):
    rust = Path(os.environ.get("TRELLIS_TEST_RUSTC", "/path/to/trellis/.rustup/toolchains/nightly-2026-06-01-x86_64-unknown-linux-gnu/bin/rustc"))
    lean = Path(os.environ.get("TRELLIS_TEST_LEAN", "/path/to/trellis/.elan/toolchains/leanprover--lean4---v4.30.0-rc2/bin/lean"))
    tools = ({"cargo": str(rust.with_name("cargo")), "rustc": str(rust)} if route == "rust_aeneas" else
             {"lean": str(lean), "lean_toolchain": "leanprover/lean4:v4.30.0-rc2"})
    root = candidate(tmp_path, route=route, tools=tools)
    sentinel = tmp_path / "unmounted-secret"
    sentinel.write_text("host sentinel")
    monkeypatch.setenv("AWS_SECRET_ACCESS_KEY", "host-only-secret")
    isolation = root / "benchmarks/harness/isolation.py"
    isolation.write_text(
        "import os,pathlib,socket\n"
        f"assert not pathlib.Path({str(sentinel)!r}).exists()\n"
        "assert 'AWS_SECRET_ACCESS_KEY' not in os.environ\n"
        "assert os.environ['HOME']=='/home/benchmark'\n"
        "try:\n pathlib.Path('/implementation/host-write').write_text('bad')\nexcept OSError: pass\nelse: raise AssertionError('source writable')\n"
        "try:\n socket.create_connection(('1.1.1.1',443),timeout=.1)\nexcept OSError: pass\nelse: raise AssertionError('network available')\n"
        "pathlib.Path('/output/isolation.json').write_text('{\"isolated\":true}')\n")
    commands = [{"name": "isolation", "argv": ["python3", "/harness/isolation.py"], "cwd": ".", "kind": "build"}]
    if route == "rust_aeneas":
        (root / "benchmarks/harness/main.rs").write_text(
            '#[path="/implementation/src/lib.rs"] mod implementation;\n'
            'fn main(){let start=std::time::Instant::now();let mut x=1u64;for _ in 0..1000000 {x=std::hint::black_box(implementation::algorithm(x));}'
            'println!("{{\\\"iterations\\\":1000000,\\\"checksum\\\":{},\\\"elapsed_ns\\\":{}}}",x,start.elapsed().as_nanos());}\n'.replace('\\\"','\\"'))
        commands.append({"name": "compile", "argv": ["rustc", "-C", "opt-level=3", "/harness/main.rs", "-o", "/build/benchmark"], "cwd": "build", "kind": "build"})
    else:
        (root / "benchmarks/harness/Main.lean").write_text(
            'import Tablet.Algorithm\n'
            'def main : IO Unit := do\n  let start ← IO.monoNanosNow\n  let mut n : UInt64 := 1\n'
            '  for _ in [:1000000] do\n    n := Algorithm n\n'
            '  let finish ← IO.monoNanosNow\n  IO.println s!"iterations=1000000 checksum={n} elapsed_ns={finish-start}"\n')
        commands += [
            {"name": "library", "argv": ["lake", "--no-cache", "build", "Tablet:static"], "cwd": "implementation", "kind": "build"},
            {"name": "translate", "argv": ["lake", "env", "lean", "--root=/harness", "-c", "/build/Main.c", "/harness/Main.lean"], "cwd": "implementation", "kind": "build"},
            {"name": "compile", "argv": ["lake", "env", "leanc", "-O3", "-o", "/build/benchmark", "/build/Main.c", "/implementation/.lake/build/lib/libpaper__implementation_Tablet.a"], "cwd": "implementation", "kind": "build"},
        ]
    commands.append({"name": "benchmark", "argv": ["/build/benchmark"], "cwd": "build", "kind": "run"})
    before = perf.current_basis(root)
    record = perf.run_measurement(root, {"sequence": 0}, measure(*commands))
    assert record["successful_run"], record
    assert record["basis"] == before == perf.current_basis(root)
    assert perf.validate_evidence(root, {}, [record["id"]])
    directory = (root / record["record_path"]).parent
    assert "artifacts/output/isolation.json" in record["evidence_files"]
    benchmark = record["commands"][-1]
    assert benchmark["retained_entrypoint"] in record["evidence_files"]
    assert "1000000" in (directory / benchmark["stdout"]).read_text()
    assert sentinel.read_text() == "host sentinel"
    timed = perf.run_measurement(root, {}, measure({"name": "timeout", "argv": ["python3", "-c", "import time;time.sleep(5)"],
                                                   "cwd": ".", "kind": "run", "timeout_seconds": .05}))
    assert timed["commands"][0]["timeout"]
    assert not timed["successful_run"]
