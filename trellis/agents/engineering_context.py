"""The engineering profile of the existing Codex/bwrap provider boundary."""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import tomllib
import uuid
import hashlib
import shlex
import stat

from trellis.burst_home import seed_burst_home
from trellis.agents.host_files import atomic_text, provider_directory

# Configuration which can inject task context is withheld. Unknown keys fail
# closed so a new provider setting cannot silently widen this file view.
CONTEXT_KEYS = {"developer_instructions", "model_instructions_file", "instructions",
                "experimental_compact_prompt_file", "skills", "plugins", "mcp_servers",
                "agents", "hooks", "project_doc_fallback_filenames", "project_doc_max_bytes"}
RETAINED_KEYS = {"approvals_reviewer", "approval_policy", "sandbox_mode", "sandbox_workspace_write",
                 "permissions", "model", "model_reasoning_effort", "model_reasoning_summary", "review_model",
                 "model_provider", "model_providers", "service_tier", "personality", "projects", "tui",
                 "cli_auth_credentials_store", "forced_login_method", "forced_chatgpt_workspace_id",
                 "disable_response_storage", "web_search", "history", "network"}
FEATURES = {"skip_host_skill_discovery": True, "plugins": False, "apps": False,
            "hooks": False, "memories": False, "shell_snapshot": False}
POLICY_FEATURES = {"guardian_approval", "exec_permission_approvals", "write_stdin_approval",
                   "request_permissions_tool", "network_proxy", "system_proxy_fallback", "respect_system_proxy"}
EXCLUDED_HOME_PATHS = ("skills", "plugins", "sessions", "AGENTS.md")


def turn_directories(work: Path, home: Path) -> dict:
    result = {}
    for path in (work, work / '.trellis', home, home / '.codex'):
        info = path.lstat()
        if not stat.S_ISDIR(info.st_mode):
            raise ValueError('Engineering turn requires ordinary retained directories')
        result[str(path)] = [info.st_dev, info.st_ino]
    return result


def _toml(value):
    if isinstance(value, dict):
        return "{ " + ", ".join(json.dumps(k) + " = " + _toml(v) for k, v in value.items()) + " }"
    if isinstance(value, list):
        return "[" + ", ".join(map(_toml, value)) + "]"
    if isinstance(value, (str, bool, int, float)):
        return json.dumps(value, ensure_ascii=False, allow_nan=False)
    raise ValueError("Unsupported engineering provider configuration value")


def seed(runtime: Path, *, source_home: Path | None = None, work_dir: Path | None = None) -> Path:
    source = source_home or Path.home()
    if os.environ.get("CODEX_HOME") and Path(os.environ["CODEX_HOME"]).resolve() != (source / ".codex").resolve():
        raise ValueError("Engineering delivery requires the standard sanctioned Codex home")
    # Keep the sanctioned authentication path. Governing rules get private bytes.
    home = seed_burst_home(runtime, "engineering-" + uuid.uuid4().hex, source_home=source,
                           subdirs=(), root_files=(".codex/auth.json",))
    from trellis.agents.engineering_exec import tree_identity, open_regular
    from trellis.modeling.history import _put
    rules = source / ".codex/rules"
    destination = home / ".codex/rules"
    destination.mkdir(parents=True)
    try:
        rules.lstat()
    except FileNotFoundError:
        pass
    else:
        original = tree_identity(rules)
        for name, identity in original.items():
            with open_regular(rules / name) as stream:
                _put(destination / name, stream.read())
            (destination / name).chmod(identity["mode"])
        if tree_identity(rules) != original or tree_identity(destination) != original:
            raise ValueError("Governing rules changed during isolated copying")
    path = source / ".codex/config.toml"
    config = tomllib.loads(path.read_text()) if path.exists() else {}
    unknown = set(config) - CONTEXT_KEYS - RETAINED_KEYS - {"features"}
    if unknown:
        raise ValueError("Engineering context cannot preserve these provider settings: " + ", ".join(sorted(unknown)))
    if set(config.get("features", {})) - FEATURES.keys() - POLICY_FEATURES:
        raise ValueError("Engineering context requires review of additional provider feature settings")
    projected = {k: v for k, v in config.items() if k in RETAINED_KEYS}
    if "projects" in projected:
        if work_dir is None:
            raise ValueError("Engineering project policies require the actual delivery directory")
        projected["projects"] = {name: policy for name, policy in projected["projects"].items()
                                 if work_dir.resolve().is_relative_to(Path(name).expanduser().resolve())}
    # Preserve policy-bearing feature settings; only context additions change.
    projected["features"] = {**config.get("features", {}), **FEATURES}
    projected["project_doc_max_bytes"] = 0
    raw = "".join(json.dumps(k) + " = " + _toml(v) + "\n" for k, v in projected.items())
    if tomllib.loads(raw) != projected:
        raise ValueError("Engineering provider configuration did not roundtrip")
    (home / ".codex").mkdir(exist_ok=True)
    with provider_directory(home) as (directory, fd):
        atomic_text(directory, fd, "config.toml", raw)
    return home


def validate(home: Path, provider=None, *, request=None) -> None:
    if provider is not None and (provider.provider != "codex" or provider.extra_args):
        raise ValueError("Engineering delivery requires Codex without additional context overrides")
    excluded = EXCLUDED_HOME_PATHS
    if request and request.get('native_record'):
        single, submission, _ = native_request(Path(request['native_record']), Path(request['request']['work_dir']), home)
        if request['request'] != single.to_dict() or (provider is not None and provider != single.provider):
            raise ValueError('Engineering native provider request changed')
        if single.resume_thread_id:
            retained_session(single, submission)
            excluded = tuple(name for name in excluded if name != 'sessions')
    config = tomllib.loads((home / ".codex/config.toml").read_text())
    if (set(config) - RETAINED_KEYS - {"features", "project_doc_max_bytes"}
            or set(config.get("features", {})) - FEATURES.keys() - POLICY_FEATURES
            or config.get("project_doc_max_bytes") != 0
            or any(config.get("features", {}).get(k) != v for k, v in FEATURES.items())
            or any(os.path.lexists(home / ".codex" / name) for name in excluded)):
        raise ValueError("Required engineering context exclusions are absent")


def runtime(environment=None) -> tuple[str, list[Path]]:
    from trellis.host_runtime import runtime_roots_for_command
    env = os.environ if environment is None else environment
    executable = shutil.which("codex", path=env.get("PATH", os.defpath))
    if not executable:
        raise ValueError("Engineering Codex executable is unavailable")
    executable = Path(executable).resolve(strict=True)
    from trellis.agents.model_preflight import native_executable_sha256
    native_executable_sha256(executable)
    cargo = env.get("TRELLIS_ENGINEERING_CARGO")
    roots = list(runtime_roots_for_command(str(executable)))
    bins = [str(executable.parent)]
    if cargo:
        tool = Path(cargo).resolve(strict=True)
        if (tool.name != "cargo" or tool.parent.name != "bin" or not (tool.parent / "rustc").is_file()
                or not (tool.parent.parent / "lib/rustlib").is_dir()):
            raise ValueError("Engineering requires an installed Rust toolchain binary directory")
        roots.append(tool.parent.parent)
        bins.append(str(tool.parent))
    return os.pathsep.join([*bins, "/usr/local/bin", "/usr/bin", "/bin"]), roots


def environment(home: Path, inherited=None, *, work: Path | None = None) -> dict[str, str]:
    from trellis.agents.engineering_exec import project_environment
    inherited = os.environ if inherited is None else inherited
    path, _ = runtime(inherited)
    return project_environment(inherited, str(home), path, str(work) if work is not None else None)


def admit_command(argv: list[str], *, home: Path, work: Path, request: dict,
                  inherited=None, files=()) -> list[str]:
    """Freeze one exact namespace invocation outside its mutable delivery view."""
    from trellis.agents import engineering_exec as leaf
    from trellis.agents.host_files import safe_directory
    validate(home, request=request)
    if not request.get("purpose") or "sequence" not in request or "request" not in request:
        raise ValueError("Engineering launch requires request, purpose and sequence")
    inherited = dict(os.environ) if inherited is None else inherited
    path, _ = runtime(inherited)
    argv = [str(Path(shutil.which(argv[0], path=os.defpath)).resolve(strict=True)), *argv[1:]]
    entrypoint = Path(leaf.__file__).resolve()
    from trellis import git_trust
    identities = {entrypoint, Path(git_trust.__file__).resolve(), Path("/usr/bin/python3").resolve(), Path(argv[0]),
                  Path(shutil.which("codex", path=path)).resolve()}
    links = {}
    for item in map(Path, files):
        # Chat helpers deliberately publish a log alias to the canonical prompt.
        for part in [*reversed(item.parents), item]:
            if part.is_symlink():
                links[str(part)] = os.readlink(part)
        identities.add(item.resolve(strict=True))
    cargo = inherited.get("TRELLIS_ENGINEERING_CARGO")
    if cargo:
        identities.update((Path(cargo).resolve(), Path(cargo).resolve().with_name("rustc")))
    for item in argv[1:]:
        candidate = Path(item)
        if candidate.is_absolute() and candidate.suffix in {".sh", ".py"} and candidate.is_file():
            identities.add(candidate)
    for name in ("config.toml", "trellis-engineering-base-instructions.md"):
        candidate = home / ".codex" / name
        if candidate.exists():
            identities.add(candidate)
    trees = [{"path": str(root), "ignored": ignored, "files": leaf.tree_identity(root, ignored)}
             for root, ignored in ((home / ".codex/rules", []), (work, [".trellis", "_CHATS"]))]
    absent = list(EXCLUDED_HOME_PATHS)
    host_files = []
    if request.get('native_record'):
        from trellis.agent_wrapper.provider_clock import engineering_host_files
        host_files = engineering_host_files()
        identities.update(host_files)
        single, submission, checked = native_request(Path(request['native_record']), work, home)
        identities.update(map(Path, checked))
        if single.resume_thread_id:
            retained, retained_files = retained_session(single, submission)
            trees.append(dict(path=str(home / '.codex/sessions'), ignored=[], files=retained['tree']))
            identities.update(retained_files)
            absent.remove('sessions')
    binding = {"schema": "engineering-launch-v1", "argv": argv, "request": request,
        "home": str(home), "work": str(work), "runtime_path": path,
        "environment_sha256": leaf.digest(leaf.project_environment(inherited, str(home), path, str(work))),
        "files": {str(p): leaf.file_identity(p) for p in sorted(identities)}, "trees": trees, "links": links,
        "absent": [str(home / ".codex" / name) for name in absent],
        "directories": {str(p): [p.stat().st_dev, p.stat().st_ino] for p in (home, home / ".codex", work)}}
    if host_files:
        binding['host_files'] = [str(path) for path in host_files]
        from trellis.git_trust import read_engineering_json
        saved, _ = read_engineering_json(Path(request['native_record']), limit=64 * 1024 * 1024)
        binding['directories'].update(saved['directories'])
    raw = json.dumps(binding, sort_keys=True, indent=2) + "\n"
    with safe_directory(home.parent / "engineering-admissions") as (directory, fd):
        saved = atomic_text(directory, fd, uuid.uuid4().hex + ".json", raw)
    return ["/usr/bin/python3", "-I", "-S", str(entrypoint), "--binding", str(saved),
            "--binding-sha256", hashlib.sha256(raw.encode()).hexdigest(), "--", *argv]


def native_request(path: Path, work: Path, home: Path):
    """Read the immutable shared-wrapper record against the current native turn."""
    from trellis.agent_wrapper.protocol import SingleAgentRequest
    from trellis.agent_wrapper.worker_submission import engineering_turn, native_digest
    from trellis.agents.engineering_exec import file_identity
    from trellis.git_trust import read_engineering_json
    record, _ = read_engineering_json(path, limit=64 * 1024 * 1024)
    if set(record) != {'request', 'submission', 'checker_files', 'directories'}:
        raise ValueError('Engineering native turn record is malformed')
    single = SingleAgentRequest.from_dict(record['request'])
    submission = record['submission']
    contract, _ = engineering_turn(single, submission)
    if (json.loads(json.dumps(single.to_dict())) != record['request']
            or single.work_dir != work or single.burst_home != home
            or single.execution_record is None or path != single.execution_record.parent / 'engineering-request.json'
            or single.execution_record != Path(submission['execution_record'])
            or not single.require_turn_completion or single.transport_resume
            or single.timeout_seconds != submission['remaining_ms'] / 1000
            or single.resume_thread_id != (submission.get('session') or {}).get('thread_id', '')
            or single.fresh != (not bool(single.resume_thread_id)) or single.session_record is None):
        raise ValueError('Engineering native turn identity changed')
    if record['directories'] != turn_directories(work, home):
        raise ValueError('Engineering retained turn directories changed')
    for protected in (path, single.execution_record, single.session_record, Path(submission['native_runtime'])):
        if not protected.is_absolute() or any(protected.is_relative_to(root) for root in (work, home)):
            raise ValueError('Engineering turn evidence must remain outside the provider view')
    directory = work / '.trellis/worker-checks'
    name = str(submission['engineering_request']['id']) + '-' + str(submission['turn'])
    files = record['checker_files']
    expected = {str(directory / name) for name in ('engineering-check.py', name + '.json', name + '.sh')}
    if (not isinstance(files, dict) or set(files) != expected
            or any(file_identity(Path(p)) != identity for p, identity in files.items())
            or files[str(directory / 'engineering-check.py')]['sha256'] != contract['checker_sha256']
            or files[str(directory / (name + '.json'))]['sha256'] != contract['artifact_contract_sha256']):
        raise ValueError('Engineering native checker files changed')
    return single, submission, files


def request_binding(work: Path, home: Path, execution_record: Path | None = None) -> tuple[dict, list[Path]]:
    from trellis.agents.engineering_exec import open_regular
    if execution_record is not None:
        path = execution_record.parent / 'engineering-request.json'
        single, submission, files = native_request(path, work, home)
        return {"request": single.to_dict(), "purpose": submission['engineering_request']['engineering']['purpose'],
                "sequence": single.cycle, "native_record": str(path)}, [path, *map(Path, files),
                    Path(submission['native_runtime']) / 'protocol_state.json']
    path = work.parent / "request.json"
    with open_regular(path) as stream:
        request = json.load(stream)
    if (request.get("kind") != "engineering" or not request.get("fresh")
            or request.get("work_dir") != str(work) or request.get("burst_home") != str(home)
            or request.get("resume_thread_id") or request.get("transport_resume")):
        raise ValueError("Engineering request identity changed")
    return {"request": request, "purpose": request["request_id"].rsplit("-", 1)[-1],
            "sequence": request["cycle"]}, [path, work.parent / "basis.json"]


def _session_tree(single, session):
    from trellis.agents.engineering_exec import tree_identity
    from trellis.agent_wrapper.continuation import require_session
    from trellis.codex_timing import find_rollout_path
    require_session(single.session_record, session)
    root = single.burst_home / '.codex/sessions'
    tree = tree_identity(root)
    suffix = '-' + session['thread_id'] + '.jsonl'
    principals = [name for name in tree
                  if Path(name).name.startswith('rollout-') and Path(name).name.endswith(suffix)]
    rollout = find_rollout_path(session['thread_id'], sessions_root=root)
    if len(principals) != 1 or rollout is None or rollout.relative_to(root).as_posix() != principals[0]:
        raise ValueError('Engineering session requires one matching principal rollout')
    return tree


def retain_completed_session(single, submission, command):
    """Supplemental execution evidence; no transition or import authority."""
    from trellis.agent_wrapper import provider_clock
    from trellis.agent_wrapper.continuation import session_binding
    from trellis.agent_wrapper.worker_submission import native_digest
    from trellis.agents.engineering_exec import file_identity
    from trellis.agents.host_files import safe_directory
    from trellis.git_trust import engineering_launch_status, report_directory, _write_result, read_engineering_json
    clock = single.execution_record
    finished, exited = provider_clock.read(clock, 'finished'), provider_clock.read(clock, 'exited')
    if (not finished or finished['disposition'] != 'completed' or not exited or exited['returncode'] != 0
            or engineering_launch_status(command) != (0, 'launcher_exited')):
        raise ValueError('Engineering session requires completed owned execution')
    session = session_binding(single.provider, single.burst_home, finished['thread_id'])
    record = clock.parent / 'engineering-request.json'
    paths = [record, *(clock / (name + '.json') for name in ('config', 'started', 'finished', 'exited')),
             report_directory(command) / 'launch.json']
    receipt = dict(request_sha256=native_digest(submission['engineering_request']),
        contract_sha256=submission['engineering_attempt']['contract_sha256'], turn=submission['turn'],
        session=session, tree=_session_tree(single, session), execution_record=str(clock),
        launch_identity=command[7], files={str(p): file_identity(p) for p in paths},
        directories=read_engineering_json(record, limit=64 * 1024 * 1024)[0]['directories'])
    if receipt['directories'] != turn_directories(single.work_dir, single.burst_home):
        raise ValueError('Engineering turn directories changed during execution')
    path = single.session_record.parent / f"engineering-session-{submission['turn']}.json"
    with safe_directory(path.parent):
        _write_result(path, receipt, exclusive=True)
    if read_engineering_json(path)[0] != receipt:
        raise ValueError('Engineering completed session receipt changed')


def retained_session(single, submission):
    from trellis.agent_wrapper.worker_submission import native_digest
    from trellis.agents.engineering_exec import file_identity
    from trellis.git_trust import read_engineering_json, read_engineering_receipt
    path = single.session_record.parent / f"engineering-session-{submission['turn'] - 1}.json"
    receipt, _ = read_engineering_json(path)
    fields = {'request_sha256', 'contract_sha256', 'turn', 'session', 'tree', 'execution_record', 'launch_identity', 'files', 'directories'}
    if (set(receipt) != fields or receipt['turn'] != submission['turn'] - 1
            or receipt['request_sha256'] != native_digest(submission['engineering_request'])
            or receipt['contract_sha256'] != submission['engineering_attempt']['contract_sha256']
            or receipt['session'] != submission['session']
            or receipt['tree'] != _session_tree(single, submission['session'])
            or receipt['directories'] != turn_directories(single.work_dir, single.burst_home)):
        raise ValueError('Engineering retained session differs from its completed turn')
    clock = Path(receipt['execution_record'])
    files = receipt['files']
    required = {str(clock.parent / 'engineering-request.json'),
                *(str(clock / (name + '.json')) for name in ('config', 'started', 'finished', 'exited'))}
    if (not isinstance(files, dict) or not required < files.keys() or len(files) != len(required) + 1
            or any(file_identity(Path(p)) != identity for p, identity in files.items())):
        raise ValueError('Engineering retained execution evidence changed')
    terminal_path = Path(next(iter(files.keys() - required)))
    terminal = read_engineering_receipt(terminal_path, receipt['launch_identity'], {'launcher_exited'})
    prior, _ = read_engineering_json(clock.parent / 'engineering-request.json', limit=64 * 1024 * 1024)
    from trellis.agent_wrapper import provider_clock
    finished, exited = provider_clock.read(clock, 'finished'), provider_clock.read(clock, 'exited')
    if (terminal['exit_code'] != 0 or prior['submission']['turn'] != receipt['turn']
            or prior['submission']['engineering_request'] != submission['engineering_request']
            or prior['request']['execution_record'] != str(clock)
            or prior['request']['burst_home'] != str(single.burst_home)
            or not finished or finished['disposition'] != 'completed' or not exited or exited['returncode'] != 0
            or finished['thread_id'] != submission['session']['thread_id']):
        raise ValueError('Engineering retained execution did not complete')
    return receipt, [path, *map(Path, files)]


def launch_guards(command: list[str]) -> list[str]:
    """Pin trusted code before Python executes it, including a delayed tmux start."""
    from trellis.agents.engineering_exec import open_regular
    if len(command) < 10 or command[:3] != ["/usr/bin/python3", "-I", "-S"] or command[4] != "--binding":
        return []
    with open_regular(Path(command[5])) as stream:
        raw = stream.read()
    if hashlib.sha256(raw).hexdigest() != command[7]:
        raise ValueError("Engineering launch binding changed before launcher publication")
    binding = json.loads(raw)
    if binding["argv"] != command[9:]:
        raise ValueError("Engineering launch command changed before launcher publication")
    from trellis import git_trust
    from trellis.agents.host_files import safe_directory
    directory = git_trust.report_directory(command)
    with safe_directory(directory) as (directory, _):
        git_trust._write_result(directory / 'ready.json',
            git_trust.engineering_receipt('recording_ready', command[7]), first=True)
    target = shlex.quote(str(directory / 'launch.json'))
    temporary = shlex.quote(str(directory / '.guard-XXXXXX'))
    # These builtins/coreutils record refusal even when Python's guard fails.
    record = '{"code":"guard_refused","identity":"' + command[7] + '","exit_code":125,"uid":%s,"euid":%s}\\n'
    guards = [
        'trellis_guard_refusal() {',
        '  local receipt',
        '  receipt=$(/usr/bin/mktemp -- ' + temporary + ') || exit 125',
        '  printf ' + shlex.quote(record) + ' "$UID" "$EUID" > "$receipt" || exit 125',
        '  /usr/bin/ln -- "$receipt" ' + target + ' 2>/dev/null || true',
        '  /usr/bin/rm -f -- "$receipt"',
        '  exit 125',
        '}',
        'test -f ' + shlex.quote(str(directory / 'ready.json')) + ' && test ! -e ' + target +
        ' && test ! -e ' + shlex.quote(str(directory / 'claimed')) + ' || exit 125',
    ]
    for path in (command[0], command[3], str(Path(git_trust.__file__).resolve()), *binding.get('host_files', ())):
        expected = binding["files"][str(Path(path).resolve())]["sha256"]
        guards.append("test \"$(/usr/bin/sha256sum -- " + shlex.quote(path) +
                      " 2>/dev/null | /usr/bin/cut -d ' ' -f 1)\" = " + shlex.quote(expected) + " || trellis_guard_refusal")
    return guards
