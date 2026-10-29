"""Managed checker launches and unchanged recovery of older mount recipes."""
import json
import os
from pathlib import Path
import sys
import time

import pytest

from trellis.dispatch import confinement

TRUE = str(Path('/usr/bin/true').resolve())


@pytest.fixture(autouse=True)
def clean_modes(monkeypatch):
    for name in ('TRELLIS_COMPILER_BWRAP', 'TRELLIS_COMPILER_DATA_VIEW', 'TRELLIS_MANAGED_AGENT_CHECKS'):
        monkeypatch.delenv(name, raising=False)


def test_native_helper_requires_root_owned_file_and_protected_parents(tmp_path):
    assert confinement.require_native_helper(TRUE) == Path(TRUE)
    for path in ('/', 'relative-bwrap', '/usr/bin/../bin/true', '/usr/bin/missing-trellis-bwrap', tmp_path / 'bwrap'):
        with pytest.raises(confinement.ConfinementError, match='original trusted helper'):
            confinement.require_native_helper(path)
    # A symlink to a root-owned executable is still a replaceable launcher.
    (tmp_path / 'bwrap').symlink_to('/usr/bin/true')
    with pytest.raises(confinement.ConfinementError):
        confinement.require_native_helper(tmp_path / 'bwrap')


def test_native_helper_rejects_unsafe_file_metadata(monkeypatch):
    original = confinement.os.fstat
    from types import SimpleNamespace
    for update in ({'st_uid': 1000}, {'st_mode': 0o100777}, {'st_mode': 0o104755}, {'st_mode': 0o100644}):
        def changed(fd):
            info = original(fd)
            values = dict(st_mode=info.st_mode, st_uid=info.st_uid)
            if confinement.stat.S_ISREG(info.st_mode):
                values.update(update)
            return SimpleNamespace(**values)
        monkeypatch.setattr(confinement.os, 'fstat', changed)
        with pytest.raises(confinement.ConfinementError):
            confinement.require_native_helper(TRUE)
    monkeypatch.setattr(confinement.os, 'fstat', original)
    monkeypatch.setattr(confinement.os, 'getxattr', lambda *args: b'capability')
    with pytest.raises(confinement.ConfinementError, match='file capabilities'):
        confinement.require_native_helper(TRUE)


def test_no_implicit_legacy_fallback_and_preserved_recipe(tmp_path, monkeypatch):
    monkeypatch.setattr(confinement, 'require_native_helper', lambda *args: pytest.fail('new work does not need a host helper'))
    monkeypatch.setattr(confinement, 'require_view', lambda *args: pytest.fail('new work does not need a FUSE view'))
    root = str(tmp_path / 'old-view')
    assert confinement.confinement_recipe(root, None, {}) == dict(kind='managed_commands', root=root)
    assert confinement.confinement_recipe(root, None, {'TRELLIS_COMPILER_BWRAP': '/retired/helper'}) == dict(kind='managed_commands', root=root)
    assert confinement.confinement_recipe(root, root, {}) == root
    with pytest.raises(confinement.ConfinementError, match='different confinement root'):
        confinement.confinement_recipe(root, root + '-different', {})
    native = dict(kind='native_noexec', root=root, helper='/recorded/helper')
    assert confinement.confinement_recipe(root, native, {}) is native
    with pytest.raises(confinement.ConfinementError, match='unknown or different'):
        confinement.confinement_recipe(root, dict(kind='unknown', root=root), {})


def test_managed_setup_needs_no_additional_mount_or_helper(monkeypatch):
    monkeypatch.setenv('TRELLIS_MANAGED_AGENT_CHECKS', '1')
    monkeypatch.setattr(confinement, 'require_native_helper', lambda *args: pytest.fail('unnecessary host dependency'))
    monkeypatch.setattr(confinement, 'require_view', lambda *args: pytest.fail('unnecessary FUSE dependency'))
    assert confinement.require_compiler_confinement() == 'managed_commands'
    monkeypatch.setenv('TRELLIS_MANAGED_AGENT_CHECKS', 'invalid')
    with pytest.raises(confinement.ConfinementError, match='inconsistent'):
        confinement.require_compiler_confinement()
    monkeypatch.delenv('TRELLIS_MANAGED_AGENT_CHECKS')
    with pytest.raises(confinement.ConfinementError, match='host-managed checker setup'):
        confinement.require_compiler_confinement()


def test_native_rewrite_uses_direct_binds_and_retains_guard(tmp_path, monkeypatch):
    monkeypatch.setattr(confinement, 'require_native_helper', lambda path=None: Path('/trusted/bwrap'))
    def forbidden(*args):
        raise AssertionError('native confinement touched a FUSE view')
    monkeypatch.setattr(confinement, 'require_view', forbidden)
    data = tmp_path / 'data'
    data.mkdir()
    command = confinement.restrict_bwrap(['bwrap', '--ro-bind', '/usr', '/usr',
        '--bind', str(data), '/data', '--ro-bind', str(data), '/readonly',
        '--tmpfs', '/hidden', '--proc', '/proc', '/usr/bin/true'],
        native_helper='/trusted/bwrap', executable_roots=['/usr'], scratch=tmp_path / 'guard', compiler='managed')
    assert command[0] == '/trusted/bwrap'
    assert command[1:4] == ['--ro-bind', '/usr', '/usr']
    assert command[4:7] == ['--noexec-bind', str(data), '/data']
    assert command[7:10] == ['--ro-noexec-bind', str(data), '/readonly']
    assert 'compiler_guard.py' in ' '.join(command)
    assert command[-1] == '/usr/bin/true'
    assert ['--unsetenv', 'TRELLIS_COMPILER_BWRAP'] == command[command.index('TRELLIS_COMPILER_BWRAP')-1:command.index('TRELLIS_COMPILER_BWRAP')+1]
    assert '--jitless' in command
    with pytest.raises(confinement.ConfinementError, match='choose native'):
        confinement.restrict_bwrap(['bwrap'], native_helper='/trusted/bwrap', view='/legacy',
            executable_roots=['/usr'], scratch=tmp_path, compiler='managed')


def test_native_none_capability_withholds_checker_tokens(tmp_path, monkeypatch):
    monkeypatch.setattr(confinement, 'require_native_helper', lambda path=None: Path('/trusted/bwrap'))
    command = confinement.restrict_bwrap(['bwrap', '--setenv', 'TRELLIS_CHECKER_TOKEN', 'private-token',
        '/usr/bin/true'], native_helper='/trusted/bwrap', executable_roots=['/usr'],
        scratch=tmp_path, compiler='none')
    assert 'private-token' not in command
    assert command[command.index('TRELLIS_COMPILER_CAPABILITY') + 1] == 'none'


def test_conflicting_enforcement_modes_fail_closed(monkeypatch):
    monkeypatch.setenv('TRELLIS_COMPILER_BWRAP', '/trusted/bwrap')
    monkeypatch.setenv('TRELLIS_COMPILER_DATA_VIEW', '/legacy')
    with pytest.raises(confinement.ConfinementError, match='cannot use native binds'):
        confinement.require_compiler_confinement()


@pytest.mark.parametrize('kind', ['fuse', 'native_noexec', 'managed_commands'])
def test_existing_attempt_keeps_exact_recorded_recipe(tmp_path, monkeypatch, kind):
    from trellis.dispatch.candidate import canonical_json
    from trellis.dispatch.process import AttemptProcess
    from trellis.dispatch import process as module
    command = ['old-bridge']
    payload = {'old': 'payload'}
    root = tmp_path / 'attempt'
    attempt = AttemptProcess(root)
    confinement_root = str(tmp_path / 'view')
    previous = (confinement_root if kind == 'fuse' else
                dict(kind=kind, root=confinement_root, **({'helper': '/recorded/helper'} if kind == 'native_noexec' else {})))
    recipe = canonical_json(dict(command=command, payload=payload, confinement=previous))
    (root / 'recipe.json').write_bytes(recipe)
    (root / 'process.json').write_text(json.dumps({'pid': 12}))
    monkeypatch.setattr(module, 'alive', lambda _: True)
    assert attempt.start_or_adopt(command, payload, confinement=confinement_root,
        environment={'TRELLIS_MANAGED_AGENT_CHECKS': '1'}) == {'pid': 12}
    assert (root / 'recipe.json').read_bytes() == recipe


def test_fresh_attempt_runs_managed_mode_without_old_host_dependencies(tmp_path):
    from trellis.dispatch.process import AttemptProcess
    process = AttemptProcess(tmp_path / 'attempt')
    command = [sys.executable, '-c',
               "import json,os; print(json.dumps({k:os.environ.get(k) for k in "
               "['TRELLIS_MANAGED_AGENT_CHECKS','TRELLIS_COMPILER_BWRAP','TRELLIS_COMPILER_DATA_VIEW']}))"]
    record = process.start_or_adopt(command, {}, confinement=tmp_path / 'unused-legacy-root',
        environment=dict(os.environ, TRELLIS_COMPILER_BWRAP='/not-installed/helper',
                         TRELLIS_COMPILER_DATA_VIEW='/not-mounted/view'))
    try:
        process.permit(record)
        deadline = time.monotonic() + 5
        while process.result() is None and time.monotonic() < deadline:
            time.sleep(.02)
        assert process.result()['returncode'] == 0
        assert json.loads((process.root / 'response.json').read_text()) == {
            'TRELLIS_MANAGED_AGENT_CHECKS': '1', 'TRELLIS_COMPILER_BWRAP': None,
            'TRELLIS_COMPILER_DATA_VIEW': None}
        assert json.loads((process.root / 'recipe.json').read_text())['confinement'] == dict(
            kind='managed_commands', root=str(tmp_path / 'unused-legacy-root'))
        assert not (tmp_path / 'unused-legacy-root').exists()
    finally:
        process.stop_and_reap(record)
