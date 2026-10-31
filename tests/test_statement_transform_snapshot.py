"""Statement provenance keeps fresh replay evidence while sharing source reads."""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import subprocess
import threading

import pytest

from trellis.atomic_actions import observations as obs
from trellis.checker import statement_transform as transform
from trellis.checker.protocol import CheckerRequest
from test_local_closure_cache_prefilter import make_server


@pytest.fixture
def server(tmp_path, monkeypatch):
    def forbidden(*args, **kwargs):
        pytest.fail("unexpected compiler process in source-snapshot regression")
    monkeypatch.setattr(subprocess, "Popen", forbidden)
    monkeypatch.setattr(obs, "_run_lake_command", forbidden)
    monkeypatch.setattr(obs, "materialize_tablet_oleans", forbidden)
    return make_server(tmp_path)


def original_environment(server):
    """The pre-optimization provenance algorithm, using real evidence checks."""
    repo = server.supervisor_repo
    return {
        "toolchain_sha256": obs._kernel_replay_toolchain_sha256(repo),
        "manifest_sha256": hashlib.sha256(server._lake_manifest_path.read_bytes()).hexdigest(),
        "dependencies": {node: obs.read_kernel_replay_artifact_evidence(repo, node)
                         for node in obs.materialization_order(repo, ["Root"])},
    }


def request():
    return CheckerRequest(op="lean_statement_transform", request_id=73, timeout_secs=1,
        raw={"input": {"name": "Original", "statement": "theorem Original : True := by",
                       "context": "import Tablet.Root\n"}})


def write_same_mtime(path, data):
    before = path.stat()
    path.write_bytes(data)
    os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))


@pytest.mark.parametrize("failed,expected", [
    ({"returncode": 1, "stdout": "error: Mathlib/Topology/ContinuousOn.ir.hash: Permission denied\n",
      "stderr": "error: build failed\n"},
     ["ContinuousOn.ir.hash: Permission denied", "error: build failed"]),
    ({"returncode": None, "stdout": "building Tablet.Root", "timed_out": True},
     ["building Tablet.Root", "timed out"]),
    ({"returncode": None, "spawn_error": "lake executable not found"},
     ["lake executable not found"]),
])
def test_dependency_failure_retains_diagnostics_and_never_derives(server, monkeypatch, failed, expected):
    monkeypatch.setattr(server, "_ensure_closures_have_current_kernel_replay", lambda *a, **kw: failed)
    response, code, extra = transform.handle(server, request())
    assert code == 1 and response["status"] == "invalid_statement"
    assert all(message in response["error"] for message in expected)
    assert "result" not in response and not extra


def test_derivation_failure_retains_stdout_with_stderr_summary(server, monkeypatch):
    monkeypatch.setattr(obs, "_run_lake_command", lambda *a, **kw: {
        "returncode": 1, "stdout": "Proof.lean:3:9: error: unknown identifier 'missing'\n",
        "stderr": "Lean exited with code 1\n"})
    response, code, _ = transform.handle(server, request())
    assert code == 1 and response["status"] == "invalid_statement"
    assert "unknown identifier 'missing'" in response["error"]
    assert "Lean exited with code 1" in response["error"]
    assert "result" not in response


def test_dependency_timeout_does_not_repeat_transport_message(server, monkeypatch):
    message = "lake build timed out after 600 seconds"
    monkeypatch.setattr(server, "_ensure_closures_have_current_kernel_replay", lambda *a, **kw: {
        "timed_out": True, "stderr": message, "spawn_error": message})
    response, code, _ = transform.handle(server, request())
    assert code == 1 and response["error"] == message


def test_provenance_matches_original_without_rewalking_each_source_closure(server, monkeypatch):
    expected = original_environment(server)
    assert all(expected["dependencies"].values())
    monkeypatch.setattr(obs, "tablet_source_closure_hash",
        lambda *args: pytest.fail("individual source closure was rewalked"))
    assert transform._environment(server, ["Root"]) == expected
    assert transform._environment(server, ["Root"]) == expected


@pytest.mark.parametrize("relative", ["Tablet/Dep.lean", "lean-toolchain", "lake-manifest.json",
                                      ".trellis/scripts/check.py"])
def test_provenance_rejects_same_mtime_source_or_environment_change(server, relative):
    assert transform._environment(server, ["Root"])
    path = server.supervisor_repo / relative
    write_same_mtime(path, path.read_bytes() + b"\n-- changed\n")
    with pytest.raises(ValueError, match="current replay evidence"):
        transform._environment(server, ["Root"])


@pytest.mark.parametrize("part", ["", ".server", ".private", ".srcclosure"])
def test_provenance_never_memoizes_artifact_or_replay_bytes(server, part):
    assert transform._environment(server, ["Root"])
    path = Path(str(obs._tablet_olean_path(server.supervisor_repo, "Dep")) + part)
    write_same_mtime(path, path.read_bytes() + b"changed")
    with pytest.raises(ValueError, match="current replay evidence"):
        transform._environment(server, ["Root"])


def test_provenance_rechecks_source_bytes_after_reading_evidence(server, monkeypatch):
    original = obs.read_kernel_replay_artifact_evidence
    def changed(repo, node, **kwargs):
        evidence = original(repo, node, **kwargs)
        if node == "Root":
            path = repo / "Tablet/Dep.lean"
            write_same_mtime(path, path.read_bytes().replace(b"1", b"2"))
        return evidence
    monkeypatch.setattr(obs, "read_kernel_replay_artifact_evidence", changed)
    with pytest.raises(ValueError, match="changed during provenance capture"):
        transform._environment(server, ["Root"])
    with pytest.raises(ValueError, match="current replay evidence"):
        transform._environment(server, ["Root"])


def test_provenance_oversize_falls_back_to_original_checks(server, monkeypatch):
    expected = original_environment(server)
    monkeypatch.setattr(obs.SourceClosureSnapshot, "MAX_BYTES", 1)
    assert transform._environment(server, ["Root"]) == expected
    (server.supervisor_repo / "Tablet/Dep.lean").unlink()
    with pytest.raises(ValueError, match="current replay evidence"):
        transform._environment(server, ["Root"])


def test_sync_writer_cannot_change_inputs_during_snapshot_capture(server, monkeypatch):
    expected = original_environment(server)
    reading, release, waiting, written = [threading.Event() for _ in range(4)]
    original = obs.read_kernel_replay_artifact_evidence
    def read(repo, node, **kwargs):
        evidence = original(repo, node, **kwargs)
        if node == "Root" and not reading.is_set():
            reading.set()
            assert release.wait(5), "test did not release the provenance reader"
        return evidence
    monkeypatch.setattr(obs, "read_kernel_replay_artifact_evidence", read)
    def sync_writer():
        waiting.set()
        with server._sync_lock:
            path = server.supervisor_repo / "Tablet/Dep.lean"
            write_same_mtime(path, path.read_bytes().replace(b"1", b"2"))
            written.set()
    with ThreadPoolExecutor(2) as pool:
        result = pool.submit(transform._environment, server, ["Root"])
        try:
            assert reading.wait(5)
            writer = pool.submit(sync_writer)
            assert waiting.wait(5)
            assert not written.wait(.05)
        finally:
            release.set()
        assert result.result(timeout=5) == expected
        writer.result(timeout=5)
    assert written.is_set()
    with pytest.raises(ValueError, match="current replay evidence"):
        transform._environment(server, ["Root"])


def test_cold_materialization_and_warm_hit_keep_existing_evidence_checks(server, monkeypatch):
    repo = server.supervisor_repo
    dep = repo / "Tablet/Dep.lean"
    write_same_mtime(dep, dep.read_bytes().replace(b"1", b"2"))
    materializations, compilations = [], []
    def materialize(actual_repo, nodes, **kwargs):
        assert actual_repo == repo
        assert not server._sync_lock.locked()
        materializations.append(list(nodes))
        for node in obs.materialization_order(repo, nodes):
            obs.write_olean_srcclosure(repo, node, obs.tablet_source_closure_hash(repo, node))
            assert obs._write_kernel_replay_attestation(repo, node, olean_sha256="unused",
                declaration_manifest=[{"name": node, "kind": "theorem"}])
        return {"returncode": 0}
    def compile(actual_repo, args, **kwargs):
        assert actual_repo == repo
        assert not server._sync_lock.locked()
        compilations.append(list(args))
        return {"returncode": 0, "stdout": json.dumps({"status": "ok", "result": {"refutation_type": "¬ True"}})}
    monkeypatch.setattr(obs, "materialize_tablet_oleans", materialize)
    monkeypatch.setattr(obs, "_run_lake_command", compile)
    with server._workspace_lock:
        first, code, extra = transform.handle(server, request())
        second, next_code, next_extra = transform.handle(server, request())
    assert code == next_code == 0 and first == second
    assert extra == {"statement_transform_cache_hit": False}
    assert next_extra == {"statement_transform_cache_hit": True}
    assert materializations == [["Root"]] and len(compilations) == 1
    # A successful cache record never substitutes for current artifact replay.
    obs._tablet_olean_path(repo, "Dep").write_bytes(b"changed after cache publication")
    monkeypatch.setattr(obs, "materialize_tablet_oleans", lambda *a, **k:
        {"returncode": 1, "stderr": "fixture replay rejected changed artifact"})
    with server._workspace_lock:
        rejected, code, _ = transform.handle(server, request())
    assert code == 1 and "replay rejected" in rejected["error"]
    assert len(compilations) == 1


def test_cold_derivation_rechecks_fresh_source_snapshot_after_compilation(server, monkeypatch):
    def compile(repo, args, **kwargs):
        assert not server._sync_lock.locked()
        path = repo / "Tablet/Dep.lean"
        write_same_mtime(path, path.read_bytes().replace(b"1", b"2"))
        return {"returncode": 0, "stdout": json.dumps({"status": "ok", "result": {"refutation_type": "¬ True"}})}
    monkeypatch.setattr(obs, "_run_lake_command", compile)
    with server._workspace_lock:
        rejected, code, _ = transform.handle(server, request())
    assert code == 1 and "current replay evidence" in rejected["error"]
    cache = server.local_closure_axioms_cache_dir.parent / "statement-transforms"
    assert not list(cache.glob("*.json"))
