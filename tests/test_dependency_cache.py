"""Only the host-prepared cache executable may install dependency artifacts."""
from pathlib import Path
import json
import os
import shutil
import subprocess

import pytest

from trellis.atomic_actions import observations
from trellis.config import SandboxConfig
from trellis.sandbox import wrap_command
from trellis.supervisor_workspace import dependency_cache_invocation, dependency_cache_install_needed


def seed(tmp_path):
    repo = tmp_path / "repo"
    package = repo / ".lake/packages/mathlib"
    binary = package / ".lake/build/bin/cache"
    binary.parent.mkdir(parents=True)
    binary.write_text("#!/bin/sh\nexit 0\n")
    binary.chmod(0o755)
    manifest = dict(packagesDir=".lake/packages", packages=[
        dict(type="git", name="mathlib", rev="a" * 40, subDir=None),
        dict(type="path", name="candidate", dir=str(repo / "Tablet")),
    ])
    (repo / "lake-manifest.json").write_text(json.dumps(manifest))
    for root in (repo, package):
        (root / "lean-toolchain").write_text("leanprover/lean4:v4.30.0-rc2\n")
    (repo / "Tablet").mkdir()
    (repo / "lakefile.lean").write_text("-- This file must never be evaluated.\n")
    return repo, package, binary, tmp_path / "host-cache"


def require_bwrap():
    if not shutil.which("bwrap"):
        pytest.skip("requires bubblewrap")
    result = subprocess.run(["bwrap", "--ro-bind", "/", "/", "/bin/true"],
                            capture_output=True, text=True)
    if result.returncode:
        pytest.skip("requires bubblewrap namespaces")


def test_dependency_cache_path_excludes_candidate_and_environment(tmp_path, monkeypatch):
    repo, package, executable, cache_root = seed(tmp_path)
    monkeypatch.setenv("LEAN_SRC_PATH", str(repo / "Tablet"))
    monkeypatch.setenv("MATHLIB_CACHE_DIR", str(tmp_path / "untrusted-cache"))
    actual, env = dependency_cache_invocation(repo, cache_root)
    assert actual == executable
    assert env["LEAN_SRC_PATH"] == str(package)
    assert "MATHLIB_CACHE_DIR" not in env
    assert Path(env["HOME"]).is_relative_to(cache_root)
    assert Path(env["XDG_CACHE_HOME"]).is_relative_to(cache_root)


@pytest.mark.parametrize("damage", ["missing_binary", "aliased_binary", "unpinned", "subdir_escape"])
def test_dependency_cache_rejects_unprepared_or_unpinned_input(tmp_path, damage):
    repo, package, executable, cache_root = seed(tmp_path)
    manifest = json.loads((repo / "lake-manifest.json").read_text())
    if damage == "missing_binary":
        executable.unlink()
    elif damage == "aliased_binary":
        target = tmp_path / "candidate-cache"
        executable.rename(target)
        executable.symlink_to(target)
    elif damage == "unpinned":
        manifest["packages"][0]["rev"] = "main"
    else:
        manifest["packages"][0]["subDir"] = "../../Tablet"
    (repo / "lake-manifest.json").write_text(json.dumps(manifest))
    with pytest.raises(ValueError):
        dependency_cache_invocation(repo, cache_root)


def test_dependency_cache_role_rejects_arbitrary_command(tmp_path, monkeypatch):
    repo, _, executable, cache_root = seed(tmp_path)
    _, env = dependency_cache_invocation(repo, cache_root)
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    with pytest.raises(ValueError, match="fixed host-prepared"):
        wrap_command(["lake", "build", "Tablet"], sandbox=SandboxConfig(),
                     work_dir=repo, burst_home=Path(env["HOME"]),
                     role="lake_dependency_cache", environment=env)


def test_dependency_cache_confined_cold_install_and_clean_environment(tmp_path, monkeypatch):
    require_bwrap()
    repo, package, executable, cache_root = seed(tmp_path)
    artifact = package / ".lake/build/lib/lean/Mathlib.olean"
    executable.write_text("""#!/usr/bin/python3
import errno, json, os, pathlib, sys
cwd = pathlib.Path.cwd()
assert not (cwd / 'Tablet').exists() and not (cwd / 'lakefile.lean').exists()
assert not os.environ.get('MATHLIB_CACHE_DIR')
assert not os.environ.get('LD_PRELOAD')
assert not os.environ.get('GIT_CONFIG_GLOBAL')
roots = os.environ['LEAN_SRC_PATH'].split(os.pathsep)
assert len(roots) == 1 and roots[0].endswith('/.lake/packages/mathlib')
artifact = pathlib.Path(roots[0]) / '.lake/build/lib/lean/Mathlib.olean'
try:
    artifact.parent.mkdir(parents=True, exist_ok=True)
    artifact.write_bytes(b'host-installed')
except OSError as error:
    if error.errno != errno.EROFS:
        raise
    print('Decompressing 1 file(s)')
    print(os.environ['XDG_CACHE_HOME'] + '/mathlib/0123456789abcdef.ltar: Read-only file system (os error 30)', file=sys.stderr)
    print('uncaught exception: leantar failed with error code 1', file=sys.stderr)
    sys.exit(1)
print(json.dumps({'uid': os.geteuid(), 'installed': str(artifact)}))
""")
    monkeypatch.setenv("MATHLIB_CACHE_DIR", str(repo / "Tablet/fake-cache"))
    monkeypatch.setenv("GIT_CONFIG_GLOBAL", str(repo / "Tablet/fake-git-config"))
    monkeypatch.setenv("LD_PRELOAD", str(repo / "Tablet/fake-loader.so"))
    result = observations.prepare_compiled_support(repo, bwrap_role="lake_compiler",
                                                   dependency_cache_root=cache_root)
    assert result["returncode"] == 0, result
    assert artifact.read_bytes() == b"host-installed"
    assert f'"uid": {os.geteuid()}' in result["stdout"]
    home = tmp_path / "compiler-home"
    home.mkdir()
    cmd = wrap_command(["/bin/sh", "-c", '! echo forged > "$1"', "probe", str(artifact)],
                       sandbox=SandboxConfig(), work_dir=repo, burst_home=home, role="lake_compiler")
    checked = subprocess.run(cmd, capture_output=True, text=True, timeout=10)
    assert checked.returncode == 0, checked.stderr
    assert artifact.read_bytes() == b"host-installed"


def test_real_mathlib_cache_cold_and_warm_from_local_archive(tmp_path, monkeypatch):
    """Opt-in: real cache/leantar, one tiny locally packed module, no download."""
    source = os.environ.get("TRELLIS_TEST_MATHLIB_CACHE")
    if not source:
        pytest.skip("set TRELLIS_TEST_MATHLIB_CACHE to a prebuilt trusted cache executable")
    require_bwrap()
    repo, package, executable, cache_root = seed(tmp_path)
    shutil.copyfile(source, executable)
    executable.chmod(0o755)
    (package / "Mathlib.lean").write_text("def cachedValue : Nat := 7\n")
    (package / "lakefile.lean").write_text("-- trusted tiny cache fixture\n")
    (package / "lake-manifest.json").write_text('{"version":"1.2.0","packages":[]}')
    (repo / "lake-manifest.json").write_text(json.dumps(dict(version="1.2.0", packagesDir=".lake/packages",
        packages=[dict(type="git", name="mathlib", scope="", rev="a" * 40, url="fixture", inputRev="fixture",
                       inherited=False, configFile="lakefile.lean", manifestFile="lake-manifest.json", subDir=None)])))
    _, env = dependency_cache_invocation(repo, cache_root)
    build = package / ".lake/build/lib/lean"
    build.mkdir(parents=True)
    ir = package / ".lake/build/ir"
    ir.mkdir()
    compiled = subprocess.run([str(Path(env["ELAN_HOME"]) / "bin/lean"),
                               "-o", str(build / "Mathlib.olean"), "-i", str(build / "Mathlib.ilean"),
                               "-c", str(ir / "Mathlib.c"), "Mathlib.lean"],
                              cwd=package, env=env, capture_output=True, text=True, timeout=30)
    assert compiled.returncode == 0, compiled.stderr
    (build / "Mathlib.trace").write_text('{"depHash":"0000000000000001"}')
    for root, suffix in ((build, "olean"), (build, "ilean"), (ir, "c")):
        (root / f"Mathlib.{suffix}.hash").write_text("42")
    packed = subprocess.run([str(executable), "pack"], cwd=package, env=env,
                            capture_output=True, text=True, timeout=30)
    assert packed.returncode == 0, packed.stderr
    assert len(list((cache_root / "cache/mathlib").glob("*.ltar"))) == 1, packed.stdout
    expected = (build / "Mathlib.olean").read_bytes()
    (build / "Mathlib.olean").unlink()
    (build / "Mathlib.trace").unlink()
    from concurrent.futures import ThreadPoolExecutor
    from trellis.checker import admission
    import threading
    scheduler = admission.Admission(2)
    monkeypatch.setattr(admission, "ACTIVE", scheduler)
    modes = []
    original = observations._Wait4Popen
    class Process(original):
        def __init__(self, *args, **kwargs):
            modes.append((scheduler.active[threading.get_ident()]["exclusive"], len(scheduler.active)))
            super().__init__(*args, **kwargs)
    monkeypatch.setattr(observations, "_Wait4Popen", Process)
    def prepare():
        return observations.prepare_compiled_support(repo, bwrap_role="lake_compiler",
                                                     dependency_cache_root=cache_root, timeout_secs=30)
    for index in range(2):
        prior_mtime = (build / "Mathlib.olean").stat().st_mtime_ns if index else None
        if index == 0:
            result = prepare()
        else:
            # A warm probe finishes while a different long-running compiler
            # retains its lease; it must never request a global drain.
            with ThreadPoolExecutor(1) as pool, scheduler.acquire("other-workspace"):
                result = pool.submit(prepare).result(timeout=5)
        assert result["returncode"] == 0, result
        assert (build / "Mathlib.olean").read_bytes() == expected
        if index == 0:
            assert "Decompress" in result["stdout"]
        else:
            assert (build / "Mathlib.olean").stat().st_mtime_ns == prior_mtime
    assert modes == [(False, 1), (True, 1), (False, 2)]


@pytest.mark.parametrize("failure", ["network", "spawn", "timeout", "foreign_archive", "foreign_target", "no_decompression"])
def test_dependency_cache_never_installs_for_unrelated_failures(tmp_path, failure):
    repo, _, _, cache_root = seed(tmp_path)
    error = str(cache_root / "cache/mathlib/0123456789abcdef.ltar") + ": Read-only file system (os error 30)"
    result = dict(returncode=1, stdout="Decompressing 1 file(s)",
                  stderr=error + "\nuncaught exception: leantar failed with error code 1\n")
    assert dependency_cache_install_needed(result, cache_root, repo)
    if failure == "network":
        result["stderr"] = "curl: download failed"
    elif failure == "spawn":
        result["spawn_error"] = "missing executable"
    elif failure == "timeout":
        result["timed_out"] = True
    elif failure == "foreign_archive":
        result["stderr"] = result["stderr"].replace(str(cache_root), str(repo / "Tablet"))
    elif failure == "foreign_target":
        result["stderr"] = result["stderr"].replace("(os error 30)", '(os error 30) at path "' + str(repo / "Tablet/A.lean") + '"')
    else:
        result["stdout"] = "configuration failed"
    assert not dependency_cache_install_needed(result, cache_root, repo)


def test_relocated_prewarmed_package_supports_lake_env_and_build(tmp_path):
    """A real Lean lakefile/config cache survives private-workspace relocation."""
    from trellis.host_runtime import worker_elan_home
    from trellis.supervisor_workspace import _ensure_supervisor_lake_packages, _sync_repo_tree
    require_bwrap()
    lake = worker_elan_home() / "bin/lake"
    if not lake.exists():
        pytest.skip("requires installed Lean")
    worker = tmp_path / "worker"
    dep = worker / ".lake/packages/dep"
    dep.mkdir(parents=True)
    (worker / "lean-toolchain").write_text("leanprover/lean4:v4.30.0-rc2\n")
    (worker / "lakefile.toml").write_text('''name = "fixture"
[[require]]
name = "dep"
path = ".lake/packages/dep"
[[lean_lib]]
name = "Tablet"
''')
    (dep / "lakefile.lean").write_text("import Lake\nopen Lake DSL\npackage dep\nlean_lib Dep\n")
    (dep / "Dep.lean").write_text("def dependencyValue : Nat := 7\n")
    (worker / "Tablet").mkdir()
    (worker / "Tablet/A.lean").write_text("import Dep\nexample : dependencyValue = 7 := rfl\n")
    # Trusted setup fixture only: no candidate code exists during prewarm.
    env = {**os.environ, "ELAN_HOME": str(worker_elan_home()),
           "PATH": str(worker_elan_home() / "bin") + os.pathsep + "/usr/bin:/bin"}
    for args in (["update"], ["build", "Tablet.A"]):
        result = subprocess.run([str(lake), *args], cwd=worker, env=env,
                                capture_output=True, text=True, timeout=40)
        assert result.returncode == 0, result.stderr
    private = tmp_path / "private/repo"
    private.mkdir(parents=True)
    _sync_repo_tree(worker, private)
    _ensure_supervisor_lake_packages(worker, private)
    assert (private / ".lake/packages/dep/.lake/build/lib/lean/Dep.olean").samefile(
        dep / ".lake/build/lib/lean/Dep.olean")
    for args in (["env", "lean", "Tablet/A.lean"], ["build", "Tablet.A"]):
        result = observations._run_lake_command(private, args, bwrap_role="lake_compiler", timeout_secs=40)
        assert result["returncode"] == 0, result
