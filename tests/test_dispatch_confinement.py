"""Exercise the actual mount/seccomp boundary, not only PATH routing."""
import json
import os
from pathlib import Path
import shutil
import subprocess

import pytest

from trellis.dispatch.confinement import data_view, restrict_bwrap, require_view, ConfinementError, PersistentDataView


def test_plain_directory_is_not_a_confinement_boundary(tmp_path):
    with pytest.raises(ConfinementError, match="mounted noexec"):
        require_view(tmp_path)


def test_agent_cannot_reach_the_host_alias_through_an_ancestor_mount(tmp_path, monkeypatch):
    view = tmp_path/'private-view'
    view.mkdir()
    monkeypatch.setattr('trellis.dispatch.confinement.require_view', lambda _: view)
    with pytest.raises(ConfinementError, match='outside every agent-visible directory'):
        restrict_bwrap(['bwrap', '--ro-bind', str(tmp_path), '/data', '/usr/bin/true'],
            view=view, executable_roots=['/usr'], scratch=tmp_path/'guard', compiler='managed')


def test_persistent_view_survives_coordinator_reopen_and_refuses_live_unmount(tmp_path):
    import sys
    import time
    from trellis.dispatch.process import AttemptProcess
    helper = os.environ.get('TRELLIS_TEST_BINDFS') or shutil.which('bindfs')
    if not helper or not Path('/dev/fuse').exists():
        pytest.skip('real compiler confinement requires bindfs and host FUSE access')
    owner = PersistentDataView(tmp_path/'mount')
    mounted = owner.prepare(bindfs=helper)
    adopted = PersistentDataView(owner.root)
    assert adopted.prepare(bindfs=helper) == mounted
    process = AttemptProcess(tmp_path/'attempt')
    # Simulate an attempt recorded before native noexec became the default.
    from trellis.dispatch.candidate import canonical_json
    command = [sys.executable, '-c', 'import time; time.sleep(60)']
    (process.root/'recipe.json').write_bytes(canonical_json(dict(
        command=command, payload={}, confinement=str(owner.root))))
    record = process.start_or_adopt(command, {}, confinement=owner.root)
    process.permit(record)
    try:
        deadline = time.monotonic()+5
        while not list((owner.root/'users').glob('*.json')) and time.monotonic()<deadline:
            time.sleep(.01)
        assert list((owner.root/'users').glob('*.json'))
        with pytest.raises(ConfinementError, match='still leased'):
            adopted.close()
        recovered = AttemptProcess(process.root)
        recovered.stop_and_reap(record)
        process.child.wait(timeout=5)
        assert not list((owner.root/'users').glob('*.json'))
    finally:
        process.stop_and_reap(record)
        adopted.close()
    assert not os.path.ismount(mounted)


def test_abnormal_view_user_leaves_evidence_and_interrupted_unmount_recovers(tmp_path):
    from trellis.dispatch.candidate import canonical_json
    from trellis.dispatch.publication import atomic
    helper = os.environ.get('TRELLIS_TEST_BINDFS') or shutil.which('bindfs')
    if not helper or not Path('/dev/fuse').exists():
        pytest.skip('real compiler confinement requires bindfs and host FUSE access')
    owner = PersistentDataView(tmp_path/'mount')
    owner.prepare(bindfs=helper)
    try:
        with pytest.raises(RuntimeError):
            with owner.use():
                raise RuntimeError('interrupted before descendant cleanup')
        with pytest.raises(ConfinementError, match='unreconciled users'):
            owner.close()
        # This test itself was the only user and launched no descendants.
        for receipt in (owner.root/'users').glob('*.json'):
            receipt.unlink()
        atomic(owner.root/'closing.json', canonical_json(owner._identity()))
        subprocess.run(['fusermount3', '-u', str(owner.view)], check=True)
        with pytest.raises(ConfinementError, match='closing'):
            PersistentDataView(owner.root).prepare(bindfs=helper)
        PersistentDataView(owner.root).close()
        assert not (owner.root/'mount.json').exists()
    finally:
        owner.close()


@pytest.fixture
def confined(tmp_path):
    helper = os.environ.get("TRELLIS_TEST_BINDFS") or shutil.which("bindfs")
    if not helper or not Path("/dev/fuse").exists():
        pytest.skip("real compiler confinement requires bindfs and host FUSE access")
    repo = tmp_path / "data"
    repo.mkdir()
    shutil.copy('/usr/bin/true', repo / 'copied-native')
    with data_view(tmp_path / "mounts", bindfs=helper) as view:
        def run(code, compiler="none"):
            cmd = ["bwrap", "--unshare-pid", "--unshare-ipc", "--cap-drop", "ALL",
                   "--ro-bind", "/usr", "/usr", "--ro-bind", "/lib", "/lib", "--ro-bind", "/lib64", "/lib64",
                   "--symlink", "usr/bin", "/bin", "--proc", "/proc", "--dev", "/dev",
                   "--bind", str(repo), "/data", "/usr/bin/python3", "-c", code]
            cmd = restrict_bwrap(cmd, view=view, executable_roots=["/usr", "/lib", "/lib64"],
                                 scratch=tmp_path / "guard", compiler=compiler)
            return subprocess.run(cmd, capture_output=True, text=True, timeout=20)
        yield run


def test_real_native_copy_loader_and_executable_memory_denied(confined):
    result = confined('''
import ctypes as C, errno, json, os, subprocess
r = {}
try:
 subprocess.run(['/data/copied-native'], check=True)
 r['copy'] = 'escaped'
except PermissionError: r['copy'] = 'denied'
p = subprocess.run(['/lib64/ld-linux-x86-64.so.2', '/data/copied-native'], capture_output=True, text=True)
r['loader'] = p.returncode
lib = C.CDLL(None, use_errno=True)
lib.mmap.restype = C.c_void_p
r['anonymous_exec_denied'] = lib.mmap(None, 4096, 7, 0x22, -1, 0) == C.c_void_p(-1).value
try: os.memfd_create('copy'); r['memfd'] = 'escaped'
except PermissionError: r['memfd'] = 'denied'
r['proc_ro'] = bool(os.statvfs('/proc').f_flag & os.ST_RDONLY)
r['data_noexec'] = bool(os.statvfs('/data').f_flag & os.ST_NOEXEC)
open('/data/proof.txt','w').write('ordinary editing works')
r['compiler_token'] = os.environ.get('TRELLIS_CHECKER_TOKEN')
print(json.dumps(r))
''')
    assert result.returncode == 0, result.stderr
    r = json.loads(result.stdout)
    assert r['copy'] == r['memfd'] == 'denied'
    assert r['loader'] != 0
    assert r['anonymous_exec_denied'] and r['proc_ro'] and r['data_noexec']
    assert r['compiler_token'] is None


def test_real_trusted_processes_and_managed_client_environment_work(confined):
    result = confined("import subprocess; print(subprocess.check_output(['/usr/bin/git', '--version'], text=True).strip())", "managed")
    assert result.returncode == 0, result.stderr
    assert result.stdout.startswith("git version")


def test_real_confined_agent_uses_managed_lean_but_cannot_run_toolchain(tmp_path, monkeypatch):
    from trellis.checker.jobs import JobService, register_workspace
    from trellis.checker.admission import Admission
    from trellis.host_runtime import worker_elan_home
    from trellis import check_job
    from test_check_jobs_lean import make_repo
    helper = os.environ.get('TRELLIS_TEST_BINDFS') or shutil.which('bindfs')
    toolchain = worker_elan_home() / 'toolchains/leanprover--lean4---v4.30.0-rc2'
    if not helper or not Path('/dev/fuse').exists() or not (toolchain/'bin/lean').is_file():
        pytest.skip('requires host FUSE and the pinned Lean toolchain')
    monkeypatch.setenv('TRELLIS_LEAN_PARALLELISM', '1')
    repo = make_repo(tmp_path)
    (repo/'Tablet/Example.lean').write_text('example : 1 = 1 := by rfl\n')
    shutil.copy(check_job.__file__, repo/'client.py')
    runtime = tmp_path/'runtime'
    env = register_workspace(runtime, repo, scope='confined-worker')
    service = JobService(runtime, scheduler=Admission(1))
    service.start()
    try:
        with data_view(tmp_path/'mounts', bindfs=helper) as view:
            def run(compiler, code):
                cmd = ['bwrap', '--unshare-pid', '--unshare-ipc', '--cap-drop', 'ALL',
                    '--ro-bind', '/usr', '/usr', '--ro-bind', '/lib', '/lib', '--ro-bind', '/lib64', '/lib64',
                    '--symlink', 'usr/bin', '/bin', '--proc', '/proc', '--dev', '/dev',
                    '--bind', str(repo), '/data', '--ro-bind', str(toolchain), '/toolchain',
                    '--ro-bind', str(service.path), '/checker/jobs.sock',
                    '--setenv', 'TRELLIS_CHECK_JOBS_SOCKET', '/checker/jobs.sock',
                    '--setenv', 'TRELLIS_CHECK_JOBS_TOKEN', env['TRELLIS_CHECK_JOBS_TOKEN'],
                    '--chdir', '/data', '/usr/bin/python3', '-c', code]
                cmd = restrict_bwrap(cmd, view=view, executable_roots=['/usr','/lib','/lib64'],
                    scratch=tmp_path/'guard', compiler=compiler)
                if compiler == 'none':
                    assert env['TRELLIS_CHECK_JOBS_TOKEN'] not in cmd, 'saved argv leaked a withheld capability'
                return subprocess.run(cmd, capture_output=True, text=True, timeout=55)
            script = """
import json, subprocess, shutil, io, contextlib
import client
metadata = io.StringIO()
with contextlib.redirect_stdout(metadata):
 assert client.shim('lean', ['--print-prefix']) == 0
assert metadata.getvalue().strip().endswith('leanprover--lean4---v4.30.0-rc2')
for path in ['/toolchain/bin/lean', '/data/copied-lean']:
 if path.startswith('/data'): shutil.copy('/toolchain/bin/lean', path)
 try: subprocess.run([path, '--version'], check=True)
 except PermissionError: pass
 else: raise AssertionError('direct Lean escaped confinement')
job = client.check('incremental', 'Example', timeout=40)
print(json.dumps({'state': job['state'], 'result': job['result']}))
"""
            good = run('managed', script)
            assert good.returncode == 0, good.stderr
            assert json.loads(good.stdout)['state'] == 'succeeded', good.stdout
            (repo/'Tablet/Example.lean').write_text('example : 1 = 2 := by rfl\n')
            bad = run('managed', script)
            assert bad.returncode == 0, bad.stderr
            result = json.loads(bad.stdout)
            assert result['state'] == 'failed' and 'rfl' in result['result']['stdout']
            denied = run('none', "import client; raise SystemExit(client.main(['incremental', 'Example']))")
            assert denied.returncode == 2
            assert 'no Lean' in denied.stderr
            with service.lock:
                assert service.db.execute('SELECT count(*) FROM jobs').fetchone()[0] == 3
    finally:
        service.shutdown()


def test_real_production_worker_wrapper_and_provider_start(tmp_path, monkeypatch):
    from trellis.config import SandboxConfig
    from trellis.sandbox import wrap_command
    helper = os.environ.get("TRELLIS_TEST_BINDFS") or shutil.which("bindfs")
    if not helper or not Path('/dev/fuse').exists():
        pytest.skip('requires host FUSE access')
    repo = tmp_path / 'repo'
    (repo / 'Tablet').mkdir(parents=True)
    from trellis.check_job import provision
    provision(repo/'.trellis/scripts')
    local_kernel = repo/'.trellis/runtime/bin/trellis_runtime_cli'
    local_kernel.parent.mkdir(parents=True)
    shutil.copy('/usr/bin/true', local_kernel)
    monkeypatch.setenv('TRELLIS_TRELLIS_KERNEL_CMD', '/usr/bin/true')
    subprocess.run(['git', 'init', '-q', str(repo)], check=True)
    home = tmp_path / 'task-home'
    home.mkdir()
    operator = tmp_path / 'operator'
    operator.mkdir()
    monkeypatch.setattr(Path, 'home', lambda: operator)
    monkeypatch.setenv('TRELLIS_AGENT_COMPILER_CAPABILITY', 'none')
    # Provider auth is deliberately absent; --version must never call an API.
    codex = shutil.which('codex')
    with data_view(tmp_path / 'mounts', bindfs=helper) as view:
        monkeypatch.setenv('TRELLIS_COMPILER_DATA_VIEW', str(view))
        cmd = wrap_command(['/usr/bin/python3', '-c',
            "import subprocess,os; print('worker-ready'); print(subprocess.check_output(['git','status','--porcelain'],text=True)); "
            "p=subprocess.run(['.trellis/scripts/check-job','--help'],capture_output=True,text=True); "
            "assert p.returncode==2 and 'no Lean capability' in p.stderr, p.stderr; "
            "subprocess.run(['.trellis/runtime/bin/trellis_runtime_cli'],check=True); "
            + (f"print(subprocess.check_output([{codex!r},'--version'],text=True))" if codex else "")],
            sandbox=SandboxConfig(enabled=True, backend='bwrap'), work_dir=repo, burst_home=home)
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
        assert result.returncode == 0, result.stderr
        assert 'worker-ready' in result.stdout
        script = repo / '.trellis/logs/fixture-burst.sh'
        script.parent.mkdir(parents=True, exist_ok=True)
        script.write_text('#!/bin/bash\nprintf "headless-worker-ready\\n"\n')
        script.chmod(0o755)
        direct = wrap_command([str(script)], sandbox=SandboxConfig(enabled=True, backend='bwrap'),
                              work_dir=repo, burst_home=home)
        denied = subprocess.run(direct, capture_output=True, text=True, timeout=30)
        assert denied.returncode != 0 and 'Permission denied' in denied.stderr
        interpreted = wrap_command(['/bin/bash', str(script)],
            sandbox=SandboxConfig(enabled=True, backend='bwrap'), work_dir=repo, burst_home=home)
        launched = subprocess.run(interpreted, capture_output=True, text=True, timeout=30)
        assert launched.returncode == 0 and 'headless-worker-ready' in launched.stdout, launched.stderr
        shutil.copy('/usr/bin/false', local_kernel)
        with pytest.raises(RuntimeError, match='differs from the deployed implementation'):
            wrap_command(['/usr/bin/true'], sandbox=SandboxConfig(enabled=True, backend='bwrap'),
                         work_dir=repo, burst_home=home)


def test_generated_headless_script_writes_markers_and_provider_log(tmp_path, monkeypatch):
    from trellis.adapters import ProviderConfig
    from trellis.agents.codex_headless import build_script, build_launcher_script
    from trellis.chat_history import ensure_chat_file_link
    from trellis.config import SandboxConfig
    from trellis.sandbox import wrap_command
    helper = os.environ.get('TRELLIS_TEST_BINDFS') or shutil.which('bindfs')
    if not helper or not Path('/dev/fuse').exists() or not shutil.which('codex'):
        pytest.skip('requires host FUSE access and the provider CLI')
    repo = tmp_path / 'repo'
    (repo / 'Tablet').mkdir(parents=True)
    subprocess.run(['git', 'init', '-q', str(repo)], check=True)
    local_kernel = repo / '.trellis/runtime/bin/trellis_runtime_cli'
    local_kernel.parent.mkdir(parents=True)
    shutil.copy('/usr/bin/true', local_kernel)
    monkeypatch.setenv('TRELLIS_TRELLIS_KERNEL_CMD', '/usr/bin/true')
    monkeypatch.setenv('TRELLIS_AGENT_COMPILER_CAPABILITY', 'none')
    operator, home = tmp_path / 'operator', tmp_path / 'task-home'
    operator.mkdir()
    home.mkdir()
    # Match standalone installs nested inside provider state. The later
    # provider-state data bind must not hide this immutable executable mount.
    provider_bin = operator / '.codex/packages/standalone/releases/test/bin'
    provider_bin.mkdir(parents=True)
    shutil.copy2(Path(shutil.which('codex')).resolve(), provider_bin / 'codex')
    (provider_bin.parents[2] / 'current').symlink_to(provider_bin.parent)
    launcher_bin = operator / '.local/bin'
    launcher_bin.mkdir(parents=True)
    (launcher_bin / 'codex').symlink_to(operator / '.codex/packages/standalone/current/bin/codex')
    data_binary = operator / '.codex/copied-native'
    shutil.copy2('/usr/bin/true', data_binary)
    monkeypatch.setenv('PATH', str(launcher_bin) + ':' + os.environ['PATH'])
    monkeypatch.setattr(Path, 'home', lambda: operator)
    logs = repo / '.trellis/runtime/runtime/logs'
    logs.mkdir(parents=True)
    prompt = ensure_chat_file_link(repo, log_dir=logs, artifact_prefix='fixture', role='worker',
        log_filename='fixture-prompt.txt', canonical_name='prompt.txt')
    prompt.write_text('This smoke test must not call a model.')
    output = ensure_chat_file_link(repo, log_dir=logs, artifact_prefix='fixture', role='worker',
        log_filename='fixture-output.log', canonical_name='output.log')
    output.write_text('')
    started, exited = logs / 'fixture.started', logs / 'fixture.exit'
    with data_view(tmp_path / 'mounts', bindfs=helper) as view:
        monkeypatch.setenv('TRELLIS_COMPILER_DATA_VIEW', str(view))
        script = build_script(ProviderConfig('codex', extra_args=['--help']), prompt_file=prompt,
            start_file=started, exit_file=exited, work_dir=repo, burst_home=home, log_prefix='fixture')
        command = wrap_command(['/bin/bash', str(script)], sandbox=SandboxConfig(enabled=True, backend='bwrap'),
            work_dir=repo, burst_home=home, role='worker')
        launcher = build_launcher_script(script_path=script, launch_cmd=command, log_dir=logs, log_prefix='fixture')
        result = subprocess.run(['/bin/bash', str(launcher)], capture_output=True, text=True, timeout=30)
        assert result.returncode == 0, result.stderr
        assert started.exists()
        assert exited.read_text().strip() == '0'
        assert 'Usage:' in output.read_text()
        denied_code = f'''
import errno, subprocess
try: subprocess.run([{str(data_binary)!r}], check=True)
except PermissionError: pass
else: raise AssertionError('provider state became executable')
try: open({str(provider_bin / 'codex')!r}, 'wb')
except OSError as error: assert error.errno == errno.EROFS
else: raise AssertionError('provider runtime became writable')
'''
        probe = wrap_command(['/usr/bin/python3', '-c', denied_code],
            sandbox=SandboxConfig(enabled=True, backend='bwrap'), work_dir=repo, burst_home=home)
        denied = subprocess.run(probe, capture_output=True, text=True, timeout=30)
        assert denied.returncode == 0, denied.stderr
