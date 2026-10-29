from pathlib import Path
import os
import json
import subprocess
import sys

import pytest

from trellis.checker import delegation


def test_default_launch_remains_direct(monkeypatch):
    monkeypatch.delenv('TRELLIS_CHECK_CGROUP', raising=False)
    monkeypatch.setattr(delegation, 'current_root', lambda: pytest.fail('no delegation for default launch'))
    delegation.ensure(['/runtime'])


def test_already_delegated_scope_needs_no_wrapper(monkeypatch):
    monkeypatch.setenv('TRELLIS_CHECK_CGROUP', 'delegated')
    monkeypatch.setattr(delegation, 'current_root', lambda: Path('/owned.scope'))
    monkeypatch.setattr(delegation, 'delegated', lambda _: True)
    monkeypatch.setattr(delegation.os, 'execvpe', lambda *_: pytest.fail('already delegated'))
    monkeypatch.setattr(delegation, 'exclusive', lambda _: True)
    delegation.ensure(['/runtime'])


def test_scope_bootstrap_keeps_command_arguments_and_environment(monkeypatch):
    monkeypatch.setenv('TRELLIS_CHECK_CGROUP', 'delegated')
    monkeypatch.setenv('TRELLIS_CHECK_MEMORY_POLICY_GENERATION', '8')
    monkeypatch.delenv(delegation._ATTEMPTED, raising=False)
    monkeypatch.setattr(delegation.os, 'getuid', lambda: 123)
    monkeypatch.setattr(delegation, 'current_root', lambda: Path('/login.scope'))
    monkeypatch.setattr(delegation, 'delegated', lambda _: False)
    monkeypatch.setattr(Path, 'exists', lambda path: str(path) == '/run/user/123/bus')
    called = []
    monkeypatch.setattr(delegation.os, 'execvpe', lambda *args: called.append(args))
    delegation.ensure(['/run with spaces', '--parallelism', '6'])
    binary, args, env = called[0]
    assert binary == 'systemd-run' and '--scope' in args and '--collect' in args
    assert '--property=Delegate=memory' in args and '--wait' not in args
    assert args[args.index('--')+1:] == [sys.executable, '-m', 'trellis.checker.server',
        '/run with spaces', '--parallelism', '6']
    assert env['TRELLIS_CHECK_MEMORY_POLICY_GENERATION'] == '8'
    assert env[delegation._ATTEMPTED] == str(os.getpid())
    assert env['DBUS_SESSION_BUS_ADDRESS'] == 'unix:path=/run/user/123/bus'


def test_failed_delegation_does_not_reexec_forever(monkeypatch):
    monkeypatch.setenv('TRELLIS_CHECK_CGROUP', 'delegated')
    monkeypatch.setenv(delegation._ATTEMPTED, str(os.getpid()))
    monkeypatch.setattr(delegation, 'current_root', lambda: Path('/wrong'))
    monkeypatch.setattr(delegation, 'delegated', lambda _: False)
    with pytest.raises(RuntimeError, match='writable memory controller'):
        delegation.ensure(['/runtime'])


def test_missing_user_manager_is_clear_error(monkeypatch):
    monkeypatch.setenv('TRELLIS_CHECK_CGROUP', 'delegated')
    monkeypatch.delenv(delegation._ATTEMPTED, raising=False)
    monkeypatch.setattr(delegation, 'current_root', lambda: Path('/login.scope'))
    monkeypatch.setattr(delegation, 'delegated', lambda _: False)
    monkeypatch.setattr(Path, 'exists', lambda _: False)
    with pytest.raises(RuntimeError, match='current user systemd manager'):
        delegation.ensure(['/runtime'])


def group_root(tmp_path, procs=None):
    (tmp_path / 'cgroup.procs').write_text(str(os.getpid()) if procs is None else procs)
    return tmp_path


def test_exclusive_root_rejects_shared_direct_processes(tmp_path):
    root = group_root(tmp_path, f'{os.getpid()}\n{os.getppid()}\n')
    assert not delegation.exclusive(root)
    (root / 'cgroup.procs').write_text(str(os.getpid()))
    assert delegation.exclusive(root)


@pytest.mark.parametrize('events', ['populated 1\n', '', 'invalid', 'populated 0\nfrozen 0\n'])
def test_exclusive_root_checks_descendant_population(tmp_path, events):
    root = group_root(tmp_path)
    (root / 'manager').mkdir()
    (root / 'manager/cgroup.events').write_text(events)
    assert delegation.exclusive(root) == events.startswith('populated 0')


def test_exclusive_root_missing_state_fails_closed(tmp_path):
    assert not delegation.exclusive(tmp_path)
    group_root(tmp_path)
    (tmp_path / 'manager').mkdir()
    assert not delegation.exclusive(tmp_path)


@pytest.mark.parametrize('inherited_marker', [None, '1', 'different-parent-pid'])
def test_shared_delegated_rollout_root_gets_new_scope(monkeypatch, inherited_marker):
    monkeypatch.setenv('TRELLIS_CHECK_CGROUP', 'delegated')
    if inherited_marker is None:
        monkeypatch.delenv(delegation._ATTEMPTED, raising=False)
    else:
        monkeypatch.setenv(delegation._ATTEMPTED, inherited_marker)
    monkeypatch.setattr(delegation, 'current_root', lambda: Path('/rollout.service'))
    monkeypatch.setattr(delegation, 'delegated', lambda _: True)
    monkeypatch.setattr(delegation, 'exclusive', lambda _: False)
    monkeypatch.setattr(Path, 'exists', lambda _: True)
    calls = []
    monkeypatch.setattr(delegation.os, 'execvpe', lambda *args: calls.append(args))
    delegation.ensure(['/runtime'])
    assert len(calls) == 1
    assert calls[0][2][delegation._ATTEMPTED] == str(os.getpid())
    assert '--scope' in calls[0][1]


def test_same_process_failed_scope_does_not_retry(monkeypatch):
    monkeypatch.setenv('TRELLIS_CHECK_CGROUP', 'delegated')
    monkeypatch.setenv(delegation._ATTEMPTED, str(os.getpid()))
    monkeypatch.setattr(delegation, 'current_root', lambda: Path('/rollout.service'))
    monkeypatch.setattr(delegation, 'delegated', lambda _: True)
    monkeypatch.setattr(delegation, 'exclusive', lambda _: False)
    with pytest.raises(RuntimeError, match='delegation did not provide'):
        delegation.ensure(['/runtime'])


def test_preflight_always_requests_fresh_scope_then_validates_controls(monkeypatch, capsys):
    from trellis.checker import compiler_cgroup
    monkeypatch.setenv('TRELLIS_CHECK_CGROUP', 'delegated')
    calls = []
    monkeypatch.setattr(delegation, 'ensure', lambda args, **kwargs: calls.append((args, kwargs)))
    class Group:
        def close(self): calls.append('closed')
    class Groups:
        root = Path('/new.scope')
        def create(self, limit):
            calls.append(limit)
            return Group()
    monkeypatch.setattr(compiler_cgroup, 'configured', lambda: Groups())
    delegation.preflight()
    assert calls == [([], dict(module='trellis.checker.delegation', fresh=True)), 64 * 2**20, 'closed']
    result = json.loads(capsys.readouterr().out)
    assert result['ready'] and result['runtime_mutations'] is False


def test_preflight_forces_new_scope_even_from_exclusive_service(monkeypatch):
    monkeypatch.setenv('TRELLIS_CHECK_CGROUP', 'delegated')
    monkeypatch.delenv(delegation._ATTEMPTED, raising=False)
    monkeypatch.setattr(delegation, 'current_root', lambda: Path('/owned.service'))
    monkeypatch.setattr(delegation, 'delegated', lambda _: True)
    monkeypatch.setattr(delegation, 'exclusive', lambda _: True)
    monkeypatch.setattr(Path, 'exists', lambda _: True)
    calls = []
    monkeypatch.setattr(delegation.os, 'execvpe', lambda *args: calls.append(args))
    delegation.ensure([], module='trellis.checker.delegation', fresh=True)
    assert calls[0][1][-3:] == [sys.executable, '-m', 'trellis.checker.delegation']
    monkeypatch.setenv(delegation._ATTEMPTED, str(os.getpid()))
    delegation.ensure([], module='trellis.checker.delegation', fresh=True)
    assert len(calls) == 1


def test_preflight_cli_without_delegation_fails_before_runtime_mutation(tmp_path):
    env = dict(os.environ, TRELLIS_CHECK_CGROUP='off', PYTHONDONTWRITEBYTECODE='1')
    result = subprocess.run([sys.executable, '-B', '-m', 'trellis.checker.delegation'],
                            env=env, text=True, capture_output=True)
    assert result.returncode != 0
    assert 'requires TRELLIS_CHECK_CGROUP=delegated' in result.stderr


def test_preflight_preserves_compiler_guard_failure(monkeypatch):
    from trellis.checker import compiler_cgroup
    monkeypatch.setenv('TRELLIS_CHECK_CGROUP', 'delegated')
    monkeypatch.setattr(delegation, 'ensure', lambda *a, **kw: None)
    def refuse(): raise RuntimeError('delegated checker group still has direct processes')
    monkeypatch.setattr(compiler_cgroup, 'configured', refuse)
    with pytest.raises(RuntimeError, match='still has direct processes'):
        delegation.preflight()
