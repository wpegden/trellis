"""Offline repair and execution identity tests: fake probes, no prover or socket."""
import hashlib
import json
from pathlib import Path
import runpy
from types import SimpleNamespace

import pytest

from trellis.checker.server import CheckerServer
from trellis.runtime import bridge

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/migrate_local_closure_records"


def wrapper():
    return runpy.run_path(str(SCRIPT))


def test_wrapper_requires_explicit_absolute_cli_before_any_write(tmp_path):
    module = wrapper()
    with pytest.raises(SystemExit):
        module["main"]([str(tmp_path)])
    assert list(tmp_path.iterdir()) == []


def test_plan_never_registers_token_or_creates_lock(tmp_path, monkeypatch, capsys):
    module = wrapper()
    state = tmp_path / "incident.json"
    state.write_text("{}")
    manifest = tmp_path / "identity.json"
    manifest.write_text("{}")
    before = {p: p.read_bytes() for p in tmp_path.iterdir()}
    monkeypatch.setattr(bridge, "_register_burst_token", lambda *a, **kw: pytest.fail("plan registered token"))
    calls = []
    def invoke(cli, request, env):
        calls.append(request)
        return {"status": "closure_identity_plan", "plan": {"direct_stale": ["Preamble"]}}
    module["main"].__globals__["invoke"] = invoke
    assert module["main"]([str(tmp_path), "--runtime-cli", "/bin/true", "--state-file", str(state), "--identity-manifest", str(manifest)]) == 0
    assert len(calls) == 1 and calls[0]["action"] == "plan_closure_identity"
    assert {p: p.read_bytes() for p in tmp_path.iterdir()} == before
    assert "Preamble" in capsys.readouterr().out


@pytest.mark.parametrize("fail", [False, True])
def test_repair_mints_own_token_and_preserves_supervisor_token(tmp_path, fail):
    module = wrapper()
    bridge._register_burst_token(tmp_path, token="existing-supervisor", burst_id="supervisor", kind="supervisor", request_id=0, cycle=0)
    seen = []
    def invoke(cli, request, env):
        registry = bridge._load_burst_tokens_file(bridge._burst_tokens_path(tmp_path))
        token = env["TRELLIS_CHECKER_TOKEN"]
        assert token != "existing-supervisor"
        assert token in registry["tokens"] and "existing-supervisor" in registry["tokens"]
        assert next(e for e in registry["entries"] if e["token"] == token)["kind"] == "closure_identity_repair"
        seen.append(token)
        if fail:
            raise RuntimeError("deliberate failed probe")
        return {"minted": 1}
    module["apply_registered"].__globals__["invoke"] = invoke
    if fail:
        with pytest.raises(RuntimeError):
            module["apply_registered"](tmp_path, Path("/bin/true"), {}, 1)
    else:
        assert module["apply_registered"](tmp_path, Path("/bin/true"), {}, 1) == {"minted": 1}
    registry = bridge._load_burst_tokens_file(bridge._burst_tokens_path(tmp_path))
    assert registry["tokens"] == ["existing-supervisor"]
    assert len(seen) == 1


def test_checker_reports_all_actual_axes_and_pins_collector(tmp_path, monkeypatch):
    from trellis.checker import server as mod
    from trellis import host_runtime
    server = CheckerServer.__new__(CheckerServer)
    server.runtime_root = tmp_path / "runtime"
    server.supervisor_repo = tmp_path / "repo"
    server.supervisor_repo.mkdir()
    (server.supervisor_repo / "Tablet").mkdir()
    server._toolchain_path = server.supervisor_repo / "lean-toolchain"
    server._toolchain_path.write_bytes(b"pin\n")
    server._lake_manifest_path = server.supervisor_repo / "lake-manifest.json"
    server._local_closure_script_path = tmp_path / "collector.lean"
    server._local_closure_script_path.write_bytes(b"-- exact bytes\n")
    (server.supervisor_repo / "Tablet/Preamble.lean").write_bytes(b"-- preamble\n")
    paths = {name: tmp_path / name for name in ("lean", "lake")}
    for name, path in paths.items():
        path.write_bytes(name.encode())
    monkeypatch.setattr(host_runtime, "worker_elan_home", lambda: tmp_path)
    monkeypatch.setattr(mod.subprocess, "run", lambda args, **kw: SimpleNamespace(stdout=str(paths[args[-1]])))
    monkeypatch.setattr(mod.observations, "authoritative_env_for_repo", lambda repo: {})
    identity, lean, lake = server._closure_execution_identity()
    assert lean == paths["lean"] and lake == paths["lake"]
    assert len(identity) == 7 and identity["lake_manifest_hash"] == ""
    pinned = server._pin_closure_collector(identity)
    from trellis.sandbox import _repo_writable_paths
    assert pinned.is_relative_to(server.supervisor_repo)
    assert not any(pinned.is_relative_to(path) for path in
                   _repo_writable_paths(server.supervisor_repo, role="lake_compiler", is_isabelle=False))
    assert pinned.read_bytes() == server._local_closure_script_path.read_bytes()
    assert identity["checker_script_hash"] == hashlib.sha256(pinned.read_bytes()).hexdigest()
    server._local_closure_script_path.write_bytes(b"-- changed\n")
    with pytest.raises(RuntimeError):
        server._pin_closure_collector(identity)
    assert pinned.read_bytes() == b"-- exact bytes\n"
    paths["lake"].write_bytes(b"new lake")
    assert server._closure_execution_identity()[0]["lake_executable_hash"] != identity["lake_executable_hash"]
    server._toolchain_path.unlink()
    with pytest.raises(FileNotFoundError):
        server._closure_execution_identity()


def test_registration_failure_never_invokes_kernel(tmp_path, monkeypatch):
    module = wrapper()
    def rejected(*args, **kwargs):
        raise OSError("deliberate token registration failure")
    monkeypatch.setattr(bridge, "_register_burst_token", rejected)
    module["apply_registered"].__globals__["invoke"] = lambda *a: pytest.fail("unregistered repair invoked kernel")
    with pytest.raises(OSError):
        module["apply_registered"](tmp_path, Path("/bin/true"), {}, 1)


def test_concurrent_first_collector_publication_exposes_only_complete_bytes(tmp_path, monkeypatch):
    from concurrent.futures import ThreadPoolExecutor
    import os
    import stat
    import threading
    from trellis.checker import server as mod

    collector = SCRIPT.parent / "lean_local_closure.lean"
    content = collector.read_bytes()
    digest = hashlib.sha256(content).hexdigest()
    repo = tmp_path / "repo"
    final_path = repo / ".trellis/closure-collectors" / f"{digest}.lean"
    publishing = threading.Barrier(8)
    real_fsync = os.fsync

    def fsync_before_publication(fd):
        if stat.S_ISREG(os.fstat(fd).st_mode):
            # Force all first callers to overlap while preparing their files.
            # No caller may expose the final name before its bytes are durable.
            assert not final_path.exists()
            publishing.wait(timeout=10)
        return real_fsync(fd)

    monkeypatch.setattr(mod.os, "fsync", fsync_before_publication)

    def pin(_):
        server = CheckerServer.__new__(CheckerServer)
        server.supervisor_repo = repo
        server._local_closure_script_path = collector
        path = server._pin_closure_collector({"checker_script_hash": digest})
        assert path.read_bytes() == content
        assert path.stat().st_mode & 0o777 == 0o444
        return path

    with ThreadPoolExecutor(max_workers=8) as pool:
        assert list(pool.map(pin, range(8))) == [final_path] * 8
    assert list(final_path.parent.iterdir()) == [final_path]
    assert pin(None) == final_path  # warm use performs no republishing
