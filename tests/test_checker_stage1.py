"""Stage-1 equivalence, fresh-byte validation and protected probe publication."""
from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import threading

import pytest

from trellis.atomic_actions import observations as obs
from trellis.checker.compiled_probe import CompiledClosureProbe, CompiledSemanticProbe, lake_probe_command


def repo_fixture(tmp_path):
    (tmp_path / "Tablet").mkdir()
    (tmp_path / ".trellis/scripts").mkdir(parents=True)
    (tmp_path / ".trellis/scripts/check.py").write_bytes(b"check\n")
    (tmp_path / "lean-toolchain").write_bytes(b"pinned\n")
    for name, content in {
        "Preamble": b"import Lean\n",
        "A": b"import Tablet.B Tablet.C\r\ntheorem A : True := trivial\r\n",
        "B": b"import Tablet.Preamble Tablet.D\n-- \xff\n",
        "C": b"/- import Tablet.Missing -/\nimport Tablet.D\n",
        "D": b"import Tablet.Preamble\n",
    }.items():
        (tmp_path / f"Tablet/{name}.lean").write_bytes(content)
    return tmp_path


@pytest.mark.parametrize("change", ["none", "cycle", "missing_dep", "missing_script", "missing_preamble", "lake"])
def test_snapshot_keys_and_order_match_original(tmp_path, change):
    repo = repo_fixture(tmp_path)
    if change == "cycle":
        (repo / "Tablet/D.lean").write_text("import Tablet.A\n")
    elif change.startswith("missing_"):
        path = {"missing_dep": "Tablet/D.lean", "missing_script": ".trellis/scripts/check.py",
                "missing_preamble": "Tablet/Preamble.lean"}[change]
        (repo / path).unlink()
    elif change == "lake":
        (repo / "lakefile.toml").write_text('name = "different"\n')
    snapshot = obs.SourceClosureSnapshot(repo)
    nodes = ["A", "B", "C", "D", "Preamble", "Absent"]
    assert snapshot.materialization_order(nodes) == obs.materialization_order(repo, nodes)
    for node in nodes:
        assert snapshot.closure_hash(node) == obs.tablet_source_closure_hash(repo, node)
    assert snapshot.is_current()


def test_snapshot_reads_each_input_once_then_validates_bytes(tmp_path, monkeypatch):
    repo = repo_fixture(tmp_path)
    counts = {}
    original = Path.read_bytes
    def read(path):
        counts[path] = counts.get(path, 0) + 1
        return original(path)
    monkeypatch.setattr(Path, "read_bytes", read)
    snapshot = obs.SourceClosureSnapshot(repo)
    for node in snapshot.materialization_order(["A"]):
        assert snapshot.closure_hash(node)
        assert snapshot.closure_hash(node)
    assert set(counts.values()) == {1}
    assert snapshot.is_current()
    assert set(counts.values()) == {2}


@pytest.mark.parametrize("mutation", ["same_mtime", "delete", "new_optional"])
def test_source_drift_rejects_snapshot_and_next_pass_is_fresh(tmp_path, mutation):
    repo = repo_fixture(tmp_path)
    snapshot = obs.SourceClosureSnapshot(repo)
    before = snapshot.closure_hash("A")
    dep = repo / "Tablet/D.lean"
    if mutation == "same_mtime":
        stat = dep.stat()
        content = dep.read_bytes()
        dep.write_bytes(content.replace(b"Preamble", b"MissingX"))
        os.utime(dep, ns=(stat.st_atime_ns, stat.st_mtime_ns))
    elif mutation == "delete":
        dep.unlink()
    else:
        (repo / "lakefile.lean").write_bytes(b"")
    assert not snapshot.is_current()
    fresh = obs.SourceClosureSnapshot(repo)
    assert fresh.closure_hash("A") == obs.tablet_source_closure_hash(repo, "A")
    if mutation != "new_optional":
        assert fresh.closure_hash("A") != before


def test_snapshot_is_not_shared_between_repositories(tmp_path):
    a = tmp_path / "a"; a.mkdir()
    b = tmp_path / "b"; b.mkdir()
    repo_fixture(a); repo_fixture(b)
    (b / "Tablet/D.lean").write_bytes(b"different\n")
    def check(repo):
        return obs.SourceClosureSnapshot(repo).closure_hash("A")
    with ThreadPoolExecutor(2) as pool:
        actual = list(pool.map(check, [a, b]))
    assert actual == [obs.tablet_source_closure_hash(p, "A") for p in [a, b]]
    assert actual[0] != actual[1]


@pytest.mark.parametrize('oversized', [False, True])
def test_materialization_hash_batch_preserves_every_key_and_bounds_memory(tmp_path, monkeypatch, oversized):
    repo = repo_fixture(tmp_path)
    nodes = ['A', 'B', 'C', 'D', 'Preamble', 'Absent']
    expected = {n: obs.tablet_source_closure_hash(repo, n) for n in nodes}
    if oversized:
        monkeypatch.setattr(obs.SourceClosureSnapshot, 'MAX_BYTES', 1)
    assert obs.tablet_source_closure_hashes(repo, nodes) == expected


def test_materialization_hash_batch_parses_shared_sources_once(tmp_path, monkeypatch):
    repo = repo_fixture(tmp_path)
    nodes = []
    for i in range(60):
        name = f'N{i}'
        (repo / f'Tablet/{name}.lean').write_text('import Tablet.' + (nodes[-1] if nodes else 'Preamble') + '\n')
        nodes.append(name)
    expected = {n: obs.tablet_source_closure_hash(repo, n) for n in nodes}
    calls = []
    original = obs._kernel_extract_tablet_imports
    def parsed(content):
        calls.append(content)
        return original(content)
    monkeypatch.setattr(obs, '_kernel_extract_tablet_imports', parsed)
    assert obs.tablet_source_closure_hashes(repo, nodes) == expected
    assert len(calls) == 61


def test_materialization_hash_batch_releases_oversized_inventory_before_fallback(tmp_path, monkeypatch):
    import weakref
    repo = repo_fixture(tmp_path)
    refs = []
    class Oversized(obs.SourceClosureSnapshot):
        MAX_BYTES = 1
        def __init__(self, repo):
            super().__init__(repo)
            refs.append(weakref.ref(self))
    original = obs.tablet_source_closure_hash
    def streaming(repo, node):
        assert refs and all(ref() is None for ref in refs)
        return original(repo, node)
    monkeypatch.setattr(obs, 'SourceClosureSnapshot', Oversized)
    monkeypatch.setattr(obs, 'tablet_source_closure_hash', streaming)
    assert obs.tablet_source_closure_hashes(repo, ['A']) == {'A': original(repo, 'A')}


def test_materialization_hash_batch_rejects_drift_and_does_not_retain_snapshot(tmp_path, monkeypatch):
    repo = repo_fixture(tmp_path)
    original = obs.SourceClosureSnapshot.is_current
    def changed(snapshot):
        dep = repo / 'Tablet/D.lean'
        info = dep.stat()
        dep.write_text('import Tablet.Missing\n')
        os.utime(dep, ns=(info.st_atime_ns, info.st_mtime_ns))
        return original(snapshot)
    monkeypatch.setattr(obs.SourceClosureSnapshot, 'is_current', changed)
    assert obs.tablet_source_closure_hashes(repo, ['A', 'D']) == {'A': None, 'D': None}
    monkeypatch.setattr(obs.SourceClosureSnapshot, 'is_current', original)
    assert obs.tablet_source_closure_hashes(repo, ['A', 'D']) == {
        n: obs.tablet_source_closure_hash(repo, n) for n in ['A', 'D']}


def test_replay_readiness_rejects_source_change_during_attestation(tmp_path, monkeypatch):
    from test_local_closure_cache_prefilter import make_server
    server = make_server(tmp_path)
    assert server._closures_have_current_kernel_replay(["Root"])
    original = obs.olean_has_current_kernel_replay
    def changed(repo, node, **kwargs):
        result = original(repo, node, **kwargs)
        if node == "Root":
            dep = repo / "Tablet/Dep.lean"
            stat = dep.stat()
            dep.write_text(dep.read_text().replace("1", "2"))
            os.utime(dep, ns=(stat.st_atime_ns, stat.st_mtime_ns))
        return result
    monkeypatch.setattr(obs, "olean_has_current_kernel_replay", changed)
    assert not server._closures_have_current_kernel_replay(["Root"])
    assert not server._closures_have_current_kernel_replay(["Root"])


def test_oversize_snapshot_falls_back_to_original_content_checks(tmp_path, monkeypatch):
    from test_local_closure_cache_prefilter import make_server
    server = make_server(tmp_path)
    monkeypatch.setattr(obs.SourceClosureSnapshot, "MAX_BYTES", 1)
    assert server._closures_have_current_kernel_replay(["Root"])
    (server.supervisor_repo / "Tablet/Dep.lean").write_text("-- changed\n")
    assert not server._closures_have_current_kernel_replay(["Root"])


@pytest.fixture(params=[CompiledClosureProbe, CompiledSemanticProbe])
def compiler(tmp_path, monkeypatch, request):
    monkeypatch.setenv("TRELLIS_CLOSURE_PRECOMPILE", "1")
    monkeypatch.setenv("TRELLIS_SEMANTIC_PRECOMPILE", "1")
    source = tmp_path / "collector.lean"
    source.write_text("import Lean\ndef main : IO Unit := pure ()\n")
    lean = tmp_path / "lean"
    lean.write_bytes(b"compiler")
    identity = {"checker_script_hash": hashlib.sha256(source.read_bytes()).hexdigest(),
                "toolchain_hash": "pin", "lean_executable_hash": hashlib.sha256(lean.read_bytes()).hexdigest()}
    calls = []
    def compile(args, **kwargs):
        assert args[0] == str(lean)
        assert "LEAN_PATH" not in kwargs["env"]
        assert "LEAN_SYSROOT" not in kwargs["env"]
        output = args[args.index('-o') + 1]
        Path(output).write_bytes(b"compiled-trusted-collector")
        Path(output + ".server").write_bytes(b"server part")
        Path(output + ".private").write_bytes(b"private part")
        calls.append(args)
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")
    monkeypatch.setattr(subprocess, "run", compile)
    return request.param(tmp_path), source, lean, identity, calls


def prepare(compiler):
    probe, source, lean, identity, _ = compiler
    return probe.prepare(source, identity, lean, timeout_secs=30)


def test_compiles_once_and_reuses_verified_artifacts_across_instances(compiler):
    first = prepare(compiler)
    assert prepare(compiler) == first
    probe, source, lean, identity, calls = compiler
    other = type(probe)(source.parent)
    assert other.prepare(source, identity, lean, timeout_secs=30) == first
    assert len(calls) == 1
    assert first[1] == first[0].parent


def test_concurrent_preparation_publishes_one_complete_bundle(compiler):
    with ThreadPoolExecutor(4) as pool:
        results = list(pool.map(lambda _: prepare(compiler), range(8)))
    assert all(r == results[0] for r in results)
    assert len(compiler[-1]) == 1


@pytest.mark.parametrize("part", ["olean", "runner", "receipt", "extra_private", "symlink", "server", "missing_private"])
def test_modified_compiled_bundle_fails_closed(compiler, part):
    runner, _ = prepare(compiler)
    olean = next(runner.parent.glob("*.olean"))
    if part == "extra_private":
        Path(str(olean) + ".private.extra").write_bytes(b"unattested")
    elif part == "symlink":
        olean.unlink(); olean.symlink_to(compiler[1])
    elif part == "server":
        path = Path(str(olean) + ".server")
        path.chmod(0o644); path.write_bytes(b"corrupt")
    elif part == "missing_private":
        Path(str(olean) + ".private").unlink()
    else:
        path = {"olean": olean, "runner": runner, "receipt": runner.parent / "receipt.json"}[part]
        path.chmod(0o644)
        path.write_bytes(b"{}" if part == "receipt" else b"corrupt")
    with pytest.raises((RuntimeError, ValueError)):
        prepare(compiler)
    assert len(compiler[-1]) == 1  # Never silently bless or overwrite corruption.


@pytest.mark.parametrize("axis", ["source", "toolchain", "lean"])
def test_each_compilation_input_selects_a_new_bundle(compiler, axis):
    before = prepare(compiler)
    _, source, lean, identity, calls = compiler
    if axis == "source":
        source.write_text(source.read_text() + "-- edit\n")
        identity["checker_script_hash"] = hashlib.sha256(source.read_bytes()).hexdigest()
    elif axis == "lean":
        lean.write_bytes(b"different compiler")
        identity["lean_executable_hash"] = hashlib.sha256(lean.read_bytes()).hexdigest()
    else:
        identity["toolchain_hash"] = "different pin"
    assert prepare(compiler)[0] != before[0]
    assert len(calls) == 2


def test_failed_compilation_publishes_nothing(compiler, monkeypatch):
    def fail(args, **kwargs):
        Path(args[args.index('-o') + 1]).write_bytes(b"partial")
        raise subprocess.TimeoutExpired(args, kwargs["timeout"])
    monkeypatch.setattr(subprocess, "run", fail)
    with pytest.raises(subprocess.TimeoutExpired):
        prepare(compiler)
    assert list(compiler[0].root.iterdir()) == []


def test_compiler_changed_during_compilation_publishes_nothing(compiler, monkeypatch):
    from trellis.checker import compiled_probe
    original = subprocess.run
    def changed(*args, **kwargs):
        result = original(*args, **kwargs)
        compiler[2].write_bytes(b"changed compiler")
        return result
    monkeypatch.setattr(compiled_probe, "guarded_run", changed)
    with pytest.raises(RuntimeError, match="Lean executable changed during"):
        prepare(compiler)
    assert list(compiler[0].root.iterdir()) == []


def test_source_execution_switch_preserves_existing_path(compiler, monkeypatch):
    monkeypatch.setenv(compiler[0].switch, "0")
    assert prepare(compiler) == (compiler[1], None)
    assert not compiler[-1]


def test_warm_prepare_does_not_reserve_or_evict_a_compiler(compiler, monkeypatch):
    from trellis.checker import admission, compiled_probe
    scheduler = admission.Admission(1)
    monkeypatch.setattr(admission, "ACTIVE", scheduler)
    fake_compile = subprocess.run
    def compile(*args, **kwargs):
        assert threading.get_ident() in scheduler.active
        return fake_compile(*args, **kwargs)
    monkeypatch.setattr(compiled_probe, "guarded_run", compile)
    expected = prepare(compiler)
    evicted = []
    with scheduler.acquire("worker"):
        scheduler.retain("worker", lambda: evicted.append(True))
    assert prepare(compiler) == expected
    assert not evicted and "worker" in scheduler.residents
    with ThreadPoolExecutor(1) as pool:
        with scheduler.acquire("worker"):
            assert pool.submit(prepare, compiler).result(timeout=2) == expected
    assert len(compiler[-1]) == 1


@pytest.mark.parametrize("warm", [False, True])
def test_cancelled_prepare_never_compiles_or_returns_a_runner(compiler, monkeypatch, warm):
    from trellis.checker import admission
    if warm:
        prepare(compiler)
    def cancelled():
        raise OSError("workspace revoked")
    with admission.authoritative_workspace("cancelled", cancelled=cancelled):
        with pytest.raises(OSError, match="workspace revoked"):
            prepare(compiler)
    assert len(compiler[-1]) == int(warm)


def test_cold_prepare_waits_for_admission_and_cancellation(compiler, monkeypatch):
    from trellis.checker import admission, compiled_probe
    scheduler = admission.Admission(1)
    monkeypatch.setattr(admission, "ACTIVE", scheduler)
    waiting, revoked = threading.Event(), threading.Event()
    def cancelled():
        if revoked.is_set():
            raise OSError("workspace revoked")
        if scheduler.waiters:
            waiting.set()
    monkeypatch.setattr(compiled_probe, "guarded_run", lambda *a, **kw: pytest.fail("unadmitted compiler"))
    def run():
        with admission.authoritative_workspace("queued", cancelled=cancelled):
            return prepare(compiler)
    with ThreadPoolExecutor(1) as pool:
        with scheduler.acquire("worker"):
            result = pool.submit(run)
            assert waiting.wait(2)
            assert not compiler[-1]
            revoked.set()
            with scheduler.condition:
                scheduler.condition.notify_all()
            with pytest.raises(OSError, match="workspace revoked"):
                result.result(timeout=2)
    assert not scheduler.waiters and not scheduler.active
    assert list(compiler[0].root.iterdir()) == []


def test_protected_import_path_precedes_lake_paths_without_shell_interpolation(tmp_path):
    # Simulate the environment AFTER Lake has prepended candidate paths.
    directory = tmp_path / 'protected $(false) space'
    fake_lean = tmp_path / "fake-lean"
    fake_lean.write_text(f'#!{sys.executable}\nimport os,sys,json\n'
                         'print(json.dumps([os.environ["LEAN_PATH"],sys.argv[1:]]))\n')
    fake_lean.chmod(0o755)
    program = directory / "runner.lean"
    args = ["Node", "--principal=Name; exit 1"]
    command = lake_probe_command(fake_lean, program, args, directory)
    env = {**os.environ, "LEAN_PATH": "/candidate:/core", "LEAN_NUM_THREADS": "2"}
    result = subprocess.run(command[1:], env=env,
                            capture_output=True, text=True, check=True)
    assert json.loads(result.stdout) == [str(directory) + ":/candidate:/core",
                                         ["-j2", "--run", str(program), *args]]
    result = subprocess.run(lake_probe_command(fake_lean, program, args, None)[1:],
                            env=env, capture_output=True, text=True, check=True)
    assert json.loads(result.stdout) == ["/candidate:/core", ["-j2", "--run", str(program), *args]]
