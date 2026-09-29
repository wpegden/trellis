"""Installation eligibility without executing Lean, Lake, Elan, or a proxy."""
import hashlib
import os
from pathlib import Path
from types import SimpleNamespace

import pytest

from trellis import git_trust as gt
from test_git_trust import project, reports, manifest, configuration, stub_git, PIN, URL


@pytest.fixture
def native_install(tmp_path, monkeypatch):
    root = tmp_path / 'native'
    anchors = {}
    for member in gt._AUDITED_NATIVE_INSTALL:
        target = root / member
        target.parent.mkdir(parents=True, exist_ok=True)
        # Synthetic trust anchors exercise identity/layout checks without
        # copying or executing a real toolchain. Subprocess tests separately
        # use the actual audited distribution and unmodified production hashes.
        data = b'fixture, never executable: ' + member.encode()
        target.write_bytes(data)
        if member.startswith('bin/'):
            target.chmod(0o755)
        anchors[member] = hashlib.sha256(data).hexdigest()
    monkeypatch.setattr(gt, '_AUDITED_NATIVE_INSTALL', anchors)
    return root


def native_env(native_install, **extra):
    return {b'PATH': os.fsencode(native_install / 'bin'),
            **{key.encode(): os.fsencode(value) for key, value in extra.items()}}


def check(project, reports, env):
    return gt.preflight_git_packages(project, 'reviewer', reports, env=env)


@pytest.mark.parametrize('override', ['y', 'YES', 't', 'TrUe', 'on', '1'])
@pytest.mark.parametrize('lean', ['', ' \t\r\n', '/missing/lean'])
@pytest.mark.parametrize('sysroot', [None, '', '/missing/sysroot', 'native'])
def test_override_lean_respects_sysroot_precedence(project, reports, native_install, monkeypatch, override, lean, sysroot):
    env = native_env(native_install, LAKE_OVERRIDE_LEAN=override, LEAN=lean)
    if sysroot is not None:
        env[b'LEAN_SYSROOT'] = os.fsencode(native_install) if sysroot == 'native' else os.fsencode(sysroot)
    established = sysroot == 'native'
    calls = stub_git(monkeypatch, [(0, b'other', b''), (128, b'', b'no origin\n')] if established else [])
    before = dict(env)
    result = check(project, reports, env)
    assert env == before
    if established:
        # Lake ignores LEAN when LEAN_SYSROOT is set. Its getGithash failure is
        # caught; it does not turn findLeanInstall? into none (InstallPath:274–286).
        assert result['status'] == 128 and result['outcome'] == 'abort' and len(calls) == 2
    else:
        assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls
        assert 'LAKE_OVERRIDE_LEAN requires' in result['message']


@pytest.mark.parametrize('override', [None, 'n', 'NO', 'f', 'false', 'off', '0', ' true ', 'invalid'])
def test_native_collocated_branch_ignores_lean_overrides_when_not_enabled(project, reports, native_install, monkeypatch, override):
    env = native_env(native_install, LEAN='', LEAN_SYSROOT='/missing', LAKE_HOME='/missing')
    if override is not None:
        env[b'LAKE_OVERRIDE_LEAN'] = override.encode()
    calls = stub_git(monkeypatch, [(0, b'other', b''), (128, b'', b'no origin\n')])
    result = check(project, reports, env)
    assert result['status'] == 128 and result['outcome'] == 'abort' and len(calls) == 2


@pytest.mark.parametrize('selection', ['env', 'empty_env', 'file', 'ancestor_file', 'unreadable_file', 'file_directory'])
def test_unestablished_toolchain_selection_defers(project, reports, native_install, monkeypatch, selection):
    env = native_env(native_install)
    if selection in {'env', 'empty_env'}:
        env[b'ELAN_TOOLCHAIN'] = b'uninstalled:toolchain' if selection == 'env' else b''
    else:
        path = (project.parent if selection == 'ancestor_file' else project) / 'lean-toolchain'
        if selection == 'file_directory':
            path.mkdir()
        else:
            path.write_text('leanprover/lean4:v4.33.0\n')
            if selection == 'unreadable_file':
                path.chmod(0)
    calls = stub_git(monkeypatch, [])
    try:
        result = check(project, reports, env)
        assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls
        assert 'selection is not established' in result['message']
    finally:
        if selection == 'unreadable_file':
            path.chmod(0o600)


@pytest.mark.parametrize('kind', ['proxy_script', 'proxy_binary', 'hardlink_proxy', 'version_script'])
def test_path_proxy_or_version_output_is_not_installation_proof(project, reports, native_install, tmp_path, monkeypatch, kind):
    proxy_bin = tmp_path / 'proxy/bin'
    proxy_bin.mkdir(parents=True)
    proxy = proxy_bin / 'lake'
    if kind == 'hardlink_proxy':
        elan = proxy_bin / 'elan'
        elan.write_bytes(b'ELF-shaped fake elan')
        os.link(elan, proxy)
    elif kind == 'proxy_binary':
        proxy.write_bytes(b'\x7fELF unestablished proxy')
    else:
        proxy.write_text('#!/bin/sh\nprintf "Lake version 5.0.0-src (Lean version 4.33.0)\\n"\n')
    proxy.chmod(0o755)
    (proxy_bin / 'lean').write_bytes(b'also a proxy')
    (proxy_bin / 'lean').chmod(0o755)
    env = native_env(native_install, ELAN_HOME=str(tmp_path / 'elan-home'))
    env[b'PATH'] = os.fsencode(proxy_bin) + b':' + env[b'PATH']
    calls = stub_git(monkeypatch, [])
    result = check(project, reports, env)
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls
    assert 'identity is outside' in result['message']


@pytest.mark.parametrize('field', list(gt._AUDITED_NATIVE_INSTALL))
@pytest.mark.parametrize('condition', ['missing', 'different', 'directory'])
def test_unestablished_native_member_defers(project, reports, native_install, monkeypatch, field, condition):
    member = native_install / field
    member.unlink()
    if condition == 'different':
        member.write_bytes(b'unreviewed runtime')
        member.chmod(0o755)
    elif condition == 'directory':
        member.mkdir()
    calls = stub_git(monkeypatch, [])
    result = check(project, reports, native_env(native_install))
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls


def test_saved_path_and_project_cwd_determine_native_selection(project, reports, native_install, tmp_path, monkeypatch):
    # Do not use the supervisor PATH; a relative payload PATH is relative to the project.
    (project / 'native-bin').symlink_to(native_install / 'bin', target_is_directory=True)
    monkeypatch.setenv('PATH', '/missing')
    monkeypatch.chdir(tmp_path)
    calls = stub_git(monkeypatch, [(0, b'other', b''), (128, b'', b'no origin\n')])
    result = check(project, reports, {b'PATH': b'native-bin'})
    assert result['status'] == 128 and len(calls) == 2


def test_elan_home_does_not_change_proven_direct_native_selection(project, reports, native_install, monkeypatch):
    env = native_env(native_install, ELAN_HOME='/missing', ELAN='')
    calls = stub_git(monkeypatch, [(0, b'other', b''), (128, b'', b'no origin\n')])
    assert check(project, reports, env)['status'] == 128 and len(calls) == 2


@pytest.mark.parametrize('env', [{}, {b'PATH': b''}, {b'PATH': b'/missing'}, {b'PATH': b'\xff'},
                                {b'LD_LIBRARY_PATH': b''}, {b'LD_PRELOAD': b'/missing'}])
def test_unknown_executable_or_loader_input_defers(project, reports, native_install, monkeypatch, env):
    if any(key.startswith(b'LD_') for key in env):
        env = {**native_env(native_install), **env}
    calls = stub_git(monkeypatch, [])
    result = check(project, reports, env)
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls


@pytest.mark.parametrize('root_present', [False, True])
def test_empty_packages_dir_defers_lakes_absolute_package_join(project, reports, native_install, monkeypatch, root_present):
    # Lake's "" / "p" is /p; Python's repo / "" / "p" would be repo/p.
    # Model /p without writing to, reading, or probing a real root checkout.
    (project / 'p').mkdir()
    configuration(project, 'p')
    manifest(project, [dict(name='p', type='git', inherited=False, rev=PIN, url=URL)], packagesDir='')
    observed = []
    original_is_dir = Path.is_dir
    def virtual_root(path):
        if path == Path('/p'):
            observed.append(root_present)
            return root_present
        return original_is_dir(path)
    monkeypatch.setattr(Path, 'is_dir', virtual_root)
    calls = stub_git(monkeypatch, [])
    result = check(project, reports, native_env(native_install))
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls
    assert 'empty manifest packagesDir' in result['message'] and observed == []


@pytest.mark.parametrize('root_present', [False, True])
def test_empty_home_defers_correct_absolute_system_config(project, reports, native_install, monkeypatch, root_present):
    original_stat = os.stat
    looked_at = []
    def virtual_root(path, *args, **kwargs):
        if os.fsencode(path) == b'/.lake/config.toml':
            looked_at.append(path)
            if not root_present:
                raise FileNotFoundError(2, 'virtual absent root config')
            return original_stat(project / 'lakefile.toml')  # Existing malformed input is unestablished.
        return original_stat(path, *args, **kwargs)
    monkeypatch.setattr(os, 'stat', virtual_root)
    calls = stub_git(monkeypatch, [])
    result = check(project, reports, native_env(native_install, HOME=''))
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls
    assert '/.lake/config.toml' in result['message'] and looked_at == []
    assert not (project / '.lake/config.toml').exists()


def test_explicit_absent_config_takes_precedence_over_empty_home(project, reports, native_install, monkeypatch):
    env = native_env(native_install, HOME='', LAKE_CONFIG='absent.toml')
    calls = stub_git(monkeypatch, [(0, b'other', b''), (128, b'', b'no origin\n')])
    assert check(project, reports, env)['status'] == 128 and len(calls) == 2


@pytest.mark.parametrize('member', ['lib/libLake_shared.so', 'lib/libleanshared.so',
                                   'lib/glibc-hwcaps', 'lib/lean/libc.so.6'])
def test_alternate_library_selection_defers(project, reports, native_install, monkeypatch, member):
    (native_install / member).write_bytes(b'shadow runtime')
    calls = stub_git(monkeypatch, [])
    result = check(project, reports, native_env(native_install))
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls
    assert 'library selection is not established' in result['message']


@pytest.mark.parametrize('member', [name for name in gt._AUDITED_NATIVE_INSTALL if name.startswith('lib/')])
@pytest.mark.parametrize('symlink', [False, True])
@pytest.mark.parametrize('noexec', [False, True])
def test_native_library_mount_eligibility_uses_opened_file(project, reports, native_install, monkeypatch,
                                                        member, symlink, noexec):
    library = native_install / member
    if symlink:
        # Model a library symlink into a different mount without mounting or
        # mapping anything. Both executables and every member's bytes stay intact.
        destination = native_install.parent / 'other-mount' / library.name
        destination.parent.mkdir()
        library.rename(destination)
        library.symlink_to(destination)
    library.chmod(0o644)  # Shared-library execute-mode bits are not the criterion.
    target = library.stat()
    assert hashlib.sha256(library.read_bytes()).hexdigest() == gt._AUDITED_NATIVE_INSTALL[member]
    assert all(os.access(native_install / binary, os.X_OK) for binary in ('bin/lake', 'bin/lean'))
    original_fstatvfs = os.fstatvfs
    observed = []
    def mount_flags(fd):
        info = os.fstat(fd)
        if (info.st_dev, info.st_ino) == (target.st_dev, target.st_ino):
            observed.append(fd)
            return SimpleNamespace(f_flag=os.ST_RDONLY | (os.ST_NOEXEC if noexec else 0))
        return original_fstatvfs(fd)
    monkeypatch.setattr(os, 'fstatvfs', mount_flags)
    calls = stub_git(monkeypatch, [] if noexec else [(0, b'other', b''), (128, b'', b'no origin\n')])
    result = check(project, reports, native_env(native_install))
    assert len(observed) == 1
    if noexec:
        assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls
        assert 'noexec mount' in result['message'] and member in result['message']
    else:
        assert result['status'] == 128 and result['outcome'] == 'abort' and len(calls) == 2


@pytest.mark.parametrize('error', [PermissionError('mount lookup denied'), OSError('mount flags unavailable')])
def test_unknown_library_mount_eligibility_defers(project, reports, native_install, monkeypatch, error):
    library = (native_install / 'lib/lean/libLake_shared.so').stat()
    original_fstatvfs = os.fstatvfs
    def mount_flags(fd):
        info = os.fstat(fd)
        if (info.st_dev, info.st_ino) == (library.st_dev, library.st_ino):
            raise error
        return original_fstatvfs(fd)
    monkeypatch.setattr(os, 'fstatvfs', mount_flags)
    calls = stub_git(monkeypatch, [])
    result = check(project, reports, native_env(native_install))
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls
    assert str(error) in result['message']


def test_missing_noexec_flag_support_defers(project, reports, native_install, monkeypatch):
    monkeypatch.delattr(os, 'ST_NOEXEC')
    calls = stub_git(monkeypatch, [])
    result = check(project, reports, native_env(native_install))
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls
    assert 'ST_NOEXEC' in result['message']


def test_slow_native_fingerprinting_defers_before_git(project, reports, native_install, monkeypatch):
    ticks = iter([0.0, 3.0])
    monkeypatch.setattr(gt.time, 'monotonic', lambda: next(ticks))
    calls = stub_git(monkeypatch, [])
    result = check(project, reports, native_env(native_install))
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls
    assert 'identity budget exhausted' in result['message']
