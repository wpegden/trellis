"""Exercise local launch scripts with a live-shell tmux stub, never real tmux."""
import importlib
import json
import subprocess
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

from git_trust_test_support import clean_install_environment, native_lake_path
from trellis import burst, git_trust as gt
from trellis.adapters import ProviderConfig
from trellis.agents import tmux_backend


@pytest.fixture(autouse=True)
def isolated_lake_system_config(tmp_path, monkeypatch):
    monkeypatch.setenv('LAKE_CONFIG', str(tmp_path / 'absent-system-config.toml'))
    clean_install_environment(monkeypatch)


@pytest.mark.parametrize('backend', ['codex_headless', 'script_headless'])
@pytest.mark.parametrize('bootstrap', [False, True])
def test_alive_tmux_shell_returns_durable_failure_promptly(tmp_path, monkeypatch, backend, bootstrap):
    module = importlib.import_module('trellis.agents.' + backend)
    report = tmp_path / 'home/.trellis/git-preflight/launch-fixture'
    assert not report.exists()  # The recorder/gate creates storage at launch.
    package = tmp_path / '.lake/packages/mathlib'
    package.mkdir(parents=True)
    (tmp_path / 'lake-manifest.json').write_text(json.dumps(dict(version='1.2.0', packagesDir='.lake/packages',
        packages=[dict(name='mathlib', type='git', inherited=False, url='remote', rev='pin')])))
    (tmp_path / 'lakefile.toml').write_text('name = \"root\"\n[[require]]\nname = \"mathlib\"\n')
    bin_dir = tmp_path / 'bin'
    bin_dir.mkdir()
    git = bin_dir / 'git'
    git.write_text('#!/bin/sh\nprintf "fatal: detected dubious ownership\\r\\n \\t" >&2\nexit 128\n')
    git.chmod(0o755)
    monkeypatch.setattr(tmux_backend, '_submit_probe_for_burst', lambda *a, **kw: None)
    monkeypatch.setattr(module.time, 'sleep', lambda _: None)
    monkeypatch.setattr(burst, 'tmux_ensure_session', lambda _: None)
    monkeypatch.setattr(burst, 'tmux_kill_window', lambda *a: None)
    killed = []
    monkeypatch.setattr(burst, 'tmux_kill_session', killed.append)
    monkeypatch.setattr(burst, 'tmux_pane_is_dead', lambda _: False)  # Regression's critical condition.
    seen = {}
    def wrap(command, **kwargs):
        seen.update(kwargs)
        inner = [sys.executable, '-I', '-S', str(Path(gt.__file__).resolve()), '--gate',
                 str(tmp_path), 'worker', native_lake_path(bin_dir), str(report), '/usr/bin/touch', str(tmp_path / 'payload-ran')]
        if bootstrap:
            # Fail before executing the gate, retaining recognizable gate argv
            # so the outside recorder still knows its report directory.
            return ['/bin/sh', '-c', 'printf "bootstrap error\\r\\n" >&2; exit 71', *inner]
        return inner
    monkeypatch.setattr(module, 'wrap_command', wrap)
    def tmux(*args, **kwargs):
        if args[0] == 'new-window':
            return SimpleNamespace(returncode=0, stdout='@9 %9\n', stderr='')
        if args[0] == 'send-keys':
            # Reproduce the reviewed failure mode: the launch fails, then the
            # parent shell continues running and successfully executes more input.
            assert not args[-2].startswith('exec ')
            proc = subprocess.run(['/bin/bash', '-c', args[-2] + '; printf shell-is-alive'], capture_output=True)
            assert proc.returncode == 0 and proc.stdout.endswith(b'shell-is-alive')
            assert json.loads((report / 'launch.json').read_bytes())['status'] == (71 if bootstrap else 128)
        assert args[0] != 'capture-pane', 'Durable files must replace terminal scrollback'
        return SimpleNamespace(returncode=0, stdout='', stderr='')
    monkeypatch.setattr(burst, 'tmux_cmd', tmux)
    result = module.run(ProviderConfig(provider='codex', model='fixture'), 'prompt', role='worker',
        session_name='fixture', work_dir=tmp_path, log_dir=tmp_path / 'logs', startup_timeout=30)
    assert result.exit_code == (71 if bootstrap else 128) and not result.ok
    assert ('bootstrap error' if bootstrap else 'dubious ownership') in result.captured_output
    assert 'Git preflight/bootstrap' in result.error
    assert not (tmp_path / 'payload-ran').exists() and killed == ['fixture']
    assert seen['git_preflight_path'] == (module.WORKER_PATH if backend == 'script_headless' else module.worker_path_env(None))
    assert (report / 'launch.json').exists()
    if not bootstrap:
        assert (report / '001-remote.stderr').read_bytes() == b'fatal: detected dubious ownership\r\n \t'


@pytest.mark.parametrize('provider', ['claude', 'gemini'])
def test_tui_readiness_detects_durable_report_with_alive_pane(tmp_path, monkeypatch, provider):
    command = [sys.executable, '-I', '-S', str(Path(gt.__file__).resolve()), '--gate',
               str(tmp_path), 'reviewer', '', str(tmp_path), 'provider']
    gt._write_result(tmp_path / 'result.json', dict(status=128, message='Git refusal', checks=[]))
    monkeypatch.setattr(tmux_backend, 'pane_dead', lambda _: False)
    monkeypatch.setattr(tmux_backend, 'capture', lambda *a, **kw: pytest.fail('must not require pane text'))
    handle = tmux_backend.AgentHandle(session='fixture', cwd=tmp_path, provider=provider, preflight_command=command)
    ready, reasons = tmux_backend.settle_until_ready(handle)
    assert not ready and reasons == ['git_preflight_failed']


def test_headless_launcher_preserves_the_baseline_c_locale_environment(tmp_path):
    from trellis.agents.codex_headless import build_launcher_script
    env = {b'PATH': b'/usr/bin:/bin', b'LANG': b'C', b'SHLVL': b'3',
           b'PYTHONHOME': b'/does/not/exist', b'PYTHONPATH': b'/do/not/import'}
    def run(command):
        launcher = build_launcher_script(script_path=tmp_path / 'unused', launch_cmd=command,
                                         log_dir=tmp_path, log_prefix='fixture')
        # The same path and same interactive-shell convention for both runs.
        result = subprocess.run(['/bin/bash', str(launcher)], env=env, capture_output=True)
        assert result.returncode == 0, result.stderr
        return dict(item.split(b'=', 1) for item in result.stdout.split(b'\0') if item)
    baseline = run(['/usr/bin/env', '-0'])
    reports = tmp_path / 'reports'
    reports.mkdir()
    command = [sys.executable, '-I', '-S', str(Path(gt.__file__).resolve()), '--gate',
               str(tmp_path), 'reviewer', '', str(reports), '/usr/bin/env', '-0']
    assert run(command) == baseline


@pytest.mark.parametrize('backend', ['codex_headless', 'script_headless'])
@pytest.mark.parametrize('bootstrap', [False, True])
def test_async_publication_is_seen_while_alive_shell_is_already_polling(tmp_path, monkeypatch, backend, bootstrap):
    import shlex
    import time
    module = importlib.import_module('trellis.agents.' + backend)
    reports = tmp_path / 'reports'
    reports.mkdir()
    release = tmp_path / 'release'
    command = [sys.executable, '-I', '-S', str(Path(gt.__file__).resolve()), '--gate',
               str(tmp_path), 'worker', '', str(reports), 'true']
    # Record a pre-gate failure asynchronously. The barrier proves supervisor
    # polling has begun before the recorder can publish its atomic launch.json.
    shell = f'while ! test -f {shlex.quote(str(release))}; do /bin/sleep 0.01; done; printf "delayed failure\\r\\n" >&2; exit 72'
    status = 72
    wrapped = ['/bin/sh', '-c', shell, *command]
    if not bootstrap:
        status = 128
        (tmp_path / '.lake/packages/mathlib').mkdir(parents=True)
        (tmp_path / 'lakefile.toml').write_text('name = "root"\n[[require]]\nname = "mathlib"\n')
        (tmp_path / 'lake-manifest.json').write_text(json.dumps(dict(version='1.2.0', packagesDir='.lake/packages',
            packages=[dict(name='mathlib', type='git', inherited=False, rev='pin', url='remote')])))
        git = tmp_path / 'git'
        git.write_text('#!/bin/sh\n' + shell.replace('exit 72', 'exit 128') + '\n')
        git.chmod(0o755)
        wrapped = list(command)
        wrapped[7] = native_lake_path(tmp_path)  # Preflight PATH; no payload env mutation.
    monkeypatch.setattr(module, 'wrap_command', lambda *a, **kw: wrapped)
    monkeypatch.setattr(tmux_backend, '_submit_probe_for_burst', lambda *a, **kw: None)
    monkeypatch.setattr(burst, 'tmux_ensure_session', lambda _: None)
    monkeypatch.setattr(burst, 'tmux_kill_window', lambda *a: None)
    killed = []
    monkeypatch.setattr(burst, 'tmux_kill_session', killed.append)
    monkeypatch.setattr(burst, 'tmux_pane_is_dead', lambda _: False)
    children = []
    def tmux(*args, **kwargs):
        if args[0] == 'new-window':
            return SimpleNamespace(returncode=0, stdout='@9 %9\n', stderr='')
        if args[0] == 'send-keys':
            children.append(subprocess.Popen(['/bin/bash', '-c', args[-2] + '; printf shell-is-alive'],
                                              stdout=subprocess.PIPE, stderr=subprocess.PIPE))
        assert args[0] != 'capture-pane'
        return SimpleNamespace(returncode=0, stdout='', stderr='')
    monkeypatch.setattr(burst, 'tmux_cmd', tmux)
    polls = []
    def observe(command):
        failure = gt.launch_failure(command)
        polls.append(failure)
        if len(polls) == 2:
            assert not (reports / 'launch.json').exists() and not (reports / 'result.json').exists()
            release.touch()
        return failure
    monkeypatch.setattr(module, 'launch_failure', observe)
    began = time.monotonic()
    try:
        result = module.run(ProviderConfig(provider='codex', model='fixture'), 'prompt', role='worker',
            session_name='fixture', work_dir=tmp_path, log_dir=tmp_path / 'logs', startup_timeout=10)
        assert result.exit_code == status and 'delayed failure' in result.captured_output
        assert 'preflight/bootstrap' in result.error and time.monotonic() - began < 5
        assert polls[:2] == [None, None] and polls[-1][0] == status
        assert killed == ['fixture']
    finally:
        release.touch()
        for child in children:
            stdout, _ = child.communicate(timeout=5)
            assert child.returncode == 0 and stdout == b'shell-is-alive'
    raw_file = 'bootstrap.stderr' if bootstrap else '001-remote.stderr'
    assert (reports / raw_file).read_bytes() == b'delayed failure\r\n'


@pytest.mark.parametrize('backend', ['codex_headless', 'script_headless'])
@pytest.mark.parametrize('complete', [False, True])
def test_startup_timeout_recovers_durable_failure_or_partial_stderr(tmp_path, monkeypatch, backend, complete):
    import time
    module = importlib.import_module('trellis.agents.' + backend)
    report = tmp_path / 'reports'
    report.mkdir()
    command = [sys.executable, '-I', '-S', str(Path(gt.__file__).resolve()), '--gate',
               str(tmp_path), 'worker', '', str(report), 'true']
    monkeypatch.setattr(module, 'wrap_command', lambda *a, **kw: command)
    monkeypatch.setattr(tmux_backend, '_submit_probe_for_burst', lambda *a, **kw: None)
    monkeypatch.setattr(burst, 'tmux_ensure_session', lambda _: None)
    monkeypatch.setattr(burst, 'tmux_kill_window', lambda *a: None)
    monkeypatch.setattr(burst, 'tmux_pane_is_dead', lambda _: False)
    killed = []
    monkeypatch.setattr(burst, 'tmux_kill_session', killed.append)
    # Deterministic monotonic clock: run the initial startup loop poll, then
    # publish during its sleep and cross the deadline before the next iteration.
    now = [0.0]
    pending = [False]
    raw = b'partial bootstrap error\r\n \t\xff'
    def sleep(seconds):
        if pending[0]:
            (report / 'bootstrap.stderr').write_bytes(raw)
            if complete:
                gt._write_result(report / 'launch.json', dict(status=74, message='completed during timeout'))
            now[0] += 100
            pending[0] = False
    monkeypatch.setattr(module, 'time', SimpleNamespace(monotonic=lambda: now[0], time=time.time, sleep=sleep))
    def tmux(*args, **kwargs):
        if args[0] == 'new-window':
            return SimpleNamespace(returncode=0, stdout='@9 %9\n', stderr='')
        if args[0] == 'send-keys':
            pending[0] = True
        assert args[0] != 'capture-pane'
        return SimpleNamespace(returncode=0, stdout='', stderr='')
    monkeypatch.setattr(burst, 'tmux_cmd', tmux)
    result = module.run(ProviderConfig(provider='codex', model='fixture'), 'prompt', role='worker',
        session_name='fixture', work_dir=tmp_path, log_dir=tmp_path / 'logs', startup_timeout=1)
    assert 'before timeout' in result.error
    assert result.exit_code == (74 if complete else None)
    assert raw.decode(errors='replace') in result.captured_output and killed == ['fixture']
    assert (report / 'bootstrap.stderr').read_bytes() == raw


@pytest.mark.parametrize('provider', ['claude', 'gemini'])
def test_full_tui_driver_returns_preflight_failure_without_retry(tmp_path, monkeypatch, provider):
    from trellis import gemini_accounts
    for name in ('_submit_probe_for_burst', '_emit_burst_launched', 'emit_event',
                 'pre_trust_gemini_folder', 'ensure_gemini_accessibility_settings',
                 'ensure_gemini_cli_updated', '_gemini_launch_stagger'):
        monkeypatch.setattr(tmux_backend, name, lambda *a, **kw: None)
    monkeypatch.setattr(gemini_accounts, 'maybe_ensure_budget', lambda **kw: None)
    monkeypatch.setattr(gemini_accounts, 'gemini_api_env_keys_to_forward', lambda **kw: ())
    monkeypatch.setattr(tmux_backend, '_gemini_active_account', lambda *a: None)
    monkeypatch.setattr(tmux_backend, 'load_session_identity', lambda *a, **kw: None)
    monkeypatch.setattr(tmux_backend, 'claude_session_exists', lambda *a, **kw: False)
    monkeypatch.setattr(tmux_backend, 'claude_launch_settings_args', lambda: [])
    monkeypatch.setattr(tmux_backend, 'pane_dead', lambda _: False)
    monkeypatch.setattr(tmux_backend, 'capture', lambda *a, **kw: pytest.fail('failure must use durable report'))
    monkeypatch.setattr(tmux_backend, 'send_prompt', lambda *a, **kw: pytest.fail('provider must not start'))
    reports = tmp_path / 'reports'
    reports.mkdir()
    package = tmp_path / '.lake/packages/mathlib'
    package.mkdir(parents=True)
    (tmp_path / 'lakefile.toml').write_text('name = "root"\n[[require]]\nname = "mathlib"\n')
    (tmp_path / 'lake-manifest.json').write_text(json.dumps(dict(version='1.2.0', packagesDir='.lake/packages',
        packages=[dict(name='mathlib', type='git', inherited=False, rev='pin', url='remote')])))
    git = tmp_path / 'git'
    git.write_text('#!/bin/sh\nprintf "fatal: not a git repository\\r\\n \\t" >&2\nexit 128\n')
    git.chmod(0o755)
    command = [sys.executable, '-I', '-S', str(Path(gt.__file__).resolve()), '--gate',
               str(tmp_path), 'worker', native_lake_path(tmp_path), str(reports), '/usr/bin/touch', str(tmp_path / 'payload')]
    monkeypatch.setattr(tmux_backend, 'sandbox_wrap', lambda *a, **kw: command)
    killed, launched = [], []
    monkeypatch.setattr(tmux_backend, 'kill_session', killed.append)
    def launch(session, *, cmd, **kwargs):
        launched.append(cmd)
        assert subprocess.run(cmd, capture_output=True, timeout=5).returncode == 128
    monkeypatch.setattr(tmux_backend, 'new_session', launch)
    result = getattr(tmux_backend, 'run_' + provider + '_burst')(
        cwd=tmp_path, prompt='unused', burst_home=tmp_path / 'home', name_hint='fixture', max_restarts=2)
    assert not result['ok'] and result['exit_code'] == 128
    assert 'not a repository' in result['captured_output'] and 'Git preflight' in result['error']
    assert len(launched) == 1 and killed == ['fixture', 'fixture']
    assert not (tmp_path / 'payload').exists()
    assert (reports / '001-remote.stderr').read_bytes() == b'fatal: not a git repository\r\n \t'


@pytest.mark.parametrize(('backend', 'provider'), [('codex_headless', 'codex'), ('script_headless', 'claude'), ('script_headless', 'gemini')])
def test_successful_provider_log_bytes_survive_complete_gate_chain(tmp_path, monkeypatch, backend, provider):
    module = importlib.import_module('trellis.agents.' + backend)
    binaries = tmp_path / 'bin'
    binaries.mkdir()
    # Deterministic native fake provider, including JSON whitespace and raw stderr.
    cli = binaries / provider
    cli.write_text('#!/bin/sh\nprintf \'{"type":"fixture", "text":"ok"}\\r\\n\'\nprintf \'diagnostic \\377\\r\\n \\t\' >&2\n')
    cli.chmod(0o755)
    git = binaries / 'git'
    git.write_text('#!/bin/sh\nprintf "pin\\n"\nprintf "git warning\\r\\n" >&2\n')
    git.chmod(0o755)
    provider_path = native_lake_path(binaries) + ':/usr/bin:/bin'
    if backend == 'codex_headless':
        monkeypatch.setattr(module, 'worker_path_env', lambda *a, **kw: provider_path)
    else:
        monkeypatch.setattr(module, 'WORKER_PATH', provider_path)
    monkeypatch.setattr(module, 'gemini_api_env_keys_to_forward', lambda **kw: ())
    prompt = tmp_path / 'prompt'
    prompt.write_text('fixture')
    (tmp_path / '.lake/packages/mathlib').mkdir(parents=True)
    (tmp_path / 'lakefile.toml').write_text('name = "root"\n[[require]]\nname = "mathlib"\n')
    (tmp_path / 'lake-manifest.json').write_text(json.dumps(dict(version='1.2.0', packagesDir='.lake/packages',
        packages=[dict(name='mathlib', type='git', inherited=False, rev='pin', url='remote')])))
    env = dict(PATH='/usr/bin:/bin', HOME=str(tmp_path), LANG='C')
    logs = []
    for gated in (False, True):
        directory = tmp_path / ('gated' if gated else 'baseline')
        directory.mkdir()
        start, end = directory / 'started', directory / 'exit'
        script = module.build_script(ProviderConfig(provider=provider, model='fixture'), prompt_file=prompt,
            start_file=start, exit_file=end, work_dir=tmp_path, log_prefix='fixture')
        command = [str(script)]
        reports = directory / 'reports'
        reports.mkdir()
        if gated:
            command = [sys.executable, '-I', '-S', str(Path(gt.__file__).resolve()), '--gate',
                       str(tmp_path), 'worker', provider_path, str(reports), *command]
        kwargs = dict(launch_cmd=command, log_dir=directory, log_prefix='fixture')
        if backend == 'codex_headless':
            kwargs['script_path'] = script
        launcher = module.build_launcher_script(**kwargs)
        result = subprocess.run([str(launcher)], env=env, capture_output=True, timeout=10)
        assert result.returncode == 0 and result.stdout == b'' and result.stderr == b''
        assert start.exists() and end.read_text().strip() == '0'
        logs.append((directory / 'fixture-output.log').read_bytes())
        if gated:
            assert json.loads((reports / 'launch.json').read_bytes())['status'] == 0
            assert json.loads((reports / 'result.json').read_bytes())['outcome'] == 'pass'
            assert (reports / '000-rev-parse.stderr').read_bytes() == b'git warning\r\n'
            assert (reports / 'bootstrap.stderr').read_bytes() == b''
    assert logs[0] == logs[1] == b'{"type":"fixture", "text":"ok"}\r\ndiagnostic \xff\r\n \t'
