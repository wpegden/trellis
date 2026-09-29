"""Offline regressions: no Lake/Lean/Isabelle, providers, tmux or live sandbox."""
import ast
import json
import os
from pathlib import Path
import subprocess
import sys
import time

import pytest

from git_trust_test_support import clean_install_environment, native_lake_path
from trellis import git_trust as gt
from trellis.config import SandboxConfig
from trellis.sandbox import wrap_command

PIN = 'a' * 40
URL = 'https://example.invalid/mathlib'
SCRIPT = str(Path(gt.__file__).resolve())


@pytest.fixture(autouse=True)
def isolated_lake_system_config(tmp_path, monkeypatch):
    # Child-process tests must not depend on the operator's Lake configuration.
    monkeypatch.setenv('LAKE_CONFIG', str(tmp_path / 'absent-system-config.toml'))
    clean_install_environment(monkeypatch)


@pytest.fixture(autouse=True)
def established_installation_for_package_unit_tests(monkeypatch):
    # Package/config branch tests start after installation eligibility. Dedicated
    # tests in test_git_trust_installation.py exercise the real guard; subprocess
    # tests load the unpatched gate and validate the actual audited installation.
    monkeypatch.setattr(gt, '_require_lake_installation', lambda *args: None)


def metadata(path):
    (path / 'objects').mkdir(parents=True)
    (path / 'HEAD').write_text('ref: refs/heads/main\n')


def configuration(repo, *names):
    (repo / "lakefile.toml").write_text(
        'name = "root"\n' + ''.join(f'[[require]]\nname = "{name}"\n' for name in names))


def manifest(repo, entries=None, **extra):
    data = dict(version='1.2.0', packagesDir='.lake/packages', packages=entries if entries is not None else [
        dict(name='mathlib', type='git', inherited=False, rev=PIN, url=URL)])
    data.update(extra)
    (repo / 'lake-manifest.json').write_text(json.dumps(data))


@pytest.fixture
def project(tmp_path):
    repo = tmp_path / 'repo with spaces'
    metadata(repo / '.git')
    metadata(repo / '.lake/packages/mathlib/.git')
    manifest(repo)
    configuration(repo, "mathlib")
    return repo


@pytest.fixture
def reports(tmp_path):
    path = tmp_path / 'reports'
    path.mkdir()
    return path


def run_check(project, reports, **kwargs):
    return gt.preflight_git_packages(project, 'reviewer', reports, env={}, **kwargs)


def stub_git(monkeypatch, results):
    calls = []
    pending = iter(results)
    def run(command, **kwargs):
        calls.append((command, kwargs))
        item = next(pending)
        if isinstance(item, Exception):
            raise item
        status, stdout, stderr = item
        return subprocess.CompletedProcess(command, status, stdout, stderr)
    monkeypatch.setattr(gt.subprocess, 'run', run)
    return calls


def test_matching_head_never_asks_for_origin(project, reports, monkeypatch):
    calls = stub_git(monkeypatch, [(0, f' \t{PIN}\r\n'.encode(), b'warning\r\n')])
    result = run_check(project, reports)
    assert result['status'] == 0 and result['outcome'] == 'pass'
    assert [c[0][3:] for c in calls] == [['rev-parse', '--verify', '--end-of-options', 'HEAD']]


@pytest.mark.parametrize(('head_status', 'head'), [(0, b'other\n'), (128, b'')])
@pytest.mark.parametrize(('status', 'error', 'cause'), [
    (128, b'fatal: detected dubious ownership\n', 'dubious ownership'),
    (127, b'git: command not found\n', 'command not found'),
    (128, b'fatal: not a git repository\n', 'not a repository'),
])
def test_abort_only_after_remote_failure(project, reports, monkeypatch, head_status, head, status, error, cause):
    calls = stub_git(monkeypatch, [(head_status, head, b'HEAD stderr'), (status, b'', error)])
    result = run_check(project, reports)
    assert result['status'] == status and result['outcome'] == 'abort'
    assert cause in result['message'] and 'mathlib' in result['message']
    assert calls[1][0][3:] == ['remote', 'get-url', 'origin']
    assert (reports / result['checks'][1]['stderr_file']).read_bytes() == error


@pytest.mark.parametrize('head_status', [0, 128])
def test_head_miss_or_failure_with_same_remote_passes(project, reports, monkeypatch, head_status):
    stub_git(monkeypatch, [(head_status, b'other', b'HEAD error'), (0, URL.encode() + b'\n', b'')])
    assert run_check(project, reports)['status'] == 0


def test_remote_mismatch_aborts_even_when_git_succeeds(project, reports, monkeypatch):
    stub_git(monkeypatch, [(0, b'other', b''), (0, b'https://example.invalid/other', b'')])
    result = run_check(project, reports)
    assert result['status'] == 1 and result['outcome'] == 'abort'
    assert result['checks'][1]['status'] == 0
    assert 'URL differs' in result['message']


def test_url_map_is_json_and_overrides_only_update_branch(project, reports, monkeypatch):
    calls = stub_git(monkeypatch, [(0, b'other', b''), (0, b'override', b'')])
    result = gt.preflight_git_packages(project, 'reviewer', reports,
        env={b'LAKE_PKG_URL_MAP': b'{"mathlib":"override"}'})
    assert result['status'] == 0 and len(calls) == 2


def test_local_remote_realpath_uses_lake_cwd(project, reports, monkeypatch):
    (project / 'upstream').mkdir()
    (project / 'alias').symlink_to(project / 'upstream', target_is_directory=True)
    manifest(project, [dict(name='mathlib', type='git', inherited=False, rev=PIN, url='alias')])
    calls = stub_git(monkeypatch, [(0, b'other', b''), (0, b'upstream\n', b'')])
    assert run_check(project, reports)['status'] == 0
    assert calls[1][1]['cwd'] == project / '.lake/packages/mathlib'


def test_unused_checkout_and_path_dependency_are_not_probed(project, reports, monkeypatch):
    metadata(project / '.lake/packages/unused/.git')
    metadata(project / '.lake/packages/path_dep/.git')
    manifest(project, [dict(name='path_dep', type='path', dir='.lake/packages/path_dep', inherited=False)])
    configuration(project, "path_dep")
    calls = stub_git(monkeypatch, [])
    assert run_check(project, reports)['status'] == 0 and calls == []


@pytest.mark.parametrize('text', [None, '{broken', '{}', '{"version":"99.0.0"}',
    '{"version":"1.2.0","packagesDir":".lake/packages","packages":[{"type":"git"}]}'])
def test_absent_or_invalid_manifest_passes_through(project, reports, monkeypatch, text):
    path = project / 'lake-manifest.json'
    path.unlink() if text is None else path.write_text(text)
    calls = stub_git(monkeypatch, [])
    result = run_check(project, reports)
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls


def test_unmaterialized_package_passes_without_git(project, reports, monkeypatch):
    manifest(project, [dict(name='new', type='git', inherited=False, rev=PIN, url=URL)])
    configuration(project, 'new')
    calls = stub_git(monkeypatch, [])
    assert run_check(project, reports)['status'] == 0 and not calls


def test_stderr_bytes_whitespace_and_os_errors_are_separate(project, reports, monkeypatch):
    raw = b'\xfffatal: not a git repository\r\n \t\n\n'
    stub_git(monkeypatch, [(128, b'', raw), (128, b'', raw)])
    result = run_check(project, reports)
    assert result['status'] == 128
    assert all((reports / c['stderr_file']).read_bytes() == raw for c in result['checks'])
    gt._write_result(reports / 'result.json', result)
    command = [sys.executable, '-I', '-S', SCRIPT, '--gate', str(project), 'reviewer', '', str(reports), 'true']
    status, detail = gt.launch_failure(command)
    assert status == 128 and '\ufffd' in detail  # Only the presentation is decoded.
    assert (reports / '001-remote.stderr').read_bytes() == raw


def test_missing_git_is_a_remote_spawn_failure(project, reports, monkeypatch):
    calls = stub_git(monkeypatch, [FileNotFoundError(2, 'missing git'), FileNotFoundError(2, 'missing git')])
    result = run_check(project, reports)
    assert len(calls) == 2 and result['status'] == 127
    assert all((reports / c['stderr_file']).read_bytes() == b'' for c in result['checks'])
    assert 'missing git' in result['checks'][1]['os_error']


def test_timeout_is_unknown_not_a_false_lake_deletion(project, reports, monkeypatch):
    calls = stub_git(monkeypatch, [subprocess.TimeoutExpired(['git'], 10, stderr=b'partial\r\n')])
    result = run_check(project, reports)
    assert result['status'] == 0 and result['outcome'] == 'inconclusive' and len(calls) == 1
    assert (reports / '000-rev-parse.stderr').read_bytes() == b'partial\r\n'


def pairs(env):
    return [(env[f'GIT_CONFIG_KEY_{i}'], env[f'GIT_CONFIG_VALUE_{i}']) for i in range(int(env['GIT_CONFIG_COUNT']))]


def test_burst_entries_are_exact_and_compose(project):
    env = dict(PATH='/bin', GIT_CONFIG_COUNT='2', GIT_CONFIG_KEY_0='safe.directory',
               GIT_CONFIG_VALUE_0='', GIT_CONFIG_KEY_1='core.quotePath', GIT_CONFIG_VALUE_1='false')
    original = dict(env)
    gt.inject_burst_git_safe_directories(env, project)
    assert pairs(env) == [('safe.directory', ''), ('core.quotePath', 'false'),
                         ('safe.directory', str(project)), ('safe.directory', str(project / '.lake/packages/mathlib'))]
    assert all(env[k] == v for k, v in original.items() if k != 'GIT_CONFIG_COUNT')


@pytest.mark.parametrize('kind', ['external_alias', 'vendor_alias', 'parent_alias', 'external_gitfile', 'writable_gitfile', 'missing_gitfile'])
def test_strict_burst_admission_rejects_noncanonical_layouts(project, tmp_path, kind):
    package = project / '.lake/packages/extra'
    if kind.endswith('alias'):
        target = {'external_alias': tmp_path / 'outside', 'vendor_alias': project / 'vendor/extra',
                  'parent_alias': project}[kind]
        if target != project:
            metadata(target / '.git')
        package.symlink_to(target, target_is_directory=True)
    else:
        target = {'external_gitfile': tmp_path / 'outside-git',
                  'writable_gitfile': project / '.lake/packages/mathlib/.lake/build/git',
                  'missing_gitfile': project / '.git/modules/missing'}[kind]
        if kind != 'missing_gitfile':
            metadata(target)
        package.mkdir()
        (package / '.git').write_text(f'gitdir: {target}\n')
    env = {}
    gt.inject_burst_git_safe_directories(env, project)
    assert len(pairs(env)) == 2
    assert all(v in {str(project), str(project / '.lake/packages/mathlib')} for _, v in pairs(env))


def test_internal_protected_gitfile_is_admitted(project):
    package = project / '.lake/packages/extra'
    package.mkdir()
    target = project / '.git/modules/extra'
    metadata(target)
    (package / '.git').write_text(f'gitdir: {target}\n')
    env = {}
    gt.inject_burst_git_safe_directories(env, project)
    assert ('safe.directory', str(package)) in pairs(env)


@pytest.mark.parametrize('config', [{}, {'GIT_CONFIG_COUNT': ''}, {'GIT_CONFIG_COUNT': '1'},
    {'GIT_CONFIG_COUNT': '1', 'GIT_CONFIG_KEY_0': 'safe.directory', 'GIT_CONFIG_VALUE_0': ''},
    {'GIT_CONFIG_COUNT': '-1'}, {'GIT_CONFIG_COUNT': 'invalid'}])
def test_managed_helper_matches_baseline_including_legacy_layouts(project, tmp_path, config):
    source = subprocess.check_output(['git', 'show', 'e8223110:trellis/atomic_actions/observations.py'], text=True)
    tree = ast.parse(source)
    func = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == '_inject_git_safe_directories')
    namespace = {'Path': Path}
    exec(compile(ast.Module(body=[func], type_ignores=[]), '<baseline>', 'exec'), namespace)
    outside = tmp_path / 'outside'
    metadata(outside / '.git')
    (project / '.lake/packages/alias').symlink_to(outside, target_is_directory=True)
    repo_alias = tmp_path / 'repo-alias'
    repo_alias.symlink_to(project, target_is_directory=True)
    states = []
    for helper in (namespace['_inject_git_safe_directories'], gt._inject_git_safe_directories):
        env = dict(config)
        error = None
        try:
            helper(env, repo_alias)
        except Exception as exc:
            error = (type(exc), str(exc))
        states.append((env, error))
    assert states[0] == states[1]


@pytest.fixture
def isolated_sandbox(monkeypatch):
    monkeypatch.setattr('trellis.sandbox.bwrap_available', lambda: True)
    monkeypatch.setattr('trellis.sandbox._host_extra_readonly_paths', lambda: [])
    monkeypatch.setattr('trellis.sandbox._env_extra_readonly_paths', lambda: [])
    monkeypatch.setattr('trellis.sandbox._reviewer_source_snapshot', lambda: None)
    monkeypatch.setattr('trellis.sandbox.host_runtime_readonly_roots', lambda **kw: [])
    for key in tuple(os.environ):
        if key.startswith(('TRELLIS_', 'GIT_CONFIG_')):
            monkeypatch.delenv(key)


@pytest.mark.parametrize('role', ['worker', 'reviewer', 'verifier', 'stuck_math_audit'])
def test_shared_wrapper_composes_pairs_without_environment_changes(project, tmp_path, monkeypatch, isolated_sandbox, role):
    monkeypatch.setenv('GIT_CONFIG_COUNT', '1')
    monkeypatch.setenv('GIT_CONFIG_KEY_0', 'core.quotePath')
    monkeypatch.setenv('GIT_CONFIG_VALUE_0', 'false')
    before = dict(os.environ)
    inner = ['provider', '', '$(literal)', 'space value']
    argv = wrap_command(inner, sandbox=SandboxConfig(), work_dir=project, burst_home=tmp_path / 'home', role=role)
    assigned = {argv[i+1]: argv[i+2] for i,x in enumerate(argv) if x == '--setenv'}
    assert pairs(assigned)[0] == ('core.quotePath', 'false') and len(pairs(assigned)) == 3
    assert dict(os.environ) == before and argv[-4:] == inner
    report = gt.report_directory(argv)
    assert report.parent == tmp_path / 'home/.trellis/git-preflight'
    assert report.name.startswith('launch-') and not report.exists()
    assert not (tmp_path / 'home').exists()
    again = wrap_command(inner, sandbox=SandboxConfig(), work_dir=project, burst_home=tmp_path / 'home', role=role)
    assert gt.report_directory(again) != report
    assert '--clearenv' not in argv
    assert any(argv[i:i+3] == ['--ro-bind', str(project), str(project)] for i in range(len(argv)))
    assert not any(argv[i:i+2] == ['--bind', str(project / '.lake/packages')] for i in range(len(argv)))
    assert not any(argv[i:i+2] == ['--bind', str(project / '.git')] for i in range(len(argv)))


@pytest.mark.parametrize('role', ['grunt', 'lake_compiler', 'rust_witness_artifact', 'source_adaptation_worker', 'source_adaptation_reviewer', 'source_adaptation_checker'])
def test_excluded_roles_get_no_gate(project, tmp_path, isolated_sandbox, role):
    argv = wrap_command(['true'], sandbox=SandboxConfig(), work_dir=project, burst_home=tmp_path / 'home', role=role)
    assert gt.report_directory(argv) is None and 'GIT_CONFIG_COUNT' not in argv


def test_missing_interpreter_fails_loudly_before_launch(project, tmp_path, isolated_sandbox, monkeypatch):
    monkeypatch.setattr('trellis.sandbox.sys.executable', str(tmp_path / 'missing-python'))
    with pytest.raises(RuntimeError, match='bootstrap interpreter is missing'):
        wrap_command(['true'], sandbox=SandboxConfig(), work_dir=project, burst_home=tmp_path / 'home')


def test_interpreter_outside_mounted_roots_is_rejected(tmp_path, monkeypatch):
    from trellis.sandbox import _git_preflight_interpreter
    interpreter = tmp_path / 'python3'
    interpreter.write_text('fake')
    interpreter.chmod(0o755)
    monkeypatch.setattr('trellis.sandbox.sys.executable', str(interpreter))
    with pytest.raises(RuntimeError, match='read-only mounted'):
        _git_preflight_interpreter([Path('/usr')])


def gate(project, reports, command, path=''):
    return [sys.executable, '-I', '-S', SCRIPT, '--gate', str(project), 'reviewer', path, str(reports), *command]


@pytest.mark.parametrize('supervised', [False, True])
@pytest.mark.parametrize('storage', ['absent', 'blocked_parent'])
def test_lazy_report_storage_preserves_payload_environment(project, tmp_path, supervised, storage):
    (project / 'lake-manifest.json').unlink()  # The payload needs no toolchain.
    home = tmp_path / 'uncreated-home'
    if storage == 'blocked_parent':
        home.write_bytes(b'not a directory')
    report = home / '.trellis/git-preflight/launch-fixture'
    command = gate(project, report, ['/usr/bin/env', '-0'])
    if supervised:
        command = gt.supervise_git_launch(command)
    env = {b'PATH': b'/usr/bin:/bin', b'HOME': os.fsencode(home), b'LANG': b'C',
           b'PYTHONHOME': b'/missing', b'PYTHONPATH': b'/ignored', b'RAW': b'\xff', b'EMPTY': b''}
    result = subprocess.run(command, env=env, capture_output=True)
    assert result.returncode == 0 and result.stderr == b''
    assert dict(item.split(b'=', 1) for item in result.stdout.split(b'\0') if item) == env
    if storage == 'absent':
        assert json.loads((report / 'result.json').read_bytes())['status'] == 0
        assert report.stat().st_mode & 0o777 == 0o700
        if supervised:
            assert json.loads((report / 'launch.json').read_bytes())['status'] == 0
    else:
        assert home.read_bytes() == b'not a directory' and not report.exists()


@pytest.mark.parametrize('failure', ['mkdir', 'stderr', 'publish'])
def test_gate_storage_errors_defer_and_exec_original_payload(project, reports, monkeypatch, failure):
    env = {b'PATH': b'/original', b'RAW': b'\xff', b'EMPTY': b''}
    command = ['payload', '', 'space value', '$(literal)']
    monkeypatch.setattr(sys, 'argv', gate(project, reports, command)[3:])
    monkeypatch.setattr(gt, 'original_environment', lambda: env)
    calls = stub_git(monkeypatch, [] if failure == 'mkdir' else [(0, b'other', b''), (128, b'', b'no origin')])
    if failure == 'mkdir':
        original = Path.mkdir
        def denied(path, *args, **kwargs):
            if path == reports:
                raise PermissionError('report mkdir denied')
            return original(path, *args, **kwargs)
        monkeypatch.setattr(Path, 'mkdir', denied)
    elif failure == 'stderr':
        original = Path.write_bytes
        def denied(path, data):
            if path.suffix == '.stderr':
                raise OSError('stderr disk full')
            return original(path, data)
        monkeypatch.setattr(Path, 'write_bytes', denied)
    else:
        original = gt._write_result
        def denied(path, result):
            if result['status']:
                raise OSError('abort report publication failed')
            return original(path, result)
        monkeypatch.setattr(gt, '_write_result', denied)
    class PayloadExec(BaseException):
        pass
    seen = []
    def execute(executable, argv, actual_env):
        seen.append((executable, argv, actual_env))
        raise PayloadExec
    monkeypatch.setattr(os, 'execvpe', execute)
    with pytest.raises(PayloadExec):
        gt.main()
    assert seen == [('payload', command, env)]
    assert len(calls) == {'mkdir': 0, 'stderr': 1, 'publish': 2}[failure]
    result = json.loads((reports / 'result.json').read_bytes())
    assert result['outcome'] == 'skipped' and result['status'] == 0 and result['checks'] == []
    assert 'report storage unavailable' in result['message']
    assert gt.launch_failure(gate(project, reports, command)) is None


def test_recorder_unwritable_stderr_execs_original_command_with_skipped_report(project, reports, monkeypatch):
    (reports / 'bootstrap.stderr').mkdir()
    command = gate(project, reports, ['payload', ''])
    outer = gt.supervise_git_launch(command)
    env = {b'RAW': b'\xff', b'EMPTY': b''}
    monkeypatch.setattr(sys, 'argv', outer[3:])
    monkeypatch.setattr(gt, 'original_environment', lambda: env)
    class PayloadExec(BaseException):
        pass
    seen = []
    def execute(executable, argv, actual_env):
        seen.append((executable, argv, actual_env))
        raise PayloadExec
    monkeypatch.setattr(os, 'execvpe', execute)
    with pytest.raises(PayloadExec):
        gt.main()
    assert seen == [(command[0], command, env)]
    assert json.loads((reports / 'result.json').read_bytes())['outcome'] == 'skipped'


@pytest.mark.parametrize('status', [0, 73])
def test_recorder_publication_failure_retains_payload_status_without_relaunch(project, reports, monkeypatch, status):
    command = gate(project, reports, ['payload'])
    outer = gt.supervise_git_launch(command)
    env = {b'PATH': b'/original'}
    monkeypatch.setattr(sys, 'argv', outer[3:])
    monkeypatch.setattr(gt, 'original_environment', lambda: env)
    calls = []
    def run(argv, **kwargs):
        calls.append((argv, kwargs['env']))
        return status
    monkeypatch.setattr(gt.subprocess, 'call', run)
    def denied(*args):
        raise PermissionError('cannot publish launch status')
    monkeypatch.setattr(gt, '_write_result', denied)
    monkeypatch.setattr(os, 'execvpe', lambda *a: pytest.fail('payload already ran'))
    with pytest.raises(SystemExit) as exc:
        gt.main()
    assert exc.value.code == status and calls == [(command, env)]


def test_fresh_c_locale_and_python_environment_do_not_leak(project, reports):
    (project / 'lake-manifest.json').unlink()  # No tools required for the environment regression.
    env = {b'PATH': b'/usr/bin:/bin', b'LANG': b'C', b'PYTHONHOME': b'/does/not/exist',
           b'PYTHONPATH': b'/do/not/import', b'RAW': b'\xff', b'EMPTY': b''}
    command = gate(project, reports, ['/usr/bin/env', '-0'])
    result = subprocess.run(command, env=env, capture_output=True)
    assert result.returncode == 0, result.stderr
    actual = dict(item.split(b'=', 1) for item in result.stdout.split(b'\0') if item)
    assert actual == env and b'LC_CTYPE' not in actual
    # The outside recorder must preserve the same bytes too.
    result = subprocess.run(gt.supervise_git_launch(command), env=env, capture_output=True)
    assert result.returncode == 0
    assert dict(item.split(b'=', 1) for item in result.stdout.split(b'\0') if item) == env


def test_empty_argv_and_slow_success_still_launch(project, reports, tmp_path):
    git = tmp_path / 'git'
    git.write_text(f'#!/bin/sh\n/bin/sleep 0.1\nprintf "%s\\n" {PIN}\n')
    git.chmod(0o755)
    command = gate(project, reports, ['/usr/bin/printf', '<%s>\\n', '', 'space value', '$(literal)'], path=native_lake_path(tmp_path))
    env = dict(os.environ, PATH='/wrong/path')
    result = subprocess.run(command, env=env, capture_output=True)
    assert result.returncode == 0 and result.stdout == b'<>\n<space value>\n<$(literal)>\n'
    record = json.loads((reports / 'result.json').read_bytes())
    assert len(record['checks']) == 1 and record['status'] == 0


def test_outer_recorder_retains_bootstrap_status_and_exact_stderr(project, reports):
    command = gate(project, reports, ['true'])
    # Simulate a bootstrap exec error, before the gate has started.
    outer = gt.supervise_git_launch(command)
    outer[6:] = ['/bin/sh', '-c', 'printf "bootstrap failure\\r\\n \\t" >&2; exit 73']
    result = subprocess.run(outer, capture_output=True)
    assert result.returncode == 73
    assert (reports / 'bootstrap.stderr').read_bytes() == b'bootstrap failure\r\n \t'
    assert gt.launch_failure(command)[0] == 73


@pytest.mark.parametrize('layout', ['symbolic', 'detached', 'parent_discovery'])
def test_real_git_pinned_checkout_with_no_origin_passes(tmp_path, layout):
    repo = tmp_path / 'repo'
    package = repo / '.lake/packages/mathlib'
    package.mkdir(parents=True)
    reports = tmp_path / 'reports'
    reports.mkdir()
    env = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}
    env.update(GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL='/dev/null',
               GIT_AUTHOR_NAME='Fixture', GIT_AUTHOR_EMAIL='fixture@example.invalid',
               GIT_COMMITTER_NAME='Fixture', GIT_COMMITTER_EMAIL='fixture@example.invalid')
    git_root = repo if layout == 'parent_discovery' else package
    subprocess.run(['git', 'init', '-q', str(git_root)], env=env, check=True)
    tree = subprocess.check_output(['git', '-C', str(git_root), 'hash-object', '-t', 'tree', '-w', '--stdin'], input=b'', env=env).strip()
    commit = subprocess.check_output(['git', '-C', str(git_root), 'commit-tree', tree.decode(), '-m', 'fixture'], env=env).decode().strip()
    subprocess.run(['git', '-C', str(git_root), 'update-ref', 'HEAD', commit], env=env, check=True)
    if layout == 'detached':
        subprocess.run(['git', '-C', str(git_root), 'checkout', '--detach', '-q', commit], env=env, check=True)
        assert (git_root / '.git/HEAD').read_text().strip() == commit
    if layout == 'parent_discovery':
        assert not (package / '.git').exists()
    configuration(repo, 'mathlib')
    manifest(repo, [dict(name='mathlib', type='git', inherited=False, rev=commit, url=URL)])
    before = {p: (p.read_bytes(), p.stat().st_mtime_ns) for p in repo.rglob('*') if p.is_file()}
    result = gt.preflight_git_packages(repo, 'reviewer', reports, env={os.fsencode(k): os.fsencode(v) for k,v in env.items()})
    assert result['status'] == 0 and len(result['checks']) == 1
    assert before == {p: (p.read_bytes(), p.stat().st_mtime_ns) for p in repo.rglob('*') if p.is_file()}
    manifest(repo)  # Different pin + still no origin: Lake would delete.
    result = gt.preflight_git_packages(repo, 'reviewer', reports, env={os.fsencode(k): os.fsencode(v) for k,v in env.items()})
    assert result['outcome'] == 'abort' and result['checks'][1]['status'] != 0


FRAGMENTS = ['verifier/correspondence/07_scratchpad.md', 'stuck_math_audit/common/04_scratchpad.md',
             'review/common/29_stuck_math_audit.md', 'review/common/29_need_input_auditor.md',
             'worker/proof_formalization/10_operational_guidance.md']


@pytest.mark.parametrize('fragment', FRAGMENTS)
def test_note_renders_once_and_excludes_isabelle(fragment):
    from trellis.runtime.bridge_prompts import _render_prompt_fragment
    context = dict(repo_path='/repo', correspondence_scratch_path='/corr', stuck_math_audit_scratch_path='/audit', stuck_math_audit_json='{}')
    rendered = _render_prompt_fragment(fragment, context, backend='lean')
    assert rendered.count('$HOME/.trellis/git-preflight/') == 1
    assert 'label cached-artifact probes advisory until the required current-source verification succeeds' in rendered
    assert '{{' not in rendered
    assert '.trellis/git-preflight/' not in _render_prompt_fragment(fragment, context, backend='isabelle_hol')


@pytest.mark.parametrize('fragment', ['pv/verifier/correspondence/07_scratchpad.md', 'pv/stuck_audit/04_scratchpad.md',
    'pv/review/29_stuck_math_audit.md', 'pv/review/29_need_input_auditor.md'])
def test_pv_guidance_is_unchanged(fragment):
    from trellis.runtime.bridge_prompts import _render_prompt_fragment
    text = _render_prompt_fragment(fragment, dict(repo_path='/repo', correspondence_scratch_path='/corr',
        stuck_math_audit_scratch_path='/audit', stuck_math_audit_json='{}'))
    assert 'invoke `lake` against' in text and '.trellis/git-preflight/' not in text


def test_shipped_implementation_report_is_unchanged():
    assert Path('IMPLEMENTATION.md').read_bytes() == subprocess.check_output(['git', 'show', 'e8223110:IMPLEMENTATION.md'])


def test_isabelle_launch_does_not_gain_lake_gate(project, tmp_path, isolated_sandbox, monkeypatch):
    (project / 'trellis.config.json').write_text('{"workflow":{"default_target":"isabelle_hol"}}')
    monkeypatch.setattr('trellis.sandbox.worker_isabelle_home', lambda: None)
    monkeypatch.setattr('trellis.sandbox.worker_isabelle_home_user', lambda: None)
    argv = wrap_command(['true'], sandbox=SandboxConfig(), work_dir=project, burst_home=tmp_path / 'home', role='reviewer')
    assert gt.report_directory(argv) is None and 'GIT_CONFIG_COUNT' not in argv


@pytest.mark.parametrize('value', ['bad', '-1'])
def test_burst_count_validation_is_explicit_and_atomic(project, value):
    env = {'GIT_CONFIG_COUNT': value}
    with pytest.raises(ValueError, match='GIT_CONFIG_COUNT'):
        gt.inject_burst_git_safe_directories(env, project)
    assert env == {'GIT_CONFIG_COUNT': value}


def test_burst_trust_never_generates_a_wildcard(tmp_path):
    repo = tmp_path / '*'
    repo.mkdir()
    env = {}
    with pytest.raises(ValueError, match='exact Git trust'):
        gt.inject_burst_git_safe_directories(env, repo)
    assert env == {}


def test_duplicate_manifest_name_uses_last_entry(project, reports, monkeypatch):
    manifest(project, [dict(name='mathlib', type='git', inherited=False, url=URL, rev='different'),
                       dict(name='mathlib', type='git', inherited=False, url=URL, rev=PIN)])
    calls = stub_git(monkeypatch, [(0, PIN.encode(), b'')])
    assert run_check(project, reports)['status'] == 0 and len(calls) == 1


def test_query_budget_exhaustion_is_inconclusive_without_git(project, reports, monkeypatch):
    calls = stub_git(monkeypatch, [])
    result = run_check(project, reports, budget=0)
    assert result['status'] == 0 and result['outcome'] == 'inconclusive' and not calls


@pytest.mark.parametrize('url_map', [b'bad json', b'[]', b'{"mathlib":false}', b'{"":"url"}'])
def test_invalid_lake_url_map_defers_without_false_abort(project, reports, monkeypatch, url_map):
    calls = stub_git(monkeypatch, [])
    result = gt.preflight_git_packages(project, 'reviewer', reports, env={b'LAKE_PKG_URL_MAP':url_map})
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls


@pytest.mark.parametrize('suffix', [',"unused":NaN', ',"unused":"\\ud800"'])
def test_non_json_manifest_extensions_do_not_cause_veto(project, reports, monkeypatch, suffix):
    path = project / 'lake-manifest.json'
    path.write_text(path.read_text()[:-1] + suffix + '}')
    calls = stub_git(monkeypatch, [])
    result = run_check(project, reports)
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls


@pytest.mark.parametrize('name', ['HEAD', 'objects', 'config'])
def test_metadata_alias_into_writable_overlay_is_not_admitted(project, name):
    package = project / '.lake/packages/extra'
    metadata(package / '.git')
    writable = package / '.lake/build'
    writable.mkdir(parents=True)
    original = package / '.git' / name
    if original.is_dir():
        original.rmdir()
        target = writable / name
        target.mkdir()
    else:
        original.unlink(missing_ok=True)
        target = writable / name
        target.write_text('ref: refs/heads/main\n')
    original.symlink_to(target)
    env = {}
    gt.inject_burst_git_safe_directories(env, project)
    assert ('safe.directory', str(package)) not in pairs(env)


def git_entry(name='mathlib', **extra):
    return dict(dict(name=name, type='git', inherited=False, rev=PIN, url=URL), **extra)


@pytest.mark.parametrize('token', ['1e18446744073709551617', '0e18446744073709551617',
                                 '1.25', '1e2', '-0.0', '7'])
@pytest.mark.parametrize('location', ['manifest_root', 'manifest_selected', 'manifest_unused',
                                    'override_root', 'override_selected', 'override_unused'])
def test_entire_manifest_and_override_reject_float_tokens(project, reports, monkeypatch, token, location):
    # Every location is ignored by the schema, but Lean parses the whole JSON
    # document before materialization, including nested data in unused entries.
    # Keep an integer control with the same shape and a confirmed deletion.
    document = dict(version=7, packages=[git_entry(), git_entry('unused')])
    if location.startswith('manifest_'):
        document['packagesDir'] = '.lake/packages'
        path = project / 'lake-manifest.json'
    else:
        path = project / '.lake/package-overrides.json'
    target = (document if location.endswith('_root') else
              document['packages'][1 if location.endswith('_unused') else 0])
    target['ignored'] = {'nested': ['NUMBER_TOKEN']}
    path.write_text(json.dumps(document).replace('"NUMBER_TOKEN"', token))
    is_integer = token == '7'
    calls = stub_git(monkeypatch, [(0, b'other', b''), (128, b'', b'no origin\n')] if is_integer else [])
    result = run_check(project, reports)
    if is_integer:
        assert result['status'] == 128 and result['outcome'] == 'abort' and len(calls) == 2
    else:
        assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls
        assert 'JSON floating/exponent token' in result['message']


@pytest.mark.parametrize('dependencies', [[], ['mathlib']])
def test_stale_unused_entry_still_in_manifest_is_never_probed(project, reports, monkeypatch, dependencies):
    metadata(project / '.lake/packages/old/.git')
    manifest(project, [git_entry(), git_entry('old', rev='stale')])
    configuration(project, *dependencies)
    calls = stub_git(monkeypatch, [(0, PIN.encode(), b'')] if dependencies else [])
    result = run_check(project, reports)
    assert result['status'] == 0 and len(calls) == len(dependencies)
    assert all(c[0][2].endswith('/mathlib') for c in calls)


def test_only_first_reverse_order_materialization_is_proven(project, reports, monkeypatch):
    # The first dependency to load may have invalid/executable configuration.
    # Neither its siblings nor transitive manifest entries are established yet.
    entries = [git_entry(f'stale{i}', rev='other') for i in range(40)] + [git_entry()]
    for entry in entries[:-1]:
        metadata(project / '.lake/packages' / entry['name'] / '.git')
    manifest(project, entries)
    configuration(project, *[e['name'] for e in entries])
    calls = stub_git(monkeypatch, [(0, PIN.encode(), b'')])
    result = run_check(project, reports)
    assert result['status'] == 0 and len(calls) == 1
    assert 'remaining traversal deferred' in result['selection']


def test_existing_root_name_is_reused_before_first_materialization(project, reports, monkeypatch):
    configuration(project, 'mathlib', 'root')
    calls = stub_git(monkeypatch, [(0, PIN.encode(), b'')])
    assert run_check(project, reports)['status'] == 0 and len(calls) == 1


@pytest.mark.parametrize('kind', ['path', 'git_pin', 'git_url', 'canonical_last_entry'])
def test_workspace_overrides_replace_manifest_before_selection(project, reports, monkeypatch, kind):
    name = 'foo.1' if kind == 'canonical_last_entry' else 'mathlib'
    if name != 'mathlib':
        metadata(project / '.lake/packages' / name / '.git')
    manifest(project, [git_entry(name, rev='obsolete', url='obsolete')], lakeDir='unused-manifest-dir')
    configuration(project, name)
    if kind == 'path':
        entries = [dict(name=name, type='path', inherited=False, dir='local')]
        results = []
    elif kind == 'git_url':
        entries = [git_entry(name, rev='new-pin', url='override-url')]
        results = [(0, b'different', b''), (0, b'override-url', b'')]
    elif kind == 'canonical_last_entry':
        entries = [git_entry('foo.001', rev='wrong'), git_entry('foo.01')]
        results = [(0, PIN.encode(), b'')]
    else:
        entries = [git_entry(name)]
        results = [(0, PIN.encode(), b'')]
    # Partial manifest requires a version, but no root name / packagesDir.
    (project / '.lake/package-overrides.json').write_text(json.dumps(dict(version='1.2.0', packages=entries)))
    calls = stub_git(monkeypatch, results)
    assert run_check(project, reports)['status'] == 0 and len(calls) == len(results)


@pytest.mark.parametrize('text', ['broken', '{}', '{"version":"1.2.0","packages":[{"name":"foo..bar"}]}'])
def test_unresolved_override_document_defers_entire_check(project, reports, monkeypatch, text):
    (project / '.lake/package-overrides.json').write_text(text)
    calls = stub_git(monkeypatch, [])
    result = run_check(project, reports)
    assert result['outcome'] == 'skipped' and result['status'] == 0 and not calls


def test_canonical_numeric_names_select_paths_pins_and_url_map(project, reports, monkeypatch):
    metadata(project / '.lake/packages/foo.1/.git')
    manifest(project, [git_entry('foo.001', rev='wrong'), git_entry('foo.01', rev='pin', url='old')])
    configuration(project, 'foo.0001')
    calls = stub_git(monkeypatch, [(0, b'different', b''), (0, b'correct', b'')])
    result = gt.preflight_git_packages(project, 'reviewer', reports,
        env={b'LAKE_PKG_URL_MAP': b'{"foo.01":"correct"}'})
    assert result['status'] == 0 and len(calls) == 2
    assert all(c[0][2] == str(project / '.lake/packages/foo.1') for c in calls)
    # The canonical duplicate in the manifest also supplies the final pin.
    calls = stub_git(monkeypatch, [(0, b'pin', b'')])
    assert run_check(project, reports)['status'] == 0 and len(calls) == 1


@pytest.mark.parametrize('name', ['', 'foo-bar', 'foo..bar', '.foo', 'foo.', '«foo»', '[anonymous]', 'λ'])
@pytest.mark.parametrize('where', ['root', 'package', 'url_map'])
def test_rejected_or_unsupported_names_defer_whole_input(project, reports, monkeypatch, name, where):
    env = {}
    if where == 'root':
        manifest(project, name=name)
    elif where == 'package':
        # Invalid unused entries still cause Lake's manifest decoder to fail.
        manifest(project, [git_entry(), git_entry(name)])
    else:
        env[b'LAKE_PKG_URL_MAP'] = json.dumps({name: 'correct'}).encode()
    calls = stub_git(monkeypatch, [])
    result = gt.preflight_git_packages(project, 'reviewer', reports, env=env)
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls


@pytest.mark.parametrize('data', [b'{"foo.01":"a","foo.1":"b"}', b'{"mathlib":"a","mathlib":"b"}'])
def test_url_map_alias_collision_defers_instead_of_guessing_fold_order(project, reports, monkeypatch, data):
    calls = stub_git(monkeypatch, [])
    result = gt.preflight_git_packages(project, 'reviewer', reports, env={b'LAKE_PKG_URL_MAP': data})
    assert result['outcome'] == 'skipped' and result['status'] == 0 and not calls


@pytest.mark.parametrize('config', [None, 'name = "root"\n[[require]]\nname = "missing"\n',
    'name = "root"\nbackend = "invalid"\n[[require]]\nname = "mathlib"\n',
    'name = "root"\n[[require]]\nname = "mathlib"\noptions = "invalid"\n',
    'name = "root"\n[[require]]\nname = "mathlib"\nrev = 123\n'])
def test_unknown_config_or_selection_defers_without_git(project, reports, monkeypatch, config):
    path = project / 'lakefile.toml'
    path.unlink() if config is None else path.write_text(config)
    calls = stub_git(monkeypatch, [])
    result = run_check(project, reports)
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls


def test_executable_lean_config_takes_precedence_and_defers(project, reports, monkeypatch):
    (project / 'lakefile.lean').write_text('import Lake\nopen Lake DSL\npackage root\n')
    calls = stub_git(monkeypatch, [])
    result = run_check(project, reports)
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls
    assert 'executable Lean' in result['message']


@pytest.mark.parametrize('protected', [True, False])
def test_burst_commondir_must_remain_in_protected_metadata(project, tmp_path, protected):
    package = project / '.lake/packages/extra'
    local = package / '.git'
    local.mkdir(parents=True)
    (local / 'HEAD').write_text('ref: refs/heads/main\n')
    common = project / '.git/common-extra' if protected else tmp_path / 'outside-common'
    metadata(common)
    (local / 'commondir').write_text(str(common) + '\n')
    env = {}
    gt.inject_burst_git_safe_directories(env, project)
    assert (('safe.directory', str(package)) in pairs(env)) == protected


def test_real_gate_timeout_preserves_partial_stderr_and_executes_payload(project, reports, tmp_path):
    git = tmp_path / 'git'
    git.write_text('#!/bin/sh\nprintf "partial git error\\r\\n \\t" >&2\nexec /bin/sleep 11\n')
    git.chmod(0o755)
    # Exercise the real default 10-second timeout, publication, and exec chain.
    result = subprocess.run(gate(project, reports, ['/usr/bin/printf', 'payload-ran'], path=native_lake_path(tmp_path)),
                            capture_output=True, timeout=15)
    assert result.returncode == 0 and result.stdout == b'payload-ran'
    record = json.loads((reports / 'result.json').read_bytes())
    assert record['status'] == 0 and record['outcome'] == 'inconclusive'
    assert (reports / '000-rev-parse.stderr').read_bytes() == b'partial git error\r\n \t'


def test_effective_git_override_can_establish_abort(project, reports, monkeypatch):
    # The old manifest pin would pass. Lake selects the override's different pin
    # and reaches the failed origin query; this remains an abort, not log-only.
    (project / '.lake/package-overrides.json').write_text(json.dumps(dict(version='1.2.0',
        packages=[git_entry(rev='new-pin')])))
    calls = stub_git(monkeypatch, [(0, PIN.encode(), b''), (128, b'', b'origin unavailable\n')])
    result = run_check(project, reports)
    assert result['status'] == 128 and result['outcome'] == 'abort' and len(calls) == 2


@pytest.mark.parametrize('separator', [b'\r', b'\x0b', b'\x7f'])
def test_non_toml_control_characters_defer_instead_of_being_normalized(project, reports, monkeypatch, separator):
    (project / 'lakefile.toml').write_bytes(separator.join([b'name = "root"', b'[[require]]', b'name = "mathlib"']))
    calls = stub_git(monkeypatch, [])
    result = run_check(project, reports)
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls


def test_supported_toml_crlf_comments_and_fields_select_same_entry(project, reports, monkeypatch):
    (project / 'lakefile.toml').write_bytes(b'# configuration\r\nname = "root"\r\npackagesDir = "elsewhere"\r\n'
        b'[[require]] # one selected dependency\r\nname = "mathlib"\r\ngit = "remote#with-fragment"\r\n'
        b'rev = "input-rev"\r\nsubDir = "sub"\r\nscope = "scope"\r\n')
    calls = stub_git(monkeypatch, [(0, PIN.encode(), b'')])
    result = run_check(project, reports)
    assert result['status'] == 0 and len(calls) == 1
    # Lake uses the explicit manifest packagesDir, even when config differs.
    assert calls[0][0][2] == str(project / '.lake/packages/mathlib')


def system_config_location(project, tmp_path, source):
    home = tmp_path / 'payload-home'
    home.mkdir(exist_ok=True)
    env = {b'HOME': os.fsencode(home)}
    if source == 'absolute':
        config = tmp_path / 'absolute-config.toml'
        env[b'LAKE_CONFIG'] = os.fsencode(config)
    elif source == 'relative':
        config = project / 'system/config.toml'
        env[b'LAKE_CONFIG'] = b'system/config.toml'
    elif source == 'relative_home':
        config = project / 'relative-home/.lake/config.toml'
        env[b'HOME'] = b'relative-home'
    else:
        assert source == 'home'
        config = home / '.lake/config.toml'
    config.parent.mkdir(parents=True, exist_ok=True)
    return config, env


@pytest.mark.parametrize('source', ['absolute', 'relative', 'home', 'relative_home'])
@pytest.mark.parametrize('contents', [b'[[', b'[cache]\ndefaultService = "undefined-service"\n'])
def test_system_config_failure_defers_before_git(project, reports, tmp_path, monkeypatch, source, contents):
    config, env = system_config_location(project, tmp_path, source)
    config.write_bytes(contents)
    # A different supervisor HOME/cwd must not alter the saved payload mapping.
    supervisor = tmp_path / 'supervisor'
    supervisor.mkdir()
    monkeypatch.chdir(supervisor)
    monkeypatch.setenv('HOME', str(supervisor))
    monkeypatch.setenv('LAKE_CONFIG', str(supervisor / 'absent.toml'))
    before = dict(env)
    calls = stub_git(monkeypatch, [])
    result = gt.preflight_git_packages(project, 'reviewer', reports, env=env)
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls
    assert 'Lake system config exists' in result['message']
    assert str(config) in result['message'] and env == before
    assert config.read_bytes() == contents


@pytest.mark.parametrize('contents', [b'', b'# valid config\n', b'[cache]\ndefaultService = ""\n'])
def test_even_valid_existing_system_config_is_outside_supported_subset(project, reports, tmp_path, monkeypatch, contents):
    config, env = system_config_location(project, tmp_path, 'home')
    config.write_bytes(contents)
    calls = stub_git(monkeypatch, [])
    result = gt.preflight_git_packages(project, 'reviewer', reports, env=env)
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls
    assert 'loadability not established' in result['message']


@pytest.mark.parametrize('kind', ['unreadable', 'directory', 'fifo', 'symlink_loop'])
def test_system_config_filesystem_uncertainty_defers(project, reports, tmp_path, monkeypatch, kind):
    config, env = system_config_location(project, tmp_path, 'absolute')
    if kind == 'directory':
        config.mkdir()
    elif kind == 'fifo':
        os.mkfifo(config)  # Gate must never open this and wait for a writer.
    elif kind == 'symlink_loop':
        config.symlink_to(config)
    else:
        config.write_bytes(b'[[')
        config.chmod(0)
    calls = stub_git(monkeypatch, [])
    try:
        result = gt.preflight_git_packages(project, 'reviewer', reports, env=env)
        assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls
    finally:
        if kind == 'unreadable':
            config.chmod(0o600)


@pytest.mark.parametrize('errno_value', [13, 5, 20])  # EACCES, EIO, ENOTDIR
@pytest.mark.parametrize('source', ['absolute', 'home'])
def test_system_config_stat_errors_are_not_mistaken_for_absence(project, reports, tmp_path, monkeypatch, errno_value, source):
    config, env = system_config_location(project, tmp_path, source)
    original_stat = os.stat
    def inaccessible(path, *args, **kwargs):
        if os.fsencode(path) == os.fsencode(config):
            raise OSError(errno_value, 'fixture system-config lookup failure', os.fsdecode(path))
        return original_stat(path, *args, **kwargs)
    # Works even when tests run as root and mode 000 alone would be readable.
    monkeypatch.setattr(gt.os, 'stat', inaccessible)
    calls = stub_git(monkeypatch, [])
    result = gt.preflight_git_packages(project, 'reviewer', reports, env=env)
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls
    assert 'fixture system-config lookup failure' in result['message']


@pytest.mark.parametrize('source', ['absolute', 'relative', 'home', 'relative_home'])
def test_absent_system_config_still_allows_confirmed_abort(project, reports, tmp_path, monkeypatch, source):
    config, env = system_config_location(project, tmp_path, source)
    assert not config.exists()
    if source in {'absolute', 'relative'}:
        # Explicit LAKE_CONFIG has precedence even if its target is absent;
        # Lake must not fall back to the invalid default HOME configuration.
        fallback = Path(os.fsdecode(env[b'HOME'])) / '.lake/config.toml'
        fallback.parent.mkdir()
        fallback.write_bytes(b'[[')
    monkeypatch.chdir(tmp_path)  # Relative configuration uses the project's cwd.
    calls = stub_git(monkeypatch, [(0, b'other-head', b''), (128, b'', b'origin unavailable\n')])
    before = dict(env)
    result = gt.preflight_git_packages(project, 'reviewer', reports, env=env)
    assert result['status'] == 128 and result['outcome'] == 'abort' and len(calls) == 2
    assert env == before and not config.exists()


def test_without_saved_home_or_lake_config_there_is_no_system_config(project, reports, tmp_path, monkeypatch):
    config, _ = system_config_location(project, tmp_path, 'home')
    config.write_bytes(b'[[')
    monkeypatch.setenv('HOME', str(config.parent.parent))
    monkeypatch.setenv('LAKE_CONFIG', str(config))
    monkeypatch.setenv('XDG_CONFIG_HOME', str(config.parent))
    calls = stub_git(monkeypatch, [(0, b'other-head', b''), (128, b'', b'origin unavailable\n')])
    # Do not consult the supervisor's environment, passwd home, or XDG fallback.
    result = gt.preflight_git_packages(project, 'reviewer', reports, env={})
    assert result['status'] == 128 and len(calls) == 2


@pytest.mark.parametrize('env', [{b'LAKE_CONFIG': b''}, {b'LAKE_CONFIG': b'bad\xff'},
                                {b'HOME': b'bad\xff'}, {b'LAKE_CONFIG': b'bad\0path'}])
def test_unsupported_system_config_environment_path_defers(project, reports, monkeypatch, env):
    calls = stub_git(monkeypatch, [])
    result = gt.preflight_git_packages(project, 'reviewer', reports, env=env)
    assert result['status'] == 0 and result['outcome'] == 'skipped' and not calls


def test_system_config_deferral_is_published_and_payload_environment_survives(project, reports, tmp_path):
    config = project / 'config.toml'
    config.write_bytes(b'[cache]\ndefaultService = "undefined-service"\n')
    env = {b'PATH': b'/usr/bin:/bin', b'LANG': b'C', b'HOME': os.fsencode(tmp_path),
           b'LAKE_CONFIG': b'config.toml', b'GIT_CONFIG_COUNT': b'1',
           b'GIT_CONFIG_KEY_0': b'safe.directory', b'GIT_CONFIG_VALUE_0': os.fsencode(project)}
    # No Git executable on the supplied Git-only PATH. An accidental probe
    # would abort; deferral must instead publish and exec the native payload.
    result = subprocess.run(gate(project, reports, ['/usr/bin/env', '-0'], path=str(tmp_path / 'no-git')),
                            env=env, cwd=tmp_path, capture_output=True, timeout=5)
    assert result.returncode == 0, result.stderr
    assert dict(item.split(b'=', 1) for item in result.stdout.split(b'\0') if item) == env
    record = json.loads((reports / 'result.json').read_bytes())
    assert record['status'] == 0 and record['outcome'] == 'skipped' and record['checks'] == []
    assert 'Lake system config exists' in record['message']
