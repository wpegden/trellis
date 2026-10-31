"""Semantic collection keeps replay, exact payloads and one-process bounds."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import time

import pytest

from trellis.atomic_actions import observations as obs
from trellis.checker import admission, compiled_probe
from trellis.checker.compiled_probe import CompiledClosureProbe, CompiledSemanticProbe, lake_probe_command
from trellis.checker.protocol import validate_request


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


@pytest.fixture
def semantic_server(tmp_path, monkeypatch):
    from test_local_closure_cache_prefilter import make_server
    server = make_server(tmp_path)
    server._compiled_semantic_probe = CompiledSemanticProbe(server.supervisor_repo)
    monkeypatch.setenv("TRELLIS_SEMANTIC_PRECOMPILE", "1")
    paths = {name: tmp_path / name for name in ("lean", "lake")}
    for name, path in paths.items():
        path.write_bytes(name.encode())
    def identity():
        return ({"checker_script_hash": sha(server._fingerprint_script_path),
                 "toolchain_hash": sha(server._toolchain_path),
                 "lean_executable_hash": sha(paths["lean"]),
                 "lake_executable_hash": sha(paths["lake"])}, paths["lean"], paths["lake"])
    monkeypatch.setattr(server, "_semantic_execution_identity", identity)
    compiles, probes = [], []
    def compile(args, **kwargs):
        Path(args[2]).write_bytes(b"trusted collector")
        compiles.append(args)
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")
    def probe(repo, args, **kwargs):
        probes.append((args, kwargs))
        if "--shared-root" in args:
            pairs = args[args.index("--shared-root") + 2:]
            stdout = "\n".join(f"FP\t{node}\t{principal}-payload" for node, principal in zip(pairs[::2], pairs[1::2]))
        else:
            node, principal = args[-2:]
            stdout = f"FP\t{node}\t{principal}-payload"
        return dict(returncode=0, stdout=stdout, stderr="")
    monkeypatch.setattr(compiled_probe, "guarded_run", compile)
    monkeypatch.setattr(obs, "_run_lake_command", probe)
    return server, paths, compiles, probes


def request():
    return validate_request(dict(op="lean_semantic_payloads", request_id=1,
                                 nodes=["Root", "Dep"], principal_names={"Root": "root", "Dep": "dep"}))


def test_server_misses_use_pinned_tools_and_one_requested_root(semantic_server, monkeypatch):
    server, tools, compiles, probes = semantic_server
    response, _, _ = server._handle_lean_semantic_payloads(request())
    assert response["nodes"] == {node: dict(ok=True, payload=principal + "-payload", error="")
                                 for node, principal in (("Root", "root"), ("Dep", "dep"))}
    assert len(compiles) == 1 and len(probes) == 1
    for args, kwargs in probes:
        assert args[0:2] == ["env", "/bin/sh"]
        assert str(tools["lean"]) in args
        assert kwargs["lake_executable"] == tools["lake"]
        assert kwargs["bwrap_role"] == "lake_compiler"
        program = Path(args[args.index("--run") + 1])
        assert program.name == "RunSemantic.lean"
        assert "TrellisSemantic_" in program.read_text()
        from trellis.sandbox import _repo_writable_paths
        assert not any(program.is_relative_to(p) for p in
                       _repo_writable_paths(server.supervisor_repo, role="lake_compiler", is_isabelle=False))
    monkeypatch.setattr(server._compiled_semantic_probe, "prepare", lambda *a, **kw: pytest.fail("cached payload prepared collector"))
    cached, _, extra = server._handle_lean_semantic_payloads(request())
    assert cached == response and extra["cache_hits"] == 2
    assert len(probes) == 1


def test_replay_failure_never_prepares_or_runs_a_collector(semantic_server, monkeypatch):
    server, _, compiles, probes = semantic_server
    monkeypatch.setattr(server, "_ensure_closures_have_current_kernel_replay",
                        lambda *a, **kw: dict(returncode=1, stderr="original replay error"))
    response, code, _ = server._handle_lean_semantic_payloads(request())
    assert code == 1 and not compiles and not probes
    assert all(row == dict(ok=False, payload="", error="original replay error") for row in response["nodes"].values())


def test_revoked_workspace_propagates_auth_error_without_observation(semantic_server):
    from trellis.checker.protocol import ProtocolError
    server, _, compiles, probes = semantic_server
    def revoked():
        raise ProtocolError("auth_required", "workspace revoked")
    with admission.authoritative_workspace("revoked", cancelled=revoked):
        with pytest.raises(ProtocolError, match="workspace revoked"):
            server._handle_lean_semantic_payloads(request())
    assert not compiles and not probes


@pytest.mark.parametrize("axis", ["lean", "lake", "toolchain", "script"])
def test_tool_or_source_drift_discards_payloads_and_caches_nothing(semantic_server, monkeypatch, axis):
    server, tools, _, _ = semantic_server
    # Never edit the checkout's trusted collector in this fault injection.
    script = server.supervisor_repo / ".trellis/scripts/semantic.lean"
    script.write_bytes(server._fingerprint_script_path.read_bytes())
    server._fingerprint_script_path = script
    target = {**tools, "toolchain": server._toolchain_path, "script": script}[axis]
    original = obs._run_lake_command
    def drift(*args, **kwargs):
        response = original(*args, **kwargs)
        target.write_bytes(target.read_bytes() + b"changed")
        return response
    monkeypatch.setattr(obs, "_run_lake_command", drift)
    response, _, _ = server._handle_lean_semantic_payloads(request())
    assert all(not row["ok"] and not row["payload"] and "identity changed" in row["error"]
               for row in response["nodes"].values())
    assert not list(server.semantic_payload_cache_dir.glob("*.json"))


def test_compiled_and_source_observation_preserve_error_text(tmp_path, monkeypatch):
    source = tmp_path / "collector.lean"
    source.write_text("import Lean\n")
    messages = {"A": "exact principal is absent", "B": "timed out after 3.0s"}
    def run(repo, args, **kwargs):
        node = args[-2]
        return (dict(returncode=0, stdout=f"ERR\t{node}\t{messages[node]}") if node == "A"
                else dict(returncode=None, timed_out=True, stderr=messages[node]))
    monkeypatch.setattr(obs, "_run_lake_command", run)
    kwargs = dict(bwrap_role="lake_compiler", script_path=source, timeout_secs=3)
    plain = obs.observe_lean_semantic_payloads(tmp_path, ["A", "B"], **kwargs)
    compiled = obs.observe_lean_semantic_payloads(tmp_path, ["A", "B"], **kwargs,
        lean_executable=Path("/pinned/lean"), lake_executable=Path("/pinned/lake"), probe_directory=tmp_path)
    assert plain == compiled == {node: dict(ok=False, payload="", error=error) for node, error in messages.items()}


def test_host_collector_arguments_never_cross_worker_rpc(tmp_path, monkeypatch):
    monkeypatch.setattr(obs, "_require_socket_path", lambda: tmp_path / "checker.sock")
    calls = []
    def client(*args, **kwargs):
        calls.append(kwargs)
        return {}
    monkeypatch.setattr(obs, "client_lean_semantic_payloads", client)
    obs.observe_lean_semantic_payloads(tmp_path, ["Root"], script_path=tmp_path / "untrusted.lean",
        lean_executable=tmp_path / "lean", lake_executable=tmp_path / "lake", probe_directory=tmp_path)
    assert calls == [{"timeout_secs": obs.LEAN_SUPPORT_TIMEOUT_SECS}]


LEAN = Path.home() / ".elan/toolchains/leanprover--lean4---v4.33.0/bin/lean"


@pytest.mark.skipif(not LEAN.is_file() or not shutil.which("bwrap"), reason="bwrap and pinned Lean v4.33.0 are required")
def test_real_lean_payloads_shadowing_and_bounded_benchmark(tmp_path, monkeypatch):
    """Two two-node rounds per runner; no unbounded process or live project."""
    # The confined runtime invocation inherits the test process environment,
    # unlike the separately constructed artifact-build env below. Pin both.
    monkeypatch.setenv("PATH", str(LEAN.parent) + os.pathsep + os.environ["PATH"])
    for name in ("LEAN_PATH", "LEAN_SYSROOT", "ELAN_TOOLCHAIN"):
        monkeypatch.delenv(name, raising=False)
    monkeypatch.setenv("TRELLIS_SEMANTIC_PRECOMPILE", "1")
    monkeypatch.setenv("TRELLIS_LEAN_PARALLELISM", "1")
    monkeypatch.setattr(admission, "ACTIVE", admission.Admission(1))
    repo = tmp_path / "fixture"
    tablet = repo / "Tablet"
    tablet.mkdir(parents=True)
    artifacts = repo / ".lake/build/lib/lean"
    (artifacts / "Tablet").mkdir(parents=True)
    (repo / "lean-toolchain").write_text("leanprover/lean4:v4.33.0\n")
    (repo / "lakefile.toml").write_text('name = "semantic_fixture"\n[[lean_lib]]\nname = "Tablet"\n')
    (repo / "lake-manifest.json").write_text(json.dumps(dict(
        version="1.2.0", packagesDir=".lake/packages", packages=[],
        name="semantic_fixture", lakeDir=".lake", fixedToolchain=False,
    )))
    (tablet / "Data.lean").write_text("structure Box where\n  value : Nat\ndef data : Box := ⟨7⟩\n")
    (tablet / "Root.lean").write_text("import Tablet.Data\nnamespace Fixture\ntheorem root : data.value = 7 := rfl\nend Fixture\n")
    env = {k: v for k, v in os.environ.items() if not k.startswith("LEAN_")}
    env.update(LEAN_PATH=str(artifacts), LEAN_NUM_THREADS="1")
    @admission.admitted
    def run(args, **kwargs):
        return admission.guarded_run(args, timeout=30, cwd=repo, env=env, capture_output=True, text=True, **kwargs)
    for node in ("Data", "Root"):
        built = run([str(LEAN), "-o", str(artifacts / f"Tablet/{node}.olean"), str(tablet / f"{node}.lean")])
        assert built.returncode == 0, built.stdout + built.stderr
    source = Path(__file__).resolve().parents[1] / "scripts/lean_semantic_fingerprint.lean"
    identity = dict(checker_script_hash=sha(source), toolchain_hash=sha(repo / "lean-toolchain"), lean_executable_hash=sha(LEAN))
    probe = CompiledSemanticProbe(repo)
    started = time.monotonic()
    program, directory = probe.prepare(source, identity, LEAN, timeout_secs=120)
    cold = time.monotonic() - started
    assert directory != CompiledClosureProbe(repo).root / directory.name
    started = time.monotonic()
    assert probe.prepare(source, identity, LEAN, timeout_secs=120) == (program, directory)
    warm = time.monotonic() - started
    calls = []
    confined_run = obs._run_lake_command
    def lake(repo, args, **kwargs):
        calls.append(args)
        result = confined_run(repo, args, **kwargs)
        assert result["returncode"] == 0, result
        return result
    monkeypatch.setattr(obs, "_run_lake_command", lake)
    shared = dict(bwrap_role="lake_compiler", lean_executable=LEAN, lake_executable=LEAN.with_name("lake"),
                  principal_names={"Data": "data", "Root": "Fixture.root"})
    timings = {"source": [], "compiled": []}
    outputs = {}
    for _ in range(2):
        for kind, script, protected in (("source", source, None), ("compiled", program, directory)):
            started = time.monotonic()
            result = obs.observe_lean_semantic_payloads(repo, ["Data", "Root"], **shared,
                script_path=script, probe_directory=protected)
            timings[kind].append(time.monotonic() - started)
            assert all(entry["ok"] for entry in result.values()), result
            outputs[kind] = result
        assert outputs["source"] == outputs["compiled"]
    assert len(calls) == 8 and all(args[-2:] in (["Data", "data"], ["Root", "Fixture.root"]) for args in calls)
    # A candidate module with the same hash-derived name must lose to the
    # protected path that is prepended after Lake builds LEAN_PATH.
    module = program.read_text().split()[1]
    shadow = repo / f"{module}.lean"
    shadow.write_text('def main (_ : List String) : IO Unit := IO.println "FP\\tRoot\\tforged"\n')
    built = run([str(LEAN), "-o", str(artifacts / f"{module}.olean"), str(shadow)])
    assert built.returncode == 0, built.stdout + built.stderr
    unprotected = run([str(LEAN.with_name("lake")), *lake_probe_command(LEAN, program, ["Root", "Fixture.root"], None)])
    assert "forged" in unprotected.stdout
    protected = obs.observe_lean_semantic_payloads(repo, ["Root"], **shared, script_path=program, probe_directory=directory)
    assert protected["Root"] == outputs["source"]["Root"]
    bad = dict(shared, principal_names={"Root": "Fixture.absent"})
    plain_error = obs.observe_lean_semantic_payloads(repo, ["Root"], **bad, script_path=source)
    compiled_error = obs.observe_lean_semantic_payloads(repo, ["Root"], **bad, script_path=program, probe_directory=directory)
    assert plain_error == compiled_error and not plain_error["Root"]["ok"]
    benchmark = dict(nodes_per_round=2, rounds=2, cold_prepare_seconds=cold, warm_prepare_seconds=warm,
                     source_seconds=timings["source"], compiled_seconds=timings["compiled"],
                     source_median=statistics.median(timings["source"]), compiled_median=statistics.median(timings["compiled"]))
    print("SEMANTIC_BENCHMARK " + json.dumps(benchmark, sort_keys=True))
