import os
from pathlib import Path
import shutil
import subprocess

import pytest

from trellis import check_job
from trellis.adapters import ProviderConfig
from trellis.agents import codex_headless, script_headless
from trellis.checker.jobs import JobService, register_workspace
from trellis.config import SandboxConfig
from trellis.sandbox import wrap_command


@pytest.mark.parametrize('builder', [codex_headless, script_headless])
@pytest.mark.parametrize('role, compiler_capability', [
    (role, '') for role in ['worker', 'reviewer', 'verifier', 'stuck_math_audit']
] + [(role, capability) for role in ['reviewer', 'verifier'] for capability in ['none', 'managed']])
@pytest.mark.parametrize('source_tools', [False, True])
def test_actual_generated_launcher_keeps_managed_lake_in_login_shell(tmp_path, monkeypatch, builder, role, source_tools, compiler_capability):
    if not shutil.which('bwrap'):
        pytest.skip('requires bubblewrap')
    repo = tmp_path/'repo'
    (repo/'Tablet').mkdir(parents=True)
    (repo/'Tablet/N.lean').write_text('example : True := by trivial\n')
    if source_tools:
        # PV bursts reapply the configured tools in every Bash child via
        # BASH_ENV, after the launcher has already restored its own PATH.
        state = repo/'.trellis/modeling/state.json'
        state.parent.mkdir(parents=True)
        state.write_text('{}')
        tools = repo/'source-tools'
        tools.mkdir()
        for name in ['lake', 'lean']:
            tool = tools/name
            tool.write_text('#!/bin/sh\necho unmanaged-compiler >&2\nexit 99\n')
            tool.chmod(0o755)
        monkeypatch.setattr('trellis.pv_toolchain.effective_tools', lambda _: {'lean': str(tools/'lean')})
    check_job.provision(repo/'.trellis/scripts')
    (repo/'.trellis/scratch').mkdir(exist_ok=True)
    (repo/'.trellis/scratch/Probe.lean').write_text('example : True := by trivial\n')
    logs = repo/'.trellis/chats/probe'
    logs.mkdir(parents=True)
    (logs/'prompt.txt').write_text('test provider; no model call\n')
    home = tmp_path/'home'
    bin_dir = home/'.local/bin'
    bin_dir.mkdir(parents=True)
    provider = bin_dir/'codex'
    provider.write_text('#!/bin/sh\nexec /bin/bash -lc \'set -e; command -v lake; command -v lean; '
                        'lake build Tablet.N; lake env lean Tablet/N.lean; lean .trellis/scratch/Probe.lean\'\n')
    if compiler_capability:
        # Real provider child shells reapply BASH_ENV. With no job socket,
        # both commands must refuse before executing the source toolchain.
        provider.write_text('#!/bin/sh\nexec /bin/bash -lc \'command -v lake; command -v lean; '
                            'lake build Tablet.N; lake_status=$?; '
                            'lean Tablet/N.lean; lean_status=$?; '
                            'test "$lake_status" -eq 2 && test "$lean_status" -eq 2\'\n')
    provider.chmod(0o755)
    path = f'{bin_dir}:/usr/local/bin:/usr/bin:/bin'
    monkeypatch.setattr(codex_headless, 'worker_path_env', lambda *a, **k: path)
    monkeypatch.setattr(script_headless, 'WORKER_PATH', path)
    for key in ['OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'GEMINI_API_KEY', 'GOOGLE_API_KEY', 'TRELLIS_INCREMENTAL_PREWARM', 'TRELLIS_SOURCE_PATH', 'TRELLIS_COMPILER_CAPABILITY', 'TRELLIS_AGENT_COMPILER_CAPABILITY', 'TRELLIS_MANAGED_AGENT_CHECKS']:
        monkeypatch.delenv(key, raising=False)
    runtime = tmp_path/'runtime'
    for key, value in register_workspace(runtime, repo, scope=role).items():
        monkeypatch.setenv(key, value)
    if compiler_capability:
        monkeypatch.setenv('TRELLIS_MANAGED_AGENT_CHECKS', '1')
        monkeypatch.setenv('TRELLIS_AGENT_COMPILER_CAPABILITY', compiler_capability)
        monkeypatch.delenv('TRELLIS_CHECK_JOBS_SOCKET')
        monkeypatch.delenv('TRELLIS_CHECK_JOBS_TOKEN')
    calls = []
    def execute(spec, job):
        calls.append((spec['scope'], spec['kind']))
        return dict(returncode=0, stdout='managed-checker-result\n', stderr='')
    service = JobService(runtime, execute=execute)
    service.start()
    try:
        script = builder.build_script(ProviderConfig(provider='codex'), prompt_file=logs/'prompt.txt',
            start_file=logs/'started', exit_file=logs/'exit', work_dir=repo, burst_home=home)
        command = wrap_command([str(script)], sandbox=SandboxConfig(enabled=True, backend='bwrap'),
                               work_dir=repo, burst_home=home, role=role, git_preflight_path=path)
        result = subprocess.run(command, text=True, capture_output=True, timeout=15)
        output = (logs/'worker-output.log').read_text() if (logs/'worker-output.log').exists() else ''
        assert result.returncode == 0, result.stderr + output
        assert calls == ([] if compiler_capability else [(role, kind) for kind in ['build', 'file', 'scratch']])
        assert str(repo/'.trellis/scripts/check-bin/lake') in output
        assert str(repo/'.trellis/scripts/check-bin/lean') in output
        if compiler_capability:
            assert 'unmanaged-compiler' not in output
            expected = 'no Lean capability' if compiler_capability == 'none' else 'central checker is not configured'
            assert output.count(expected) == 2
        else:
            assert 'managed-checker-result' in output
        assert 'check-job:' in output
    finally:
        service.shutdown()
