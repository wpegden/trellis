"""Cooperative compiler commands without a privileged helper or FUSE mount."""
import os
from pathlib import Path
import subprocess

import pytest

from trellis import check_job
from trellis.config import SandboxConfig
from trellis.sandbox import wrap_command


@pytest.fixture
def workspace(tmp_path, monkeypatch):
    repo = tmp_path / 'repo'
    (repo / 'Tablet').mkdir(parents=True)
    (repo / '.lake/packages/shared').mkdir(parents=True)
    (repo / '.lake/packages/shared/input').write_text('unchanged')
    home = tmp_path / 'home'
    home.mkdir()
    check_job.provision(repo / '.trellis/scripts')
    monkeypatch.setenv('TRELLIS_MANAGED_AGENT_CHECKS', '1')
    for name in ('TRELLIS_COMPILER_DATA_VIEW', 'TRELLIS_COMPILER_BWRAP', 'TRELLIS_TRELLIS_KERNEL_CMD'):
        monkeypatch.delenv(name, raising=False)
    return repo, home


@pytest.mark.parametrize('marker', ['legacy', 'capability'])
def test_managed_client_does_not_fall_back_to_local_compilation(monkeypatch, capsys, marker):
    monkeypatch.setenv('TRELLIS_MANAGED_AGENT_CHECKS', '1')
    monkeypatch.delenv('TRELLIS_COMPILER_CAPABILITY', raising=False)
    if marker == 'capability':
        monkeypatch.delenv('TRELLIS_MANAGED_AGENT_CHECKS')
        monkeypatch.setenv('TRELLIS_COMPILER_CAPABILITY', 'managed')
    monkeypatch.delenv('TRELLIS_CHECK_JOBS_SOCKET', raising=False)
    monkeypatch.setattr(subprocess, 'run', lambda *a, **k: pytest.fail('unmanaged compiler started'))
    assert check_job.main(['build', 'Tablet.A']) == 2
    assert 'report this infrastructure problem in your response' in capsys.readouterr().err


def test_nl_only_sandbox_withholds_compiler_tokens(workspace, monkeypatch):
    repo, home = workspace
    monkeypatch.setattr('trellis.sandbox.bwrap_available', lambda: True)
    monkeypatch.setenv('TRELLIS_AGENT_COMPILER_CAPABILITY', 'none')
    monkeypatch.setenv('TRELLIS_CHECKER_TOKEN', 'authority-secret')
    monkeypatch.setenv('TRELLIS_CHECK_JOBS_TOKEN', 'advisory-secret')
    cmd = wrap_command(['/usr/bin/true'], sandbox=SandboxConfig(enabled=True, backend='bwrap'),
                       work_dir=repo, burst_home=home)
    assert 'authority-secret' not in cmd and 'advisory-secret' not in cmd
    assert '--jitless' not in cmd and not any('compiler_guard.py' in part for part in cmd)
    assert ['--ro-bind', str(repo / '.lake/packages'), str(repo / '.lake/packages')] in [cmd[i:i+3] for i in range(len(cmd))]
    for name in ('TRELLIS_CHECKER_TOKEN', 'TRELLIS_CHECK_JOBS_TOKEN'):
        assert ['--unsetenv', name] in [cmd[i:i+2] for i in range(len(cmd))]


@pytest.mark.skipif(os.environ.get('TRELLIS_TEST_MANAGED_GUARDS') != '1', reason='opt-in host stock-bwrap smoke')
@pytest.mark.parametrize('capability', ['managed', 'none'])
def test_real_stock_bwrap_guard_and_native_execution(workspace, monkeypatch, capability):
    repo, home = workspace
    from trellis.host_runtime import worker_elan_home
    monkeypatch.setenv('TRELLIS_AGENT_COMPILER_CAPABILITY', capability)
    lean = worker_elan_home() / 'bin/lean'
    assert lean.is_file()
    # A regular executable and executable anonymous memory remain available;
    # only ordinary compiler entrypoints are guarded. Shared caches stay RO.
    code = '''
import mmap, pathlib, subprocess
r = subprocess.run([%r, '--version'], capture_output=True, text=True)
assert r.returncode == 2 and 'Do not' in r.stderr, r
assert %r in r.stderr, r.stderr
assert subprocess.run(['/usr/bin/true']).returncode == 0
m = mmap.mmap(-1, 4096, prot=mmap.PROT_READ | mmap.PROT_WRITE | mmap.PROT_EXEC)
m.close()
try: pathlib.Path('.lake/packages/shared/input').write_text('changed')
except OSError: pass
else: raise AssertionError('shared input was writable')
print('stock-bwrap-command-guard-ok')
''' % (str(lean), '.trellis/scripts/check-job' if capability == 'managed' else 'no Lean capability')
    cmd = wrap_command(['/usr/bin/python3', '-c', code], sandbox=SandboxConfig(enabled=True, backend='bwrap'),
                       work_dir=repo, burst_home=home)
    result = subprocess.run(cmd, text=True, capture_output=True, timeout=30)
    assert result.returncode == 0, result.stderr
    assert 'stock-bwrap-command-guard-ok' in result.stdout
