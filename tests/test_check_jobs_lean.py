"""Managed incremental checks against a real, confined Lean server."""
import json
from pathlib import Path
import shutil
import subprocess
import time

import pytest

from trellis.checker.jobs import JobService, register_workspace
from trellis import check_job
from trellis.host_runtime import worker_elan_home


TOOLCHAIN = "leanprover/lean4:v4.30.0-rc2"
pytestmark = pytest.mark.skipif(
    not shutil.which("bwrap") or not (
        worker_elan_home() / "toolchains/leanprover--lean4---v4.30.0-rc2/bin/lean"
    ).is_file(),
    reason="bwrap and the pinned Lean toolchain are required",
)


def test_managed_shim_preserves_readonly_toolchain_prefix_query(tmp_path):
    import os
    scripts = tmp_path / "scripts"
    check_job.provision(scripts)
    lean = worker_elan_home() / "toolchains/leanprover--lean4---v4.30.0-rc2/bin/lean"
    expected = subprocess.check_output([str(lean), "--print-prefix"], text=True)
    env = {**os.environ, "TRELLIS_CHECK_JOBS_REAL_LEAN": str(lean)}
    for command in (["lean", "--print-prefix"], ["lake", "env", "lean", "--print-prefix"]):
        result = subprocess.run([str(scripts / "check-bin" / command[0]), *command[1:]],
                                env=env, text=True, capture_output=True, timeout=10)
        assert result.returncode == 0, result.stderr
        assert result.stdout == expected
        prefix = Path(result.stdout.strip())
        assert (prefix / "bin/lean").read_bytes() == lean.read_bytes()
        assert (prefix / "bin/lake").is_file()


def test_prefix_query_cannot_forward_extra_arguments(monkeypatch):
    monkeypatch.setattr(subprocess, "run", lambda *a, **kw: pytest.fail("unmanaged execution"))
    for command in (["lean", "--print-prefix", "Tablet/N.lean"], ["lake", "--print-prefix"]):
        assert check_job.main(["--shim", *command]) == 2


def make_repo(tmp_path):
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / ".lake/packages").mkdir(parents=True)
    (repo / "lean-toolchain").write_text(TOOLCHAIN + "\n")
    (repo / "lakefile.toml").write_text(
        'name = "managed_incremental_test"\nversion = "0.1.0"\n'
        '[[lean_lib]]\nname = "Tablet"\n'
    )
    (repo / "lake-manifest.json").write_text(json.dumps(dict(
        version="1.2.0", packagesDir=".lake/packages", packages=[],
        name="managed_incremental_test", lakeDir=".lake", fixedToolchain=False,
    )))
    return repo


@pytest.mark.parametrize('project_limits', [False, True])
def test_incremental_limits_match_build_and_source_overrides(tmp_path, monkeypatch, project_limits):
    monkeypatch.setenv('TRELLIS_LEAN_PARALLELISM', '1')
    repo = make_repo(tmp_path)
    if project_limits:
        config = repo / 'lakefile.toml'
        config.write_text(config.read_text().replace('[[lean_lib]]',
            '[leanOptions]\nmaxHeartbeats = 1000\nmaxRecDepth = 2000\n\n[[lean_lib]]'))
    heartbeat_limit, recursion_limit = (1000, '2000') if project_limits else (200000, 'Lean.defaultMaxRecDepth')
    # Charge a deterministic heartbeat count without an expensive proof. The
    # same captured source must fail both the warm server and the real builder.
    body = (f'run_elab unless Lean.maxRecDepth.get (← Lean.getOptions) == {recursion_limit} do '
            'throwError "unexpected recursion budget"\n'
            'run_elab do\n'
            '  IO.addHeartbeats 200001000\n'
            '  Lean.Core.checkMaxHeartbeats "incremental budget regression"\n')
    node = repo / 'Tablet/Budget.lean'
    source = 'import Lean\n' + body
    node.write_text(source)
    runtime = tmp_path / 'runtime'
    token = register_workspace(runtime, repo, scope='budget')['TRELLIS_CHECK_JOBS_TOKEN']
    service = JobService(runtime)
    service.start()
    def check(kind, request_id):
        job = service.handle(dict(action='submit', token=token, request_id=request_id,
            kind=kind, target='Budget', timeout=45))
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            result = service.handle(dict(action='status', token=token, job_id=job['job_id']))
            if result['state'] not in {'queued', 'running'}:
                return result
            time.sleep(.05)
        raise AssertionError('managed budget check timed out')
    def fails_at_budget(result):
        assert result['state'] == 'failed', result
        output = result['result']['stdout'] + result['result']['stderr']
        assert f'maximum number of heartbeats ({heartbeat_limit})' in output, output
        assert 'unexpected recursion budget' not in output, output
        assert 'Tablet/Budget.lean:3:' in output, output
    try:
        first = check('incremental', 'bounded')
        fails_at_budget(first)
        assert first['result']['backend'] == 'incremental'
        session = next(iter(service.sessions.values()))
        pid = session.broker._srv.proc.pid
        # A file's own preamble remains effective, including on warm edits.
        node.write_text('import Lean\nset_option maxHeartbeats 0\n' + body)
        repaired = check('incremental', 'explicit-source-budget')
        assert repaired['state'] == 'succeeded', repaired
        assert session.broker._srv.proc.pid == pid
        node.write_text(source)
        fails_at_budget(check('incremental', 'restore-budget'))
        assert session.broker._srv.proc.pid == pid
        fails_at_budget(check('build', 'authoritative-shaped-build'))
        node.write_text('import Lean\nset_option maxHeartbeats 0\n' + body)
        assert check('build', 'build-with-source-budget')['state'] == 'succeeded'
    finally:
        service.shutdown()


def test_switching_incremental_target_builds_its_unmaterialized_imports(tmp_path, monkeypatch):
    monkeypatch.setenv("TRELLIS_LEAN_PARALLELISM", "1")
    repo = make_repo(tmp_path)
    (repo / "Tablet/Helper.lean").write_text("def helper : Nat := 1\n")
    parent = repo / "Tablet/Parent.lean"
    parent.write_text("import Tablet.Helper\nexample : helper = 1 := by rfl\n")
    runtime = tmp_path / "runtime"
    token = register_workspace(runtime, repo, scope="test")["TRELLIS_CHECK_JOBS_TOKEN"]
    service = JobService(runtime)
    service.start()

    def check(node, request_id):
        job = service.handle(dict(action="submit", token=token, request_id=request_id,
                                  kind="incremental", target=node, timeout=45))
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            result = service.handle(dict(action="status", token=token, job_id=job["job_id"]))
            if result["state"] not in {"queued", "running"}:
                return result
            time.sleep(.05)
        raise AssertionError("managed check did not finish")

    try:
        first = check("Helper", "helper")
        assert first["state"] == "succeeded", first
        assert first["result"]["backend"] == "incremental"
        # LSP checking did not produce an olean. Opening Parent in the warm
        # session must first materialize Helper through the managed builder.
        assert not list(service.root.glob("workspaces/*/repo/.lake/build/lib/lean/Tablet/Helper.olean"))
        second = check("Parent", "parent")
        assert second["state"] == "succeeded", second
        assert second["result"]["backend"] == "incremental"
        assert list(service.root.glob("workspaces/*/repo/.lake/build/lib/lean/Tablet/Helper.olean"))
        session = next(iter(service.sessions.values()))
        pid = session.broker._srv.proc.pid
        # Body edits still reuse the server and return actual Lean errors.
        parent.write_text("import Tablet.Helper\nexample : helper = 2 := by rfl\n")
        invalid = check("Parent", "invalid")
        assert invalid["state"] == "failed", invalid
        assert invalid["result"]["backend"] == "incremental"
        assert "rfl" in invalid["result"]["stdout"]
        assert session.broker._srv.proc.pid == pid
        parent.write_text("import Tablet.Helper\nexample : helper = 1 := by decide\n")
        repaired = check("Parent", "repaired")
        assert repaired["state"] == "succeeded", repaired
        assert session.broker._srv.proc.pid == pid
    finally:
        service.shutdown()


@pytest.mark.parametrize('kind,path', [('scratch', '.trellis/scratch/Probe.lean'),
                                     ('file', 'Tablet/Probe.lean')])
def test_warm_file_diagnostics_and_content_invalidation(tmp_path, monkeypatch, kind, path):
    monkeypatch.setenv('TRELLIS_LEAN_PARALLELISM', '1')
    repo = make_repo(tmp_path)
    (repo/'lakefile.toml').unlink()
    (repo/'lakefile.lean').write_text('import Lake\nopen Lake DSL\n'
        'package managed_incremental_test where\n  leanOptions := #[⟨`autoImplicit, false⟩]\n'
        'lean_lib Tablet\n')
    helper = repo/'Tablet/Helper.lean'
    helper.write_text('def helper : Nat := 1\n')
    probe = repo/path
    probe.parent.mkdir(parents=True, exist_ok=True)
    # The existing `lake env lean FILE` leaves autoImplicit at its default.
    # Editor setup would incorrectly impose the package's false option.
    source = ('import Tablet.Helper\n#check helper\n#eval 1 + 2\n'
              'example : helper = 1 := by rfl\nexample : True := by sorry\n'
              'def identityTest (a : α) := a\n#check identityTest\n')
    probe.write_text(source)
    runtime = tmp_path/'runtime'
    token = register_workspace(runtime, repo, scope='files')['TRELLIS_CHECK_JOBS_TOKEN']
    service = JobService(runtime)
    service.start()
    count = 0
    def check():
        nonlocal count
        count += 1
        job = service.handle(dict(action='submit', token=token, request_id=str(count),
                                  kind=kind, target=path, timeout=45))
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            result = service.handle(dict(action='status', token=token, job_id=job['job_id']))
            if result['state'] not in {'queued', 'running'}:
                return result
            time.sleep(.05)
        raise AssertionError('managed file check timed out')
    try:
        first = check()
        assert first['state'] == 'succeeded', first
        assert first['result']['backend'] == 'incremental', first
        assert 'helper : Nat' in first['result']['stdout']
        assert 'identityTest' in first['result']['stdout']
        assert ': info: 3' in first['result']['stdout']
        assert 'warning:' in first['result']['stdout'] and 'sorry' in first['result']['stdout']
        session = next(iter(service.sessions.values()))
        pid = session.broker._srv.proc.pid
        probe.write_text(source.replace('helper = 1', 'helper = 2'))
        bad = check()
        assert bad['state'] == 'failed', bad
        assert path + ':4:' in bad['result']['stdout']
        assert session.broker._srv.proc.pid == pid
        assert bad['result']['phases']['warm_session'] is True
        probe.write_text(source.replace('by rfl', 'by decide'))
        good = check()
        assert good['state'] == 'succeeded', good
        assert session.broker._srv.proc.pid == pid
        # Admission may reclaim the LSP while its immutable dependency
        # artifacts remain valid. A cold server must still reject a bad body
        # without repeating Lake dependency setup.
        owner = session.broker_owner
        with service.scheduler.condition:
            assert service.scheduler.condition.wait_for(
                lambda: not any(r['owner'] == owner for r in service.scheduler.active.values()), timeout=5)
            service.scheduler.evict_idle(owner)
        probe.write_text(source.replace('helper = 1', 'helper = 3'))
        evicted = check()
        assert evicted['state'] == 'failed', evicted
        assert evicted['result']['phases']['warm_session'] is False
        assert evicted['result']['phases']['dependencies_reused'] is True
        assert 'dependencies_ms' not in evicted['result']['phases']
        pid = session.broker._srv.proc.pid
        probe.write_text(source.replace('by rfl', 'by decide'))
        # Same-length source edit with preserved timestamps must invalidate
        # the resident process, rebuild the dependency, and reject this proof.
        import os
        old_stat = helper.stat()
        helper.write_text('def helper : Nat := 2\n')
        os.utime(helper, ns=(old_stat.st_atime_ns, old_stat.st_mtime_ns))
        stale = check()
        assert stale['state'] == 'failed', stale
        assert session.broker._srv.proc.pid != pid
        assert stale['result']['phases']['warm_session'] is False
        assert stale['result']['phases']['dependencies_reused'] is False
        # Header changes materialize newly imported modules before opening.
        (repo/'Tablet/Other.lean').write_text('def other : Nat := 9\n')
        probe.write_text('import Tablet.Other\n#check other\nexample : other = 9 := by rfl\n')
        switched = check()
        assert switched['state'] == 'succeeded', switched
        assert 'other : Nat' in switched['result']['stdout']
        # Unknown diagnostics acknowledgement must use the same admitted
        # compiler fallback, preserving the current source's real failure.
        monkeypatch.setattr(session.broker, 'handle_file', lambda _: dict(verdict='fallback', reason='test unavailable'))
        probe.write_text('import Tablet.Other\nexample : other = 8 := by rfl\n')
        fallback = check()
        assert fallback['state'] == 'failed', fallback
        assert fallback['result']['backend'] == 'lake'
        assert 'using managed Lean file check' in fallback['result']['stderr']
    finally:
        service.shutdown()


@pytest.mark.parametrize('kind,path', [('scratch', '.trellis/scratch/Large.lean'),
                                     ('file', 'Tablet/Large.lean')])
def test_large_file_checks_build_and_refresh_imports(tmp_path, monkeypatch, kind, path):
    monkeypatch.setenv('TRELLIS_LEAN_PARALLELISM', '1')
    repo = make_repo(tmp_path)
    helper = repo / 'Tablet/Helper.lean'
    helper.write_text('def helper : Nat := 1\n')
    probe = repo / path
    probe.parent.mkdir(parents=True, exist_ok=True)
    # A high heartbeat limit selects the managed CLI path, even for a small
    # file. It must prepare imports just as the incremental backend does.
    probe.write_text('import Tablet.Helper\nset_option maxHeartbeats 3000000\n'
                     'example : helper = 1 := by rfl\n')
    runtime = tmp_path / 'runtime'
    token = register_workspace(runtime, repo, scope='large-files')['TRELLIS_CHECK_JOBS_TOKEN']
    service = JobService(runtime)
    service.start()

    def check(request_id):
        job = service.handle(dict(action='submit', token=token, request_id=request_id,
                                  kind=kind, target=path, timeout=45))
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            result = service.handle(dict(action='status', token=token, job_id=job['job_id']))
            if result['state'] not in {'queued', 'running'}:
                return result
            time.sleep(.05)
        raise AssertionError('managed large file check timed out')

    try:
        first = check('fresh')
        assert first['state'] == 'succeeded', first
        assert first['result']['backend'] == 'lake'
        assert list(service.root.glob('workspaces/*/repo/.lake/build/lib/lean/Tablet/Helper.olean'))
        # Keep the probe identical: a dependency edit must rebuild its olean,
        # reject the now-false proof, and retain its correctly located error.
        import os
        old_stat = helper.stat()
        helper.write_text('def helper : Nat := 2\n')
        os.utime(helper, ns=(old_stat.st_atime_ns, old_stat.st_mtime_ns))
        stale = check('changed-import')
        assert stale['state'] == 'failed', stale
        output = stale['result']['stdout'] + stale['result']['stderr']
        assert path + ':3:' in output and 'rfl' in output, output
        assert check('reconnect')['job_id'] == stale['job_id']
        probe.write_text(probe.read_text().replace('helper = 1', 'helper = 2'))
        assert check('repaired')['state'] == 'succeeded'
    finally:
        service.shutdown()


