"""Legacy managed trust, explicit burst admission, and Lake's deletion preflight.

The command-line gate is Linux-only, standard-library-only, and runs with -I -S.
It restores the exec-time environment from procfs before running Git or a provider.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import time


# Keep this legacy helper's behavior unchanged for managed observations.
def _inject_git_safe_directories(env: dict[str, str], repo: Path) -> None:
    safe_dirs: list[str] = [str(repo)]
    packages_root = repo / ".lake" / "packages"
    if packages_root.exists():
        for package_dir in sorted(packages_root.iterdir()):
            if not package_dir.is_dir():
                continue
            if (package_dir / ".git").exists():
                safe_dirs.append(str(package_dir))
    existing = int(env.get("GIT_CONFIG_COUNT", "0") or "0")
    env["GIT_CONFIG_COUNT"] = str(existing + len(safe_dirs))
    for idx, path in enumerate(safe_dirs, start=existing):
        env[f"GIT_CONFIG_KEY_{idx}"] = "safe.directory"
        env[f"GIT_CONFIG_VALUE_{idx}"] = path


def _protected_metadata(path: Path, repo: Path) -> bool:
    """Metadata must stay under a canonical .git, never a writable overlay."""
    root = repo / '.lake/packages'
    path = path.resolve()
    roots = [repo / '.git']
    if root.resolve() == root and root.is_dir():
        roots += [p / '.git' for p in root.iterdir() if p.resolve() == p]
    return any(r.resolve() == r and path.is_relative_to(r) for r in roots)


def _admit_burst_package(path: Path, repo: Path) -> bool:
    root = repo / '.lake/packages'
    actual = path.resolve()
    if root.resolve() != root or actual.parent != root:
        return False
    metadata = actual / '.git'
    try:
        if metadata.is_file():
            line = metadata.read_text().strip()
            if not line.startswith('gitdir: '):
                return False
            metadata = actual / line[8:]
        metadata = metadata.resolve(strict=True)
        if not _protected_metadata(metadata, repo) or not (metadata / 'HEAD').is_file():
            return False
        common = metadata
        if (metadata / 'commondir').is_file():
            common = (metadata / (metadata / 'commondir').read_text().strip()).resolve(strict=True)
        metadata_paths = [metadata / 'HEAD', common / 'objects', common]
        for base in (metadata, common):
            metadata_paths += [base / name for name in ('config', 'refs', 'packed-refs', 'commondir')
                               if (base / name).exists()]
        return (all(_protected_metadata(p, repo) for p in metadata_paths)
                and (common / 'objects').is_dir()
                and os.access(metadata / 'HEAD', os.R_OK)
                and os.access(common / 'objects', os.R_OK | os.X_OK))
    except (OSError, UnicodeError, ValueError):
        return False


def inject_burst_git_safe_directories(env: dict[str, str], repo: Path) -> None:
    """Stricter, named burst policy; discovery is delegated to the legacy helper."""
    count = env.get('GIT_CONFIG_COUNT', '0') or '0'
    if not count.isascii() or not count.isdecimal():
        raise ValueError('invalid GIT_CONFIG_COUNT: expected a nonnegative integer')
    existing = int(count)
    for i in range(existing):
        for field in ('KEY', 'VALUE'):
            key = f'GIT_CONFIG_{field}_{i}'
            if key not in env or (field == 'KEY' and not env[key]):
                raise ValueError(f'invalid Git command configuration: missing or empty {key}')
    repo = repo.resolve()
    candidates: dict[str, str] = {}
    _inject_git_safe_directories(candidates, repo)
    admitted = [repo]
    for i in range(1, int(candidates['GIT_CONFIG_COUNT'])):
        path = Path(candidates[f'GIT_CONFIG_VALUE_{i}'])
        if _admit_burst_package(path, repo):
            admitted.append(path.resolve())
    admitted = list(dict.fromkeys(admitted))
    if any(str(p).endswith('/*') for p in admitted):
        raise ValueError('cannot express exact Git trust for a path ending in /*')
    for i, path in enumerate(admitted, existing):
        env[f'GIT_CONFIG_KEY_{i}'] = 'safe.directory'
        env[f'GIT_CONFIG_VALUE_{i}'] = str(path)
    env['GIT_CONFIG_COUNT'] = str(existing + len(admitted))


def _strict_json(data: bytes):
    def invalid_constant(value):
        raise ValueError(f'non-JSON constant: {value}')
    def unsupported_float(value):
        # Lean parses numbers even in ignored fields before schema decoding.
        # Python floats lose exponent information (including 0e<huge>), so
        # defer every fractional/exponent token throughout the document.
        raise ValueError('JSON floating/exponent token outside supported subset')
    def unique_object(pairs):
        obj = {}
        for key, value in pairs:
            if key in obj:
                raise ValueError('duplicate JSON object key; decoding deferred')
            obj[key] = value
        return obj
    value = json.loads(data.decode('utf-8'), parse_constant=invalid_constant,
                       parse_float=unsupported_float,
                       object_pairs_hook=unique_object)
    # Reject unpaired surrogate escapes rather than guessing Lean's decoding.
    json.dumps(value, ensure_ascii=False).encode('utf-8')
    return value


def _name(value: str) -> str:
    """Established subset of Lean String.toName, with canonical numeric parts.

    Init/Meta/Defs.lean:1190–1242; Name.fromJson? rejects the anonymous result.
    Escaped, Unicode and anonymous names are deliberately deferred as a whole
    input. ASCII identifiers and decimal components have unambiguous identity
    and the returned spelling is also Name.toString (escape := false).
    """
    if not isinstance(value, str) or not re.fullmatch(
        r"(?:[A-Za-z_][A-Za-z_0-9'!?]*|[0-9]+)(?:\.(?:[A-Za-z_][A-Za-z_0-9'!?]*|[0-9]+))*", value
    ):
        raise ValueError('unsupported Lean name')
    return '.'.join((part.lstrip('0') or '0') if part.isdecimal() else part
                    for part in value.split('.'))


def _string(value):
    if not isinstance(value, str) or '\0' in value:
        raise ValueError('unsupported string')
    return value


def _entries(document: dict) -> list[dict]:
    """Modern full/partial manifest entries, validated before any Git query."""
    version = document.get('version', document.get('schemaVersion'))
    if not ((type(version) is int and version >= 7) or
            (isinstance(version, str) and re.fullmatch(r'1\.[0-9]+\.[0-9]+', version))):
        raise ValueError('unsupported manifest version')
    packages = document.get('packages', [])
    if not isinstance(packages, list):
        raise ValueError('invalid packages array')
    entries = []
    for entry in packages:
        name = _name(entry['name'])
        if type(entry.get('inherited')) is not bool:
            raise ValueError('invalid inherited flag')
        for key in ('scope', 'configFile'):
            if key in entry:
                _string(entry[key])
        for key in ('manifestFile', 'inputRev', 'subDir'):
            if entry.get(key) is not None:
                _string(entry[key])
        required = {'git': ('url', 'rev'), 'path': ('dir',)}.get(entry.get('type'))
        if required is None:
            raise ValueError('unsupported entry type')
        for key in required:
            _string(entry[key])
        entries.append(dict(entry, name=name))
    return entries


def _root_dependencies(repo: Path) -> tuple[str, list[str]]:
    """Read a deliberately small declarative TOML subset, never evaluate Lean.

    Lake/Load/Package.lean:119–130 prefers lakefile.lean. Lake/Load/Toml.lean
    decodes all configuration before materialization. Thus even unrelated,
    unsupported fields require deferral, not just ignoring them. Accept only
    plain ASCII single-line string assignments, comments, and [[require]].
    This avoids claiming equivalence between general TOML/Lean parsers.
    """
    if (repo / 'lakefile.lean').exists():
        raise ValueError('executable Lean configuration; dependency selection unknown')
    text = (repo / 'lakefile.toml').read_bytes().decode('utf-8').replace('\r\n', '\n')
    if not text.isascii() or any((ord(c) < 32 and c not in '\n\t') or ord(c) == 127 for c in text):
        raise ValueError('unsupported TOML syntax')
    root: dict[str, str] = {}
    deps: list[dict[str, str]] = []
    table = root
    for line in text.splitlines():
        if re.fullmatch(r'[ \t]*(?:#.*)?', line):
            continue
        if re.fullmatch(r'[ \t]*\[\[require\]\][ \t]*(?:#.*)?', line):
            table = {}
            deps.append(table)
            continue
        field = re.fullmatch(r'[ \t]*([A-Za-z]+)[ \t]*=[ \t]*"([^"\\]*)"[ \t]*(?:#.*)?', line)
        if field is None:
            raise ValueError('unsupported TOML syntax')
        key, value = field.groups()
        allowed = {'name', 'packagesDir'} if table is root else {'name', 'git', 'rev', 'subDir', 'path', 'scope'}
        if key not in allowed or key in table:
            raise ValueError('unsupported or duplicate TOML field')
        table[key] = _string(value)
    return _name(root['name']), [_name(dep['name']) for dep in deps]


def _selected_manifest_package(repo: Path) -> tuple[list[dict], Path]:
    """Prove the first materialization step of a default manifest load.

    Resolve.lean:207–226 uses foldrM: the last direct require is visited first,
    reusing the already loaded root. We stop after that FIRST missing entry.
    Selecting any later sibling/transitive entry would require proving the
    preceding package's config loads (possibly executable Lean). No such proof
    is available without running Lake, so the rest is deliberately deferred.
    """
    root_name, dependencies = _root_dependencies(repo)
    manifest = _strict_json((repo / 'lake-manifest.json').read_bytes())
    entries = _entries(manifest)
    if not isinstance(manifest.get('fixedToolchain', False), bool):
        raise ValueError('invalid fixedToolchain flag')
    if 'name' in manifest:
        _name(manifest['name'])
    if 'lakeDir' in manifest:
        _string(manifest['lakeDir'])
    # Explicit packagesDir avoids guessing workspace defaults in other schemas.
    packages_dir_value = _string(manifest['packagesDir'])
    if not packages_dir_value:
        # Lake first joins "" / name => /name, then wsDir / /name. Python's
        # repo / "" / name instead selects repo/name. Do not probe that path.
        raise ValueError('empty manifest packagesDir; Lake selects absolute package paths')
    packages_dir = repo / packages_dir_value
    # Package.relLakeDir is fixed to .lake (Config/Package.lean:207–213).
    # The manifest lakeDir is NOT the workspace override directory.
    try:
        overrides = (repo / '.lake/package-overrides.json').read_bytes()
    except FileNotFoundError:
        overrides = None
    if overrides is not None:
        entries += _entries(_strict_json(overrides))  # Partial-manifest parser.
    # Resolve.lean:580–588: last entry wins, after canonical Name decoding.
    effective = {entry['name']: entry for entry in entries}
    for name in reversed(dependencies):
        if name != root_name:  # Already loaded packages are reused before resolving.
            return [effective[name]], packages_dir
    return [], packages_dir


def _url_map(data: bytes) -> dict[str, str]:
    value = _strict_json(data)
    if not isinstance(value, dict):
        raise ValueError('not a NameMap')
    result = {}
    for key, url in value.items():
        name = _name(key)
        if name in result:
            # Lean folds JSON's ordered object, not Python insertion order.
            # Defer collisions rather than assuming an ordering of aliases.
            raise ValueError('ambiguous canonical URL-map keys')
        result[name] = _string(url)
    return result


def git_failure_cause(stderr: bytes, status: int) -> str:
    message = stderr.lower()
    if b'dubious ownership' in message or b'unsafe repository' in message:
        return 'dubious ownership'
    if status == 127 or b'command not found' in message:
        return 'git command not found'
    if b'not a git repository' in message or b'not a repository' in message:
        return 'not a repository'
    return 'Git metadata/configuration or access failure'


def _same_url(remote: str, expected: str, repo: Path) -> bool:
    if remote == expected:
        return True
    # Lake's realPath calls execute in the Lake process cwd, not Git's -C cwd.
    try:
        return (repo / remote).resolve(strict=True) == (repo / expected).resolve(strict=True)
    except (OSError, ValueError, RuntimeError):
        return False


def _require_absent_lake_system_config(repo: Path, env: dict[bytes, bytes]) -> None:
    """Establish the absence-only system-config subset before materialization.

    Lake/Config/Env.lean:90–100,169–179 uses LAKE_CONFIG if set, otherwise
    HOME/.lake/config.toml on Linux (no passwd or XDG fallback). Relative paths
    use Lake's project cwd. Load/Workspace.lean:31–36 loads this file first;
    Load/Toml.lean:513–591 validates semantics as well as syntax. We do neither:
    any existing input, even an empty/valid file, defers the whole gate.
    """
    if b'LAKE_CONFIG' in env:
        raw_path = env[b'LAKE_CONFIG']
        if not raw_path:
            raise ValueError('empty LAKE_CONFIG; system-config path deferred')
    elif b'HOME' in env:
        if not env[b'HOME']:
            # Lake's FilePath division yields /.lake/config.toml here, not
            # repo/.lake/config.toml. Defer without inspecting the real root.
            raise ValueError('empty HOME; Lake system config is /.lake/config.toml; deferred')
        raw_path = os.path.join(env[b'HOME'], b'.lake', b'config.toml')
    else:
        return  # Lake has no applicable system-config path without either env key.
    # Do not guess how Lean decodes invalid environment bytes or embedded NULs.
    _string(raw_path.decode('utf-8'))
    # Preserve symlink/.. and trailing-slash semantics; never expand ~ or use
    # Python's HOME/current environment in place of the saved payload mapping.
    path = os.path.join(os.fsencode(repo), raw_path)
    try:
        os.stat(path)
    except FileNotFoundError:
        return  # Only a confirmed absent path admits the default configuration.
    # Other stat errors propagate to the deferral handler, including EACCES and
    # ELOOP. Do not open files/devices/FIFOs, or mistake a directory for absence.
    raise ValueError(f'Lake system config exists at {os.fsdecode(path)!r}; loadability not established')


# Identity of the inspected Linux Lean/Lake v4.33.0 distribution, not a claim
# that every executable called "lake" implements the source audited here.
# Different builds/platforms defer; no proxy or version-output identification.
_AUDITED_NATIVE_INSTALL = {
    'bin/lake': '60330ab6f07dce20f3fa9ebb08e8b984ea9549eac172afeb15d9d2227060e2b3',
    'bin/lean': 'e8baaa71855a616dc351028f3ad2200051b0671f423a1696a100e809302d5550',
    'lib/lean/libLake_shared.so': 'feb2914d056638438613284b1f98278579ff8d8d99d0567e9823167806d150c0',
    'lib/lean/libInit_shared.so': '2f96493ac9046f64ec7bcb78102a4c1e7cd9a7042487f246af45eda8a96667f5',
    'lib/lean/libleanshared_2.so': '2f96493ac9046f64ec7bcb78102a4c1e7cd9a7042487f246af45eda8a96667f5',
    'lib/lean/libleanshared_1.so': '2f96493ac9046f64ec7bcb78102a4c1e7cd9a7042487f246af45eda8a96667f5',
    'lib/lean/libleanshared.so': 'f2a36d5e56b936afc07bdbea18542de0bce6a9977ebc325915272c300d992d42',
}


def _require_lake_installation(repo: Path, env: dict[bytes, bytes], path: str) -> None:
    """Establish a narrow native/co-located findInstall? route without exec.

    InstallPath.lean:329–336,419–430 admits the co-located installation without
    querying Lean. Overrides without an established sysroot require executing
    Lean and therefore defer. Elan selection is not inferred from its proxies
    or toolchain selectors.
    """
    if sys.platform != 'linux' or os.uname().machine != 'x86_64':
        raise ValueError('platform is outside the audited native installation subset')
    if b'ELAN_TOOLCHAIN' in env:
        raise ValueError('ELAN_TOOLCHAIN selection is not established')
    for directory in (repo, *repo.parents):
        try:
            (directory / 'lean-toolchain').stat()
        except FileNotFoundError:
            continue
        raise ValueError('lean-toolchain selection is not established')
    if any(key.startswith(b'LD_') for key in env):
        raise ValueError('custom dynamic-loader environment is not established')
    search = path if path else _string(env.get(b'PATH', b'').decode('utf-8'))
    if not search:
        raise ValueError('Lake executable selection requires an explicit nonempty PATH')
    lake = None
    for directory in search.split(os.pathsep):
        candidate = repo / directory / 'lake'
        if candidate.is_file() and os.access(candidate, os.X_OK):
            lake = candidate.resolve(strict=True)
            break
    if lake is None or lake.name != 'lake' or lake.parent.name != 'bin':
        raise ValueError('native Lake installation is not established (missing executable or proxy)')
    root = lake.parent.parent
    # ELAN/ELAN_HOME alone do not select a toolchain for this direct native
    # executable. findElanInstall? merely constructs optional cache-path data.
    # Audited ELF RUNPATH searches lib before lib/lean. Reject shadow copies
    # and alternate hardware-capability directories rather than hash a library
    # that the loader would not select. System libc/loader remain OS prerequisites.
    shadow_paths = [root / 'lib' / Path(name).name for name in _AUDITED_NATIVE_INSTALL
                    if name.startswith('lib/lean/')]
    for directory in (root / 'lib', root / 'lib/lean'):
        shadow_paths += [directory / name for name in (
            'glibc-hwcaps', 'libc.so.6', 'ld-linux-x86-64.so.2',
            'libpthread.so.0', 'libdl.so.2', 'librt.so.1', 'libm.so.6')]
    for alternate in shadow_paths:
        try:
            alternate.stat()
        except FileNotFoundError:
            continue
        raise ValueError('native installation library selection is not established')
    deadline = time.monotonic() + 2.0
    for relative, digest in _AUDITED_NATIVE_INSTALL.items():
        member = root / relative
        if not member.is_file():
            raise ValueError('native installation member is missing or not a regular file')
        if relative.startswith('bin/') and not os.access(member, os.X_OK):
            raise ValueError('native Lake/Lean executable is inaccessible')
        with member.open('rb') as source:
            # Query the opened file's mount in this (sandbox) namespace, not
            # the installation directory's mount or the supervisor's view.
            # This follows library symlinks and individual bind mounts. Linux
            # forbids PROT_EXEC mappings on noexec even when hashing succeeds;
            # the library's own execute-mode bits do not establish this.
            # Missing flag support / failed mount lookup also defer via caller.
            if os.fstatvfs(source.fileno()).f_flag & os.ST_NOEXEC:
                raise ValueError(f'native installation member is on a noexec mount: {relative}')
            actual = hashlib.sha256()
            for chunk in iter(lambda: source.read(131072), b''):
                if time.monotonic() >= deadline:
                    raise ValueError('native installation identity budget exhausted; deferred')
                actual.update(chunk)
        if actual.hexdigest() != digest:
            raise ValueError('Lake/Lean installation identity is outside the audited native subset')
    override = env.get(b'LAKE_OVERRIDE_LEAN', b'').lower()
    if override in (b'y', b'yes', b't', b'true', b'on', b'1'):
        # Presence of LEAN_SYSROOT takes precedence over LEAN, including an
        # empty LEAN. Admit only this exact audited co-located root; no attempt
        # to run --print-prefix/--githash or interpret another installation.
        if env.get(b'LEAN_SYSROOT') != os.fsencode(root):
            raise ValueError('LAKE_OVERRIDE_LEAN requires the established native LEAN_SYSROOT; deferred')


def preflight_git_packages(repo: Path, role: str, report_dir: Path, *,
                           env: dict[bytes, bytes], path: str = '',
                           budget: float = 20.0, query_timeout: float = 10.0) -> dict:
    """Abort only on an established first materialization that would delete.

    Empty path means inherit (not an explicit empty PATH). Timeouts are
    inconclusive, not a Git exit or proof of a URL mismatch: pass through.
    """
    result = {'status': 0, 'outcome': 'pass', 'role': role, 'checks': [],
              'message': 'Lake Git preflight passed.', 'time': time.time()}
    try:
        _require_absent_lake_system_config(repo, env)
        manifest = _selected_manifest_package(repo)
        url_map = _url_map(env.get(b'LAKE_PKG_URL_MAP', b'{}'))
        _require_lake_installation(repo, env, path)
    except (OSError, ValueError, TypeError, KeyError, AttributeError, RecursionError, RuntimeError) as exc:
        result.update(outcome='skipped', message=(
            f'Dependency selection/decoding not established ({exc}); deferred to Lake.'))
        return result
    result['selection'] = 'first materialization from supported root TOML; remaining traversal deferred'
    run_env = dict(env)
    if path:
        run_env[b'PATH'] = os.fsencode(path)
    deadline = time.monotonic() + budget
    packages, root = manifest
    result['message'] = ('First selected materialization has no deletion condition; remaining traversal deferred.'
                         if packages else 'No missing direct dependencies; no Git materialization selected.')
    for entry in packages:
        package = root / entry['name']
        if entry['type'] != 'git' or not package.is_dir():
            continue  # Lake's path or fresh-clone branch cannot delete this checkout.
        queries = [['rev-parse', '--verify', '--end-of-options', 'HEAD'],
                   ['remote', 'get-url', 'origin']]
        for index, query in enumerate(queries):
            command = ['git', '-C', str(package), *query]
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                result.update(outcome='inconclusive', message='Preflight time budget exhausted; deferred to Lake.')
                return result
            error = ''
            try:
                # Lake spawns Git in the package cwd; this also matters for
                # relative PATH entries. URL realPath comparison stays at repo.
                proc = subprocess.run(command, env=run_env, cwd=package, capture_output=True,
                                      timeout=min(query_timeout, remaining))
                status, stdout, stderr = proc.returncode, proc.stdout, proc.stderr
            except subprocess.TimeoutExpired as exc:
                status, stdout, stderr = None, b'', exc.stderr or b''
                error = 'Git query timed out; its result is unknown.'
            except OSError as exc:
                status = 127 if isinstance(exc, FileNotFoundError) else 126
                stdout, stderr, error = b'', b'', str(exc)
            stderr_name = f'{len(result["checks"]):03d}-{query[0]}.stderr'
            (report_dir / stderr_name).write_bytes(stderr)
            check = {'package': entry['name'], 'path': str(package), 'command': command,
                     'status': status, 'stderr_file': stderr_name, 'os_error': error}
            result['checks'].append(check)
            if status is None:
                result.update(outcome='inconclusive', message=error + ' Deferred to Lake.')
                return result
            # Lake captureProc? trims ASCII whitespace only, not revision/URL input.
            try:
                value = stdout.strip(b' \t\r\n').decode('utf-8') if status == 0 else None
            except UnicodeError:
                result.update(outcome='inconclusive', message='Non-UTF8 Git stdout; deferred to Lake.')
                return result
            if index == 0:
                if value == entry['rev']:
                    break  # Lake never queries origin at its pinned HEAD.
                continue
            expected = url_map.get(entry['name'], entry['url'])
            if value is not None and _same_url(value, expected, repo):
                continue  # Lake may update the revision, but will not delete.
            cause = git_failure_cause(stderr, status) if status else 'remote URL differs from the manifest URL'
            result.update(status=(status if status > 0 else 128 - status) if status else 1,
                          outcome='abort', message=(
                f'Lake Git preflight stopped package {entry["name"]} ({package}) in the {role} sandbox: '
                f'likely {cause}. Git exit status: {status}. '
                'Lake would enter its URL-change deletion branch. '
                f'Report: {report_dir / "result.json"}; raw stderr: {report_dir / stderr_name}.'))
            return result
    return result


def original_environment() -> dict[bytes, bytes]:
    """Linux exec-time bytes, before CPython locale coercion or site processing."""
    return dict(item.split(b'=', 1) for item in Path('/proc/self/environ').read_bytes().split(b'\0') if item)


def _write_result(path: Path, result: dict) -> None:
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(result, ensure_ascii=True) + '\n')
    temporary.replace(path)  # Publish only after all raw stderr files are closed.


def _report_storage_unavailable(directory: Path, exc: OSError) -> None:
    """Best effort only: diagnostic storage failure must not veto a payload."""
    try:
        _write_result(directory / 'result.json', {
            'status': 0, 'outcome': 'skipped', 'checks': [],
            'message': f'Git preflight report storage unavailable ({exc}); deferred to Lake.',
        })
    except OSError:
        pass


def _gate_index(command: list[str]) -> int | None:
    script = str(Path(__file__).resolve())
    return next((i for i, value in enumerate(command)
                 if value == script and command[i + 1:i + 2] == ['--gate']), None)


def report_directory(command: list[str]) -> Path | None:
    index = _gate_index(command)
    return Path(command[index + 5]) if index is not None else None


def supervise_git_launch(command: list[str]) -> list[str]:
    """Provider-side outer recorder; no change to the shared bwrap argv contract."""
    directory = report_directory(command)
    if directory is None:
        return command
    i = _gate_index(command)
    interpreter = command[i - 3]  # validated executable followed by -I -S
    return [interpreter, '-I', '-S', str(Path(__file__).resolve()),
            '--supervise', str(directory), *command]


def launch_failure(command: list[str]) -> tuple[int, str] | None:
    """Read a complete durable result; works even when the tmux shell is alive."""
    directory = report_directory(command)
    if directory is None:
        return None
    for filename in ('result.json', 'launch.json'):
        try:
            record = json.loads((directory / filename).read_bytes())
        except (OSError, ValueError):
            continue
        status = record.get('status', 0)
        if not status:
            continue
        detail = record.get('message', 'Burst bootstrap failed') + f'\nDiagnostic directory: {directory}\n'
        for check in record.get('checks', []):
            detail += f'{shlex.join(check["command"])}: exit {check["status"]}\n'
            detail += (directory / check['stderr_file']).read_bytes().decode('utf-8', errors='replace')
            if check.get('os_error'):
                detail += '\n' + check['os_error'] + '\n'
        if filename == 'launch.json':
            detail += (directory / 'bootstrap.stderr').read_bytes().decode('utf-8', errors='replace')
        return status, detail
    return None


def main() -> None:
    env = original_environment()
    if sys.argv[1] == '--supervise':
        directory = Path(sys.argv[2])
        command = sys.argv[3:]
        try:
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)
            stderr = (directory / 'bootstrap.stderr').open('wb')
        except OSError as exc:
            _report_storage_unavailable(directory, exc)
            # The inner gate can retry after HOME is mounted. Do not change
            # command/environment or turn absent report storage into failure.
            os.execvpe(command[0], command, env)
        try:
            try:
                status = subprocess.call(command, env=env, stderr=stderr)
            except OSError as exc:
                status = 127 if isinstance(exc, FileNotFoundError) else 126
                try:
                    stderr.write(os.fsencode(str(exc)))
                except OSError:
                    pass
        finally:
            try:
                stderr.close()
            except OSError:
                pass
        status = status if status >= 0 else 128 - status
        try:
            _write_result(directory / 'launch.json', {'status': status, 'message': f'Burst launcher exited with status {status}.'})
        except OSError:
            pass  # Payload already ran: retain its status and never launch twice.
        sys.exit(status)
    _, repo, role, path, directory, *command = sys.argv[1:]
    report_dir = Path(directory)
    try:
        report_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
        result = preflight_git_packages(Path(repo), role, report_dir, env=env, path=path)
        _write_result(report_dir / 'result.json', result)
    except OSError as exc:
        _report_storage_unavailable(report_dir, exc)
        os.execvpe(command[0], command, env)
    if result['status']:
        print(result['message'], file=sys.stderr)
        sys.exit(result['status'])
    os.execvpe(command[0], command, env)


if __name__ == '__main__':
    main()
