"""Native provenance keeps exact Elan selection checks inside managed agents."""
import json
from pathlib import Path
import shutil
import subprocess

import pytest

from trellis import check_job, pv_toolchain
from trellis.checker.jobs import JobService, register_workspace
from trellis.config import SandboxConfig
from trellis.host_runtime import worker_elan_home
from trellis.sandbox import wrap_command


TOOLCHAIN = 'leanprover/lean4:v4.30.0-rc2'


def resources(root, lean):
    package = root / 'library'
    library = package / '.lake/build/lib/lean'
    library.mkdir(parents=True)
    (library / 'Aeneas.olean').write_bytes(b'fixture library; no compilation')
    (package / 'lakefile.toml').write_text('name = "fixture"\n')
    return dict(lean=str(lean), lean_toolchain=TOOLCHAIN,
                lean_paths=[str(library)], aeneas_lean_lib=str(package))


@pytest.mark.parametrize('failure', ['', 'stale', 'missing_freshness', 'failed', 'nonzero', 'toolchain', 'executable'])
def test_managed_native_resources_requires_exact_current_identity(tmp_path, monkeypatch, failure):
    lean = tmp_path / 'lean'
    lean.write_text('fixture executable')
    tools = resources(tmp_path, lean)
    monkeypatch.setenv('TRELLIS_COMPILER_CAPABILITY', 'managed')
    monkeypatch.setattr(subprocess, 'run', lambda *a, **k: pytest.fail('local Elan invoked'))
    job = dict(state='succeeded', current_inputs_match=True,
               result=dict(returncode=0, stdout=str(lean) + '\n', toolchain=TOOLCHAIN))
    if failure == 'stale':
        job['current_inputs_match'] = False
    elif failure == 'missing_freshness':
        job.pop('current_inputs_match')
    elif failure == 'failed':
        job['state'] = 'interrupted'
    elif failure == 'nonzero':
        job['result']['returncode'] = 1
    elif failure == 'toolchain':
        job['result']['toolchain'] = 'another/workspace:v1'
    elif failure == 'executable':
        job['result']['stdout'] = str(tmp_path / 'other-lean')
    def query(tool, argument):
        assert (tool, argument) == ('elan', 'which-lean')
        return job
    monkeypatch.setattr(check_job, 'tool_info', query)
    if failure:
        with pytest.raises(ValueError):
            pv_toolchain.native_resources(tools)
    else:
        assert pv_toolchain.native_resources(tools)['lean_toolchain'] == TOOLCHAIN


def test_none_cannot_query_native_toolchain(tmp_path, monkeypatch):
    tools = resources(tmp_path, tmp_path / 'lean')
    monkeypatch.setenv('TRELLIS_COMPILER_CAPABILITY', 'none')
    monkeypatch.setattr(check_job, 'tool_info', lambda *a: pytest.fail('metadata requested'))
    monkeypatch.setattr(subprocess, 'run', lambda *a, **k: pytest.fail('local Elan invoked'))
    with pytest.raises(ValueError, match='no Lean capability'):
        pv_toolchain.native_resources(tools)


def test_offline_native_resources_keeps_explicit_elan_selection(tmp_path, monkeypatch):
    from types import SimpleNamespace
    tools = resources(tmp_path, tmp_path / 'lean')
    monkeypatch.delenv('TRELLIS_COMPILER_CAPABILITY', raising=False)
    def run(command, **kwargs):
        assert command == [str(worker_elan_home() / 'bin/elan'), 'which', 'lean']
        assert kwargs['env']['ELAN_TOOLCHAIN'] == TOOLCHAIN
        assert kwargs['env']['ELAN_HOME'] == str(worker_elan_home())
        return SimpleNamespace(returncode=0, stdout=tools['lean'] + '\n')
    monkeypatch.setattr(subprocess, 'run', run)
    assert pv_toolchain.native_resources(tools)['lean_toolchain'] == TOOLCHAIN


def test_managed_provenance_resources_through_real_guard_and_host_elan(tmp_path, monkeypatch):
    lean = worker_elan_home() / 'toolchains/leanprover--lean4---v4.30.0-rc2/bin/lean'
    if not shutil.which('bwrap') or not lean.is_file():
        pytest.skip('requires stock bubblewrap and the installed Lean toolchain')
    repo = tmp_path / 'repo'
    (repo / 'Tablet').mkdir(parents=True)
    (repo / 'lean-toolchain').write_text(TOOLCHAIN + '\n')
    tools = resources(repo, lean)
    check_job.provision(repo / '.trellis/scripts')
    home = tmp_path / 'home'
    home.mkdir()
    runtime = tmp_path / 'runtime'
    for key, value in register_workspace(runtime, repo, scope='worker').items():
        monkeypatch.setenv(key, value)
    monkeypatch.setenv('TRELLIS_MANAGED_AGENT_CHECKS', '1')
    monkeypatch.setenv('TRELLIS_AGENT_COMPILER_CAPABILITY', 'managed')
    # The host's ambient selection must not replace this workspace's capture.
    monkeypatch.setenv('ELAN_HOME', str(worker_elan_home()))
    monkeypatch.setenv('ELAN_TOOLCHAIN', 'nonexistent-host-default')
    source = Path(__file__).resolve().parents[1]
    monkeypatch.setenv('TRELLIS_SANDBOX_EXTRA_RO_PATHS', str(source))
    monkeypatch.setenv('PYTHONPATH', str(source))
    code = '''
import os, subprocess
from trellis.pv_toolchain import native_resources, native_resource_environment
p = subprocess.run([%r, 'which', 'lean'], text=True, capture_output=True)
assert p.returncode == 2 and 'Do not' in p.stderr, p
tools = %r
# The old local lookup reproduces the recorded provenance failure here.
os.environ.pop('TRELLIS_COMPILER_CAPABILITY')
try: native_resources(tools)
except ValueError as error: assert 'lean_toolchain must resolve' in str(error), error
else: raise AssertionError('guarded local Elan unexpectedly resolved')
os.environ['TRELLIS_COMPILER_CAPABILITY'] = 'managed'
resources = native_resources(tools)
assert resources['lean_toolchain'] == %r
assert native_resource_environment(%r)['ELAN_TOOLCHAIN'] == %r
print('managed-provenance-resources-ok')
''' % (str(worker_elan_home() / 'bin/elan'), tools, TOOLCHAIN, tools, TOOLCHAIN)
    service = JobService(runtime)
    service.start()
    try:
        command = wrap_command(['/usr/bin/python3', '-c', code],
                               sandbox=SandboxConfig(enabled=True, backend='bwrap'),
                               work_dir=repo, burst_home=home, role='worker')
        result = subprocess.run(command, text=True, capture_output=True, timeout=30)
        assert result.returncode == 0, result.stderr + result.stdout
        assert 'managed-provenance-resources-ok' in result.stdout
        rows = service.db.execute('SELECT request,state FROM jobs').fetchall()
        assert len(rows) == 1, 'repeated provenance resource checks reuse the captured metadata job'
        assert json.loads(rows[0]['request'])['target'] == 'elan/which-lean'
        assert rows[0]['state'] == 'succeeded'
    finally:
        service.shutdown()


@pytest.mark.parametrize('target', ['elan/which', 'elan/which-lake', 'elan/which-lean --run', 'elan/../lean', 'elan/toolchain-install'])
def test_elan_metadata_does_not_accept_arbitrary_commands(target):
    with pytest.raises(ValueError, match='unsupported tool metadata'):
        JobService._validate(dict(action='submit', kind='tool_info', target=target, request_id='probe'))
