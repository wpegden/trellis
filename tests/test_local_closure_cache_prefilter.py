"""Negative-only lookup tests. Synthetic artifacts; no Lean/Isabelle processes.

The real key, bundle, provenance, readiness, handler and dispatcher code runs
against temporary files. Only sync and the external probe are stubbed. A frozen
3a8c24f8 validator supplies the before-change control-flow oracle.
"""
from __future__ import annotations

import hashlib
import inspect
import json
import subprocess
import textwrap
from pathlib import Path
from types import MethodType

import pytest

from trellis.atomic_actions import observations as obs
from trellis.checker import server as mod, telemetry
from trellis.checker.protocol import CheckerRequest
from trellis.checker.server import CheckerServer
from trellis.checker.sync import SyncError


BASELINE = Path(__file__).parent / "fixtures/local_closure_cache_validator_3a8c24f8.py"
namespace = dict(vars(mod))
exec(compile(BASELINE.read_text(), str(BASELINE), "exec"), namespace)
old_validator = namespace["_try_local_closure_axioms_cache_hit"]


def make_server(tmp: Path):
    repo = tmp / "repo"
    runtime = repo / ".trellis/runtime/test"
    runtime.mkdir(parents=True)
    server = CheckerServer(runtime, parallelism=1, socket_group_gid=None)
    server.supervisor_repo = repo
    server._toolchain_path = repo / "lean-toolchain"
    server._lake_manifest_path = repo / "lake-manifest.json"
    server._local_closure_script_path = repo / ".trellis/scripts/local_closure.lean"
    server._local_closure_script_path.parent.mkdir()
    server._local_closure_script_path.write_text("-- fixture probe\n")
    (repo / ".trellis/scripts/check.py").write_text("# fixture check\n")
    (repo / "lakefile.lean").write_text("package fixture\n")
    server._toolchain_path.write_text("leanprover/lean4:fixture\n")
    server._lake_manifest_path.write_text("{}\n")
    tablet = repo / "Tablet"
    tablet.mkdir()
    sources = {
        "Preamble": "-- fixture preamble\n",
        "Dep": "import Tablet.Preamble\ndef dep := 1\n",
        "Root": "import Tablet.Dep\ntheorem root : True := trivial\n",
    }
    fingerprints = {}
    for node, source in sources.items():
        (tablet / f"{node}.lean").write_text(source)
        fingerprints[f"{node}.lean"] = {"sha256": hashlib.sha256(source.encode()).hexdigest()}
        artifact = obs._tablet_olean_path(repo, node)
        artifact.parent.mkdir(parents=True, exist_ok=True)
        for suffix in ("", ".server", ".private"):
            Path(str(artifact) + suffix).write_bytes(f"synthetic {node}{suffix}".encode())
    for node in sources:
        obs.write_olean_srcclosure(repo, node, obs.tablet_source_closure_hash(repo, node))
        assert obs._write_kernel_replay_attestation(
            repo, node, olean_sha256="unused",
            declaration_manifest=[{"name": node, "kind": "theorem"}],
        )
    server.fingerprint_cache_path.parent.mkdir(parents=True, exist_ok=True)
    server.fingerprint_cache_path.write_text(json.dumps(fingerprints))
    return server


def request(node="Root", **raw):
    return CheckerRequest(op="local_closure_axioms", request_id=73,
                          node_name=node, timeout_secs=1,
                          raw={"principal_name": node, **raw})


def key(server, req):
    base = mod.compute_semantic_payload_cache_key(
        server.supervisor_repo, req.node_name,
        mod.load_fingerprint_cache(server.fingerprint_cache_path),
        mod._sha256_file_or_empty(server._local_closure_script_path),
        mod._sha256_file_or_empty(server._toolchain_path),
        mod._sha256_file_or_empty(server._lake_manifest_path),
        mod.LOCAL_CLOSURE_AXIOMS_CACHE_VERSION,
    )
    assert base is not None
    principal = "<module-owner>" if req.raw.get("module_owner") else req.raw["principal_name"]
    return (base + "-principal-" + hashlib.sha256(principal.encode()).hexdigest()
            + ("-noax" if req.raw.get("no_axcheck") else ""))


def candidate(server, req, response=None):
    cache_key = key(server, req)
    mod.store_local_closure_axioms(
        server.local_closure_axioms_cache_dir, cache_key, node_name=req.node_name,
        response=response or {"request_id": 1, "status": "ok", "returncode": 0},
        cache_version=mod.LOCAL_CLOSURE_AXIOMS_CACHE_VERSION,
    )
    return server.local_closure_axioms_cache_dir / f"{cache_key}.json"


@pytest.fixture(autouse=True)
def no_external_processes(monkeypatch):
    def forbidden(*args, **kwargs):
        pytest.fail("external process/build attempted by focused prefilter test")
    monkeypatch.setattr(subprocess, "Popen", forbidden)
    monkeypatch.setattr(obs, "_run_lake_command", forbidden)
    monkeypatch.setattr(obs, "materialize_tablet_oleans", forbidden)


@pytest.fixture
def server(tmp_path):
    return make_server(tmp_path)


def measure(fn):
    state = telemetry.Measurement()
    token = telemetry._current.set(state)
    try:
        result = fn()
        return result, state.snapshot()
    finally:
        telemetry._current.reset(token)


def stub_dispatch(monkeypatch, server):
    monkeypatch.setattr(mod, "sync_tablet_dir", lambda *a: {})
    calls = []
    def probe(repo, args, **kwargs):
        calls.append(args)
        scan = "--scan-only" in args
        return {"returncode": 0, "stdout": json.dumps({
            "status": "ok", "root_kind": "theorem", "principal_declaration": "Root",
            "declaration_manifest": [] if scan else [{"name": "Root", "kind": "theorem"}],
            "kernel_axioms": ["Classical.choice"], "errors": [],
            "axiomization_check": {"agreed": True},
        }), "stderr": "", "timed_out": False, "spawn_error": ""}
    monkeypatch.setattr(obs, "_run_lake_command", probe)
    return calls


def test_authoritative_branch_is_byte_identical():
    current = textwrap.dedent(inspect.getsource(CheckerServer._try_local_closure_axioms_cache_hit))
    addition = ("    if self._local_closure_axioms_cache_definitely_misses(request) is True:\n"
                "        return None\n")
    assert current.count(addition) == 1
    assert current.replace(addition, "") == BASELINE.read_text().split("\n", 1)[1]


@pytest.mark.parametrize("damage", ["absent", "json", "version", "key", "response", "not-object"])
def test_missing_invalid_evidence_only_returns_boolean_miss(server, damage):
    req = request()
    path = candidate(server, req)
    record = json.loads(path.read_text())
    if damage == "absent":
        path.unlink()
    elif damage == "json":
        path.write_text("{")
    elif damage == "not-object":
        path.write_text("[]")
    else:
        record[{"version": "cache_version", "key": "key_blob_sha256", "response": "response"}[damage]] = "invalid"
        path.write_text(json.dumps(record))
    assert server._local_closure_axioms_cache_definitely_misses(req) is True
    result, timing = measure(lambda: server._try_local_closure_axioms_cache_hit(req))
    assert result is None
    assert "replay_readiness" not in timing["spans"]
    assert "source_closure_hash" not in timing["spans"]


def test_warm_hit_runs_original_validator_and_discards_advisory_products(server, monkeypatch):
    req = request()
    candidate(server, req)
    original_hash = mod._sha256_file_or_empty
    original_load = mod.load_local_closure_axioms
    original_key = mod.compute_semantic_payload_cache_key
    events = []
    def file_hash(path):
        events.append("hash")
        return original_hash(path)
    def compute(*args, **kwargs):
        events.append("key")
        return original_key(*args, **kwargs)
    def load(*args, **kwargs):
        events.append("load")
        value = original_load(*args, **kwargs)
        # Poison only the advisory response. It must never leave the lookup.
        if events.count("load") == 1:
            value["response"]["advisory_poison"] = True
        return value
    ready = server._closures_have_current_kernel_replay
    def readiness(nodes):
        events.append("readiness")
        return ready(nodes)
    monkeypatch.setattr(mod, "_sha256_file_or_empty", file_hash)
    monkeypatch.setattr(mod, "compute_semantic_payload_cache_key", compute)
    monkeypatch.setattr(mod, "load_local_closure_axioms", load)
    monkeypatch.setattr(server, "_closures_have_current_kernel_replay", readiness)
    hit = server._try_local_closure_axioms_cache_hit(req)
    assert hit[0] == {"request_id": 73, "returncode": 0, "status": "ok"}
    assert events == ["hash"] * 3 + ["key", "load", "readiness"] + ["hash"] * 3 + ["key", "load"]


@pytest.mark.parametrize("stage", ["hash", "fingerprints", "key", "load"])
def test_unexpected_advisory_error_defers_to_fresh_validator(server, monkeypatch, stage):
    req = request()
    candidate(server, req)
    name = {"hash": "_sha256_file_or_empty", "fingerprints": "load_fingerprint_cache",
            "key": "compute_semantic_payload_cache_key", "load": "load_local_closure_axioms"}[stage]
    original = getattr(mod, name)
    calls = []
    def fail_once(*args, **kwargs):
        calls.append(1)
        if len(calls) == 1:
            raise RuntimeError("injected advisory failure")
        return original(*args, **kwargs)
    monkeypatch.setattr(mod, name, fail_once)
    result, timing = measure(lambda: server._try_local_closure_axioms_cache_hit(req))
    assert result[2] == {"local_closure_axioms_cache_hit": True}
    assert timing["spans"]["replay_readiness"]["calls"] == 1
    assert len(calls) >= 2


@pytest.mark.parametrize("hint", [False, None, {"response": {"status": "ok"}}, "trusted-key", True])
def test_lying_or_non_boolean_hint_cannot_create_success(server, monkeypatch, hint):
    req = request()
    candidate(server, req)
    # Real readiness must reject stale source regardless of the hint.
    (server.supervisor_repo / "Tablet/Dep.lean").write_text("-- changed dependency\n")
    monkeypatch.setattr(server, "_local_closure_axioms_cache_definitely_misses", lambda req: hint)
    assert server._try_local_closure_axioms_cache_hit(req) is None


@pytest.mark.parametrize("mutation", ["source", "dependency", "imports", "toolchain", "manifest", "script",
                                      "exported", "private", "missing-server", "missing-source", "unattested", "sidecar"])
def test_mutation_after_possible_candidate_cannot_serve_advisory_response(server, monkeypatch, mutation):
    req = request()
    path = candidate(server, req)
    repo = server.supervisor_repo
    prefilter = server._local_closure_axioms_cache_definitely_misses
    def mutate_after_hint(req):
        assert prefilter(req) is False
        paths = {"source": repo / "Tablet/Root.lean", "dependency": repo / "Tablet/Dep.lean",
                 "imports": repo / "Tablet/Dep.lean", "toolchain": server._toolchain_path,
                 "manifest": server._lake_manifest_path, "script": server._local_closure_script_path,
                 "exported": obs._tablet_olean_path(repo, "Root"),
                 "private": Path(str(obs._tablet_olean_path(repo, "Dep")) + ".private")}
        if mutation in paths:
            paths[mutation].write_text("import Tablet.Missing\n" if mutation == "imports" else "changed\n")
        elif mutation == "missing-server":
            Path(str(obs._tablet_olean_path(repo, "Root")) + ".server").unlink()
        elif mutation == "missing-source":
            (repo / "Tablet/Dep.lean").unlink()
        elif mutation == "unattested":
            obs._olean_srcclosure_sidecar_path(repo, "Root").unlink()
        else:
            path.unlink()
        return False
    monkeypatch.setattr(server, "_local_closure_axioms_cache_definitely_misses", mutate_after_hint)
    assert server._try_local_closure_axioms_cache_hit(req) is None
    assert old_validator(server, req) is None


@pytest.mark.parametrize("node,raw", [
    ("Root", {"principal_name": "Root"}), ("Root", {"principal_name": "Different"}),
    ("Root", {"principal_name": "Root", "no_axcheck": True}),
    ("Preamble", {"module_owner": True, "principal_name": None}),
    ("Preamble", {"module_owner": True, "principal_name": None, "no_axcheck": True}),
])
def test_principal_and_mode_use_identical_keys(server, node, raw):
    req = request(node, **raw)
    candidate(server, req)
    assert server._local_closure_axioms_cache_definitely_misses(req) is False
    assert server._try_local_closure_axioms_cache_hit(req) == old_validator(server, req)
    other = request(node, **{**raw, "no_axcheck": not raw.get("no_axcheck", False)})
    assert server._local_closure_axioms_cache_definitely_misses(other) is True
    assert server._try_local_closure_axioms_cache_hit(other) is None
    if not raw.get("module_owner"):
        other = request(node, principal_name="Absent")
        assert server._local_closure_axioms_cache_definitely_misses(other) is True
        assert server._try_local_closure_axioms_cache_hit(other) is None


@pytest.mark.parametrize("node,raw", [
    ("Root", {"principal_name": None}), ("Root", {"principal_name": ""}),
    ("Root", {"principal_name": 12}), ("Root", {"module_owner": True, "principal_name": None}),
    ("Preamble", {"module_owner": True, "principal_name": "Preamble"}),
    ("Preamble", {"module_owner": True, "principal_name": ""}),
])
def test_uncertain_principal_and_mode_defer_without_changing_validation(server, node, raw):
    req = request(node, **raw)
    assert server._local_closure_axioms_cache_definitely_misses(req) is False
    assert server._try_local_closure_axioms_cache_hit(req) == old_validator(server, req) is None


def test_scan_only_bypasses_prefilter_readiness_and_persistence(server, monkeypatch):
    req = request(scan_only=True)
    candidate(server, request())
    calls = stub_dispatch(monkeypatch, server)
    def forbidden(*args, **kwargs):
        pytest.fail("scan-only used full-probe cache or replay readiness")
    monkeypatch.setattr(server, "_local_closure_axioms_cache_definitely_misses", forbidden)
    monkeypatch.setattr(server, "_closures_have_current_kernel_replay", forbidden)
    monkeypatch.setattr(mod, "store_local_closure_axioms", forbidden)
    result = server._handle_request(req)
    assert result["status"] == "ok"
    assert len(calls) == 1 and "--scan-only" in calls[0]


def test_full_miss_differential_preserves_response_keys_evidence_and_four_guards(server, monkeypatch):
    req = request()
    calls = stub_dispatch(monkeypatch, server)
    new_validator = server._try_local_closure_axioms_cache_hit
    monkeypatch.setattr(server, "_try_local_closure_axioms_cache_hit", MethodType(old_validator, server))
    before, old_timing = measure(lambda: server._handle_request(req))
    cache_path = server.local_closure_axioms_cache_dir / f"{key(server, req)}.json"
    before_cache = json.loads(cache_path.read_text())
    before_evidence = obs.read_kernel_replay_artifact_evidence(server.supervisor_repo, "Root")
    before_hash = obs.tablet_source_closure_hash(server.supervisor_repo, "Root")
    cache_path.unlink()
    monkeypatch.setattr(server, "_try_local_closure_axioms_cache_hit", new_validator)
    after, new_timing = measure(lambda: server._handle_request(req))
    after_cache = json.loads(cache_path.read_text())
    before_cache.pop("created_ts")
    after_cache.pop("created_ts")
    assert before == after
    assert before_cache == after_cache
    assert before_evidence == obs.read_kernel_replay_artifact_evidence(server.supervisor_repo, "Root")
    assert before_hash == obs.tablet_source_closure_hash(server.supervisor_repo, "Root")
    assert len(calls) == 2
    assert old_timing["spans"]["replay_readiness"]["calls"] == 6
    assert new_timing["spans"]["replay_readiness"]["calls"] == 4
    assert old_timing["spans"]["source_closure_hash"]["calls"] == 6 * 3 + 1
    assert new_timing["spans"]["source_closure_hash"]["calls"] == 4 * 3 + 1
    assert new_timing["spans"]["local_closure_cache_prefilter"]["calls"] == 2
    assert old_timing["spans"]["semantic_cache_key"]["calls"] == new_timing["spans"]["semantic_cache_key"]["calls"] == 4


def test_candidate_created_after_negative_is_not_retained_as_fact(server, monkeypatch):
    req = request()
    stub_dispatch(monkeypatch, server)
    prefilter = server._local_closure_axioms_cache_definitely_misses
    hints = []
    def appear(req):
        hint = prefilter(req)
        hints.append(hint)
        if hint:
            candidate(server, req)
        return hint
    monkeypatch.setattr(server, "_local_closure_axioms_cache_definitely_misses", appear)
    response, timing = measure(lambda: server._handle_request(req))
    assert response["status"] == "ok"
    assert hints == [True, False]
    # Shared admission and then the original validator both check readiness.
    assert timing["spans"]["replay_readiness"]["calls"] == 2


def test_false_negative_reaches_original_handler_guard(server, monkeypatch):
    req = request()
    candidate(server, req)
    stub_dispatch(monkeypatch, server)
    monkeypatch.setattr(server, "_local_closure_axioms_cache_definitely_misses", lambda req: True)
    response, timing = measure(lambda: server._handle_request(req))
    assert response["status"] == "ok"
    assert timing["spans"]["replay_readiness"]["calls"] == 3
    assert timing["spans"]["semantic_cache_key"]["calls"] == 1


def test_replay_failure_still_fails_closed_on_miss(server, monkeypatch):
    stub_dispatch(monkeypatch, server)
    (server.supervisor_repo / "Tablet/Dep.lean").unlink()
    monkeypatch.setattr(obs, "materialize_tablet_oleans", lambda *a, **kw: {
        "returncode": 1, "stderr": "replay failed", "timed_out": False})
    result = server._handle_request(request())
    assert result["status"] == "internal_error"
    assert result["returncode"] == 1
    assert not server.local_closure_axioms_cache_dir.exists()


def test_sync_failure_precedes_any_hint(server, monkeypatch):
    def fail_sync(*args):
        raise SyncError("injected sync failure")
    def forbidden(*args):
        pytest.fail("prefilter reached after failed sync")
    monkeypatch.setattr(mod, "sync_tablet_dir", fail_sync)
    monkeypatch.setattr(server, "_local_closure_axioms_cache_definitely_misses", forbidden)
    with pytest.raises(SyncError, match="injected sync failure"):
        server._handle_request(request())


def test_source_mutation_during_live_probe_cannot_publish_evidence(server, monkeypatch):
    calls = stub_dispatch(monkeypatch, server)
    probe = obs._run_lake_command
    def mutate(*args, **kwargs):
        result = probe(*args, **kwargs)
        (server.supervisor_repo / "Tablet/Dep.lean").write_text("-- changed during probe\n")
        return result
    monkeypatch.setattr(obs, "_run_lake_command", mutate)
    result = server._handle_request(request())
    assert len(calls) == 1
    assert result["status"] == "manifest_gap"
    assert result["artifact_bundle"] == []
    assert not server.local_closure_axioms_cache_dir.exists()


def test_forged_advisory_key_is_not_reused(server, monkeypatch):
    req = request()
    candidate(server, req)
    compute = mod.compute_semantic_payload_cache_key
    load = mod.load_local_closure_axioms
    keys = []
    def compute_key(*args, **kwargs):
        return "forged-advisory-key" if not keys else compute(*args, **kwargs)
    def load_candidate(cache_dir, cache_key, **kwargs):
        keys.append(cache_key)
        if len(keys) == 1:
            return {"response": {"status": "forged-advisory-response"}}
        return load(cache_dir, cache_key, **kwargs)
    monkeypatch.setattr(mod, "compute_semantic_payload_cache_key", compute_key)
    monkeypatch.setattr(mod, "load_local_closure_axioms", load_candidate)
    result = server._try_local_closure_axioms_cache_hit(req)
    assert result[0]["status"] == "ok"
    assert len(keys) == 2 and keys[0] != keys[1]
