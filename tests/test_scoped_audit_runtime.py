"""Exercise the private audit's advertised validator without a provider or Lean."""
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess

import pytest

from trellis.checking import write_scripts
from trellis.dispatch.candidate import source_manifest
from trellis.dispatch.workspace import prepare
from trellis.runtime import bridge
from trellis.runtime.kernel_cli import run_kernel_cli


@pytest.mark.parametrize('support', [False, True])
def test_scoped_audit_private_validator_has_its_materialized_runtime(tmp_path, monkeypatch, support):
    from test_runtime_bridge import _fake_config, _fake_policy, _provider_binding

    canonical = tmp_path / 'canonical'
    runtime = tmp_path / 'run'
    canonical.mkdir(); runtime.mkdir()
    (canonical / 'Tablet').mkdir()
    (canonical / 'Tablet/A.lean').write_text('theorem A : True := by trivial\n')
    (canonical / 'Tablet/A.tex').write_text('A is true.\n')
    config_path = canonical / 'trellis.config.json'
    config_path.write_text(json.dumps({'repo_path': str(canonical), 'state_dir': '.trellis'}))
    (runtime / 'runtime_metadata.json').write_text(json.dumps({
        'repo_path': str(canonical), 'config_path': str(config_path)}))
    for args in [('init', '-q'), ('config', 'user.name', 'Test'),
                 ('config', 'user.email', 'test@example.invalid'), ('add', '.'),
                 ('commit', '-qm', 'private audit source')]:
        subprocess.run(['git', '-C', str(canonical), *args], check=True, capture_output=True)
    write_scripts(canonical, canonical / '.trellis')

    request = run_kernel_cli({'action': 'request_prompt_contracts', 'request': {
        'id': 41, 'cycle': 9, 'kind': 'StuckMathAudit', 'phase': 'ProofFormalization',
        'runtime_support_required': support,
        'current_present_nodes': ['A'], 'current_proof_nodes': ['A'],
        'current_deps': {'A': []},
        'scoped_audit': {'retry': 0, 'feedback': '', 'memory_entry_ids': [],
            'goals': ['A'], 'nodes': ['A'], 'reason': 'Inspect the remaining proof route',
            'previous': {}, 'completed_attempts': {}, 'recent_attempts': []},
    }})['request']
    destination = runtime / 'dispatch/verifier-1'
    prepare(runtime, canonical, config_path, destination,
            {'in_flight_request': request}, expected_manifest=source_manifest(canonical),
            seed_packages=False)
    repo = destination / 'repo'
    # Production preparation copies the launcher script, excluding its runtime.
    assert (repo / '.trellis/scripts/check.py').is_file()
    assert not (repo / '.trellis/runtime/src').exists()
    config = _fake_config(repo)
    config.parallel_dispatch = {'enabled': True}
    # The native bridge payload spells enum names in snake_case.
    request['kind'] = 'stuck_math_audit'
    request['phase'] = 'proof_formalization'
    request['stuck_math_audit_binding'] = _provider_binding(config.reviewer)
    monkeypatch.setattr(bridge, 'load_config', lambda _: config)
    monkeypatch.setattr(bridge, '_policy', lambda _: _fake_policy())
    for name in list(os.environ):
        if name.startswith('TRELLIS_') and name not in {'TRELLIS_TRELLIS_KERNEL_CMD'}:
            monkeypatch.delenv(name)
    for name, value in {
        'TRELLIS_DISPATCH_RUNTIME_ROOT': str(runtime), 'TRELLIS_MANAGED_AGENT_CHECKS': '1',
        'TRELLIS_CHECKER_TOKEN': 'workspace:' + 'x' * 40,
        'TRELLIS_CHECK_JOBS': '1', 'TRELLIS_CHECK_JOBS_SOCKET': str(runtime / 'unused.sock'),
        'TRELLIS_CHECK_JOBS_TOKEN': 'unused-test-channel', 'TRELLIS_TRELLIS_BRIDGE_DRY_RUN': '1',
    }.items():
        monkeypatch.setenv(name, value)
    preview = bridge.handle_bridge_request(bridge.BridgeCliRequest(
        config_path=repo / 'trellis.config.json', runtime_root=destination / 'runtime',
        request=request))
    prompt = Path(preview['prompt_path']).read_text()
    delivery = json.loads(prompt.split('## Kernel-authored artifact delivery contract')[1]
                          .split('```json\n')[1].split('\n```')[0])
    command = shlex.split(delivery['json_check_command'])
    raw = Path(command[3])
    raw.write_text(json.dumps({'report': '## Claim being audited\n' + 'Inspect the supplied recurrence and prove its monotone bound. ' * 8,
        'tasks': [{'id': 'task-1', 'title': 'Prove the bound',
                   'body': 'Prove the recurrence bound and check its boundary case.'}], 'probe_paths': []}))
    # Only the private repo and system interpreter/libraries are visible. The
    # external implementation checkout cannot rescue a missing local snapshot.
    bwrap = shutil.which('bwrap')
    if bwrap is None:
        pytest.skip('requires bubblewrap for private validator isolation')
    isolated = [bwrap, '--die-with-parent', '--unshare-net', '--ro-bind', '/usr', '/usr']
    for path in ('/lib', '/lib64', '/bin'):
        if Path(path).exists():
            isolated += ['--ro-bind', path, path]
    isolated += ['--proc', '/proc', '--dev', '/dev', '--tmpfs', '/tmp',
                 '--bind', str(repo), str(repo), '--chdir', str(repo), '--', *command]
    result = subprocess.run(isolated, env={'PATH': '/usr/bin:/bin', 'PYTHONDONTWRITEBYTECODE': '1'},
                            text=True, capture_output=True, timeout=30)
    if not support:
        assert result.returncode != 0
        assert 'ModuleNotFoundError' in result.stderr and 'trellis.agent_check' in result.stderr
    else:
        assert (repo / '.trellis/runtime/src/trellis/agent_check.py').is_file()
        assert (repo / '.trellis/runtime/acceptance-build.json').is_file()
        assert result.returncode == 0, result.stdout + result.stderr
        assert result.stdout.strip().startswith('OK')
