"""Exercise installation discovery without relying on the host's Elan install."""

import hashlib
import json
import subprocess
import sys

import pytest

from trellis.checker.server import CheckerServer
from trellis.host_runtime import resolve_elan_executable


def executable(path, content=b"binary"):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(content)
    path.chmod(0o755)
    return path


@pytest.fixture
def installation(tmp_path, monkeypatch):
    home = tmp_path / "toolchain storage"
    repo = tmp_path / "repo"
    repo.mkdir()
    server = CheckerServer.__new__(CheckerServer)
    server.supervisor_repo = repo
    server._toolchain_path = repo / "lean-toolchain"
    server._toolchain_path.write_text("test-toolchain\n")
    server._lake_manifest_path = repo / "lake-manifest.json"
    server._local_closure_script_path = repo / "collector.lean"
    server._local_closure_script_path.write_text("-- collector\n")
    (repo / "Tablet").mkdir()
    (repo / "Tablet/Preamble.lean").write_text("-- preamble\n")
    native = home / "toolchains/test-toolchain/bin"
    for name in ("lean", "lake"):
        executable(native / name, name.encode())
    log = tmp_path / "resolver-calls.jsonl"
    script = (
        f"#!{sys.executable}\n"
        "import json, os, pathlib, sys\n"
        "assert sys.argv[1] == 'which'\n"
        f"with open({str(log)!r}, 'a') as log:\n"
        " log.write(json.dumps([os.environ['ELAN_HOME'], os.getcwd(), sys.argv[2]]) + '\\n')\n"
        "print(pathlib.Path(os.environ['ELAN_HOME']) / 'toolchains/test-toolchain/bin' / sys.argv[2])\n"
    ).encode()
    monkeypatch.setenv("ELAN_HOME", str(home))
    monkeypatch.setenv("PATH", "")
    return server, home, native, log, script


@pytest.mark.parametrize("layout", ["system", "user", "custom", "symlink"])
def test_execution_identity_uses_actual_installation(installation, tmp_path, monkeypatch, layout):
    server, home, native, log, script = installation
    directory = home / "bin" if layout == "user" else tmp_path / layout / "bin"
    if layout == "symlink":
        target = executable(tmp_path / "real/elan", script)
        directory.mkdir(parents=True)
        (directory / "elan").symlink_to(target)
    else:
        executable(directory / "elan", script)
    if layout != "user":
        monkeypatch.setenv("PATH", str(directory))
        assert not (home / "bin/elan").exists()
    identity, lean, lake = server._closure_execution_identity()
    assert (lean, lake) == (native / "lean", native / "lake")
    assert identity['lean_executable_hash'] == hashlib.sha256(b"lean").hexdigest()
    assert identity['lake_executable_hash'] == hashlib.sha256(b"lake").hexdigest()
    assert [json.loads(line) for line in log.read_text().splitlines()] == [
        [str(home), str(server.supervisor_repo), name] for name in ("lean", "lake")]


def test_path_precedence_and_home_fallback(tmp_path):
    home = tmp_path / "home"
    fallback = executable(home / "bin/elan")
    selected = executable(tmp_path / "system/elan")
    env = {"PATH": str(selected.parent)}
    assert resolve_elan_executable(home, env) == selected
    selected.chmod(0o644)
    assert resolve_elan_executable(home, env) == fallback


def test_relative_and_empty_path_entries_never_select_workspace_binary(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    executable(tmp_path / "elan")
    executable(tmp_path / "bin/elan")
    with pytest.raises(RuntimeError, match="ELAN_HOME selects toolchain storage"):
        resolve_elan_executable(tmp_path / "absent", {"PATH": ":.:bin:"})


def test_broken_path_symlink_uses_home_fallback(tmp_path):
    directory = tmp_path / "system"
    directory.mkdir()
    (directory / "elan").symlink_to(tmp_path / "missing")
    fallback = executable(tmp_path / "home/bin/elan")
    assert resolve_elan_executable(tmp_path / "home", {"PATH": str(directory)}) == fallback


@pytest.mark.parametrize("output", ["", "relative/lean", "/missing/lean", "multiline", "nonexec", "directory"])
def test_invalid_resolver_output_is_rejected(installation, monkeypatch, output):
    server, home, native, log, script = installation
    executable(home / "bin/elan", script)
    if output == "multiline":
        output = f"{native / 'lean'}\n{native / 'lake'}"
    elif output == "nonexec":
        (native / "lean").chmod(0o644)
        output = str(native / "lean")
    elif output == "directory":
        output = str(native)
    monkeypatch.setattr(subprocess, "run", lambda *a, **kw: subprocess.CompletedProcess(a, 0, output))
    with pytest.raises(RuntimeError, match="IdentityUnavailable: lean executable"):
        server._closure_execution_identity()


@pytest.mark.parametrize("failure", ["exit", "timeout"])
def test_selected_resolver_failure_does_not_fall_back(installation, tmp_path, monkeypatch, failure):
    server, home, native, log, script = installation
    selected = executable(tmp_path / "system/elan", script)
    executable(home / "bin/elan", script)
    monkeypatch.setenv("PATH", str(selected.parent))
    error = subprocess.CalledProcessError(1, [str(selected)]) if failure == "exit" else subprocess.TimeoutExpired([str(selected)], 15)
    calls = []
    def fail(args, **kwargs):
        calls.append(args)
        raise error
    monkeypatch.setattr(subprocess, "run", fail)
    with pytest.raises(type(error)):
        server._closure_execution_identity()
    assert calls == [[str(selected), "which", "lean"]]
