"""Atomic external-tool and fingerprint actions for the checker boundary.

These helpers only gather raw facts for the Rust kernel. They do not decide
validity, classify failures, or synthesize acceptance results.
"""

from __future__ import annotations

import os
import re
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any, Dict, Optional, Sequence

from trellis.atomic_actions.checker_client import (
    _resolve_socket_path,
    client_build_tablet,
    client_compile_node,
    client_lean_semantic_payloads,
    client_materialize_tablet_oleans,
    client_prepare_compiled_support,
    client_print_axioms,
)
from trellis.project_paths import repo_tmp_subdir
from trellis.supervisor_workspace import authoritative_env_for_repo

_TABLET_IMPORT_RE = re.compile(r"^\s*import\s+Tablet\.([A-Za-z0-9_']+)\s*$", re.MULTILINE)
_BARE_MATHLIB_IMPORT_RE = re.compile(r"^\s*import\s+Mathlib\s*$")
LEAN_SUPPORT_TIMEOUT_SECS = 3600.0

# Sub-progress channel for the worker-side acceptance checker.
#
# The kernel binary emits top-level `[acceptance] phase k/6: ...` lines to
# its own stderr, which `trellis.runtime.kernel_cli._run_kernel_cli_once`
# forwards line-by-line to the calling agent. Inside phase 6/6 ("hydrate
# response and legality check") the kernel shells back out to Python
# observations (this module) via `python3 .../check.py materialize-tablet-oleans`
# and `python3 .../check.py lean-semantic-payloads`, each of which loops
# over nodes for many seconds per node. The kernel's spawn captures the
# child Python's stderr into a buffer (Stdio::piped + cmd.output), so a
# plain `print(..., file=sys.stderr)` from this module is INVISIBLE to the
# calling agent.
#
# To surface per-node sub-progress without rebuilding the kernel binary,
# `_run_kernel_cli_once` creates a tail-and-forward log file before
# spawning the kernel and exports its path via the
# `TRELLIS_ACCEPTANCE_PROGRESS_LOG` env var. The kernel inherits this env
# (no env_clear in run_repo_command_json), so the python3 child here also
# sees it. `_progress_emit` appends one line per call to that file; the
# outer Python tail thread forwards each new line to the agent's stderr
# in real time. When the env var is unset (direct CLI use, tests,
# fast-mode), `_progress_emit` is a no-op.
_PROGRESS_LOG_ENV = "TRELLIS_ACCEPTANCE_PROGRESS_LOG"


def _require_socket_path() -> str:
    """Resolve the checker socket or raise if it is unset.

    Acceptance observations (the six public fns below, when called with
    ``bwrap_role is None``) are socket-mandatory: the unified-checker
    UNIX-socket server is the only supported way to run authoritative
    acceptance lake checks. The legacy direct-host-lake fallback has been
    removed. ``restart_configured_run.sh`` launches the server and exports
    ``TRELLIS_CHECKER_SOCKET``, so in any real run the socket resolves. If it is unset, that is an
    operator misconfiguration — surface it loudly rather than silently
    running lake on the host (which bypasses the server's authority and
    confinement). Mirrors the no-fallback precedent for
    ``local-closure-axioms`` in :mod:`trellis.atomic_actions.cli`.
    """
    socket_path = _resolve_socket_path()
    if socket_path is None:
        raise RuntimeError(
            "checker socket required: TRELLIS_CHECKER_SOCKET unset. "
            "Acceptance lake checks must route through the supervisor-side "
            "checker server (no host-lake fallback). Launch the run via "
            "scripts/restart_configured_run.sh, "
            "which starts the server and exports TRELLIS_CHECKER_SOCKET."
        )
    return socket_path


def _progress_emit(message: str) -> None:
    """Best-effort append `message` to the acceptance progress log file.

    No-op when `TRELLIS_ACCEPTANCE_PROGRESS_LOG` is unset (which is the
    case for direct CLI invocations, unit tests, and fast-mode runs).
    Errors are swallowed: progress logging must never break the underlying
    observation. The file is line-buffered so the parent's tail thread
    sees each line as soon as we flush.
    """
    path = os.environ.get(_PROGRESS_LOG_ENV, "").strip()
    if not path:
        return
    try:
        with open(path, "a", encoding="utf-8", buffering=1) as handle:
            handle.write(message.rstrip("\n") + "\n")
    except OSError:
        # The log file may have been cleaned up by the parent already; in
        # that case we just drop the line — the parent has already moved
        # on, so there is nobody to read it.
        pass


def _completed_process_payload(
    proc: subprocess.CompletedProcess[str],
) -> Dict[str, Any]:
    return {
        "returncode": proc.returncode,
        "stdout": proc.stdout,
        "stderr": proc.stderr,
        "timed_out": False,
        "spawn_error": "",
    }


def _timeout_payload(timeout_secs: float, *, command: str) -> Dict[str, Any]:
    message = f"command timed out after {timeout_secs}s: {command}"
    return {
        "returncode": None,
        "stdout": "",
        "stderr": message,
        "timed_out": True,
        "spawn_error": message,
    }


def _spawn_error_payload(exc: Exception, *, command: str) -> Dict[str, Any]:
    return {
        "returncode": None,
        "stdout": "",
        "stderr": f"failed to start command {command}: {exc}",
        "timed_out": False,
        "spawn_error": str(exc),
    }


def _run_lake_command(
    repo: Path,
    args: Sequence[str],
    *,
    timeout_secs: float,
    bwrap_role: Optional[str] = None,
) -> Dict[str, Any]:
    inner_cmd: list[str] = ["lake", *list(args)]
    # Threat-model mitigation 1: when invoked from the supervisor-side
    # checker server, route lake through wrap_command(role="lake_compiler")
    # so an elaborator RCE is bwrap-confined to the supervisor workspace
    # build outputs + Tablet/. The current direct-call path (worker burst's
    # outer bwrap'd lake; tests; supervisor pre-warm) is unchanged with
    # bwrap_role=None.
    if bwrap_role is not None:
        from trellis.config import SandboxConfig
        from trellis.sandbox import bwrap_available, wrap_command
        from trellis.project_paths import supervisor_workspace_home_path

        if not bwrap_available():
            return _spawn_error_payload(
                RuntimeError(
                    "bwrap is required for lake_compiler role but is not installed"
                ),
                command=" ".join(inner_cmd),
            )
        # supervisor_workspace.py lays out the workspace as
        #   <worker_repo>/.trellis/supervisor/{repo,home,cache}
        # so when the lake_compiler role is invoked, ``repo`` is conventionally
        # ``<worker>/.trellis/supervisor/repo`` and HOME lives at
        # ``<worker>/.trellis/supervisor/home``. Be tolerant: if the marker
        # path doesn't yet exist (e.g. tests with a synthetic repo) fall back
        # to the canonical helper which derives HOME from the worker repo.
        supervisor_home: Path
        sibling_home = repo.parent / "home" if repo.name == "repo" else None
        if sibling_home is not None and sibling_home.exists():
            supervisor_home = sibling_home
        else:
            supervisor_home = supervisor_workspace_home_path(repo)
            try:
                supervisor_home.mkdir(parents=True, exist_ok=True)
            except OSError:
                pass
        try:
            inner_cmd = wrap_command(
                inner_cmd,
                sandbox=SandboxConfig(enabled=True, backend="bwrap"),
                work_dir=repo,
                burst_home=supervisor_home,
                role=bwrap_role,
            )
            # `wrap_command` adds `--ro-bind <elan_home> <elan_home>` via
            # host_runtime_readonly_roots, but inside the bwrap HOME is
            # rebound to <supervisor_home> so `elan` can't find ~/.elan
            # by default. Make ELAN_HOME explicit so lake locates the
            # toolchain at the read-only mount instead of trying to
            # download it into <supervisor_home>/.elan.
            #
            # NOTE on multi-toolchain exposure: ELAN_HOME points at the
            # shared elan installation, which contains every toolchain
            # trellis can resolve. lake itself selects the toolchain
            # via the workspace's ``lean-toolchain`` file, so a confined
            # supervisor-side lake is still pinned to the workspace's
            # declared version even though the elan tree as a whole
            # holds others.
            from trellis.host_runtime import worker_elan_home

            elan_home = str(worker_elan_home())
            lean_threads = (
                os.environ.get("TRELLIS_LEAN_PARALLELISM", "6").strip() or "6"
            )
            inner_cmd = inner_cmd[:1] + [
                "--setenv", "ELAN_HOME", elan_home,
                "--setenv", "LEAN_NUM_THREADS", lean_threads,
            ] + inner_cmd[1:]
        except Exception as exc:
            return _spawn_error_payload(exc, command=" ".join(["lake", *list(args)]))

    command = " ".join(["lake", *list(args)])
    try:
        env = os.environ.copy()
        env.update(authoritative_env_for_repo(repo))
        _inject_git_safe_directories(env, repo)
        # Cap lake's task-pool (and per-process Lean elaborator pool) at the
        # supervisor's configured parallelism. Lake schedules build jobs as
        # Lean tasks; LEAN_NUM_THREADS bounds that scheduler. Default 6.
        env["LEAN_NUM_THREADS"] = (
            os.environ.get("TRELLIS_LEAN_PARALLELISM", "6").strip() or "6"
        )
        if bwrap_role is not None:
            # `inner_cmd` is now `bwrap … lake …`; bwrap resolves the OUTER `lake`
            # via this env's PATH (wrap_command adds no `--setenv PATH`). The
            # checker server may be launched from a shell without elan on PATH,
            # which surfaces as a cryptic `bwrap: execvp lake: No such file or
            # directory`. Prepend the resolved elan bin (independent of the
            # launching shell's PATH) so lake always resolves — mirroring the
            # worker side, which already runs lake under `worker_path_env`.
            from trellis.host_runtime import worker_elan_home

            elan_bin = str(worker_elan_home() / "bin")
            existing_path = env.get("PATH", "")
            if elan_bin not in existing_path.split(os.pathsep):
                env["PATH"] = (
                    f"{elan_bin}{os.pathsep}{existing_path}" if existing_path else elan_bin
                )
        proc = subprocess.run(
            inner_cmd,
            capture_output=True,
            text=True,
            cwd=str(repo),
            env=env,
            timeout=timeout_secs,
        )
        return _completed_process_payload(proc)
    except subprocess.TimeoutExpired:
        return _timeout_payload(timeout_secs, command=command)
    except FileNotFoundError as exc:
        return _spawn_error_payload(exc, command=command)


def _shared_build_roots(repo: Path) -> list[Path]:
    roots: list[Path] = []
    main_root = repo / ".lake" / "build"
    if main_root.exists():
        roots.append(main_root)
    packages_root = repo / ".lake" / "packages"
    if packages_root.exists():
        for build_root in packages_root.glob("*/.lake/build"):
            if build_root.exists():
                roots.append(build_root)
    return roots


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


def _normalized_mode(path: Path, mode: int) -> int:
    execute_bits = mode & 0o111
    if path.is_dir():
        return 0o2775
    if execute_bits:
        return 0o775
    return 0o664


def _normalize_recent_shared_build_artifacts(repo: Path, *, since_ns: int) -> None:
    for root in _shared_build_roots(repo):
        for current_root, dirnames, filenames in os.walk(root):
            current_path = Path(current_root)
            try:
                current_stat = current_path.stat()
            except OSError:
                continue
            if current_stat.st_mtime_ns >= since_ns:
                try:
                    os.chmod(current_path, _normalized_mode(current_path, current_stat.st_mode & 0o777))
                except OSError:
                    pass
            for name in dirnames:
                path = current_path / name
                try:
                    st = path.stat()
                except OSError:
                    continue
                if st.st_mtime_ns < since_ns:
                    continue
                try:
                    os.chmod(path, _normalized_mode(path, st.st_mode & 0o777))
                except OSError:
                    pass
            for name in filenames:
                path = current_path / name
                try:
                    st = path.stat()
                except OSError:
                    continue
                if st.st_mtime_ns < since_ns:
                    continue
                try:
                    os.chmod(path, _normalized_mode(path, st.st_mode & 0o777))
                except OSError:
                    pass


def _tablet_lean_path(repo: Path, node_name: str) -> Path:
    return repo / "Tablet" / f"{node_name}.lean"


def _tablet_olean_path(repo: Path, node_name: str) -> Path:
    return repo / ".lake" / "build" / "lib" / "lean" / "Tablet" / f"{node_name}.olean"


def _direct_tablet_imports(repo: Path, node_name: str) -> list[str]:
    lean_path = _tablet_lean_path(repo, node_name)
    if not lean_path.exists():
        return []
    content = lean_path.read_text(encoding="utf-8", errors="replace")
    return [match.group(1) for match in _TABLET_IMPORT_RE.finditer(content)]


def materialization_order(
    repo: Path,
    requested_nodes: Sequence[str],
) -> list[str]:
    order: list[str] = []
    visited: set[str] = set()

    def visit(node_name: str) -> None:
        cleaned = str(node_name).strip()
        if not cleaned or cleaned in visited:
            return
        visited.add(cleaned)
        for dep in _direct_tablet_imports(repo, cleaned):
            visit(dep)
        order.append(cleaned)

    for node_name in requested_nodes:
        visit(node_name)
    return order


def _find_bare_mathlib_imports(
    repo: Path,
    node_names: Sequence[str],
) -> list[tuple[str, int]]:
    """Walk the transitive Tablet import closure of ``node_names`` and return
    every (node_name, line_number) where a bare ``import Mathlib`` appears.
    A bare Mathlib import pulls the entire library into the import graph and
    makes every dependent compile pay ~80–150s in olean loading."""
    closure = materialization_order(repo, node_names)
    findings: list[tuple[str, int]] = []
    for node_name in closure:
        lean_path = _tablet_lean_path(repo, node_name)
        if not lean_path.exists():
            continue
        try:
            text = lean_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            if _BARE_MATHLIB_IMPORT_RE.match(line):
                findings.append((node_name, lineno))
    return findings


def _bare_mathlib_error_message(findings: list[tuple[str, int]]) -> str:
    locs = ", ".join(f"Tablet/{name}.lean:{lineno}" for name, lineno in findings)
    return (
        f"FAIL: bare `import Mathlib` is not allowed. Replace it with the specific "
        f"`Mathlib.*` modules you actually use (e.g. `import Mathlib.Data.Real.Basic`, "
        f"`import Mathlib.Topology.MetricSpace.Pseudo.Lemmas`). Bare Mathlib pulls in "
        f"the entire library and inflates every dependent compile to ~80–150s. "
        f"Found in: {locs}"
    )


def materialize_tablet_oleans(
    repo: Path,
    node_names: Sequence[str],
    *,
    timeout_secs: float = LEAN_SUPPORT_TIMEOUT_SECS,
    bwrap_role: Optional[str] = None,
) -> Dict[str, Any]:
    # Acceptance routing: when called with ``bwrap_role is None`` (the
    # kernel-invoked acceptance path), the checker socket is mandatory —
    # route through the supervisor-side checker server. See ``compile_node``
    # for the design rationale (recursion guard via ``bwrap_role``,
    # no host-lake fallback). The direct ``_run_lake_command`` path below is
    # reachable only for explicit ``bwrap_role`` values (e.g. the server's
    # own ``"lake_compiler"`` endpoint).
    if bwrap_role is None:
        socket_path = _require_socket_path()
        response = client_materialize_tablet_oleans(
            socket_path,
            repo,
            list(node_names),
            timeout_secs=timeout_secs,
        )
        result = dict(response)
        result.pop("request_id", None)
        return result

    requested = [str(name).strip() for name in node_names if str(name).strip()]
    if not requested:
        requested = sorted(
            path.stem
            for path in (repo / "Tablet").glob("*.lean")
            if path.stem != "Axioms"
        )
    bare_mathlib = _find_bare_mathlib_imports(repo, requested)
    if bare_mathlib:
        return {
            "requested_nodes": requested,
            "materialized_nodes": [],
            "returncode": 2,
            "stdout": "",
            "stderr": _bare_mathlib_error_message(bare_mathlib),
            "timed_out": False,
            "spawn_error": "",
        }
    order = materialization_order(repo, requested)
    output_dir = repo / ".lake" / "build" / "lib" / "lean" / "Tablet"
    output_dir.mkdir(parents=True, exist_ok=True)

    total = len(order)
    materialize_started = time.time()
    _progress_emit(
        f"[acceptance]   materialize-tablet-oleans: starting batched build on {total} node(s)"
    )

    if not order:
        # Degenerate case: nothing to build. Surface a successful no-op so
        # the caller's contract (returncode==0 means "everything you asked
        # for is current") still holds.
        _progress_emit(
            "[acceptance]   materialize-tablet-oleans: no nodes to build"
        )
        return {
            "requested_nodes": requested,
            "materialized_nodes": [],
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    # Single batched lake build: lake itself parallelises across targets
    # using its own job graph, so a single invocation is strictly faster
    # than the per-node loop (paid one lake startup, not N). The per-node
    # `[Foo]\n...` stdout/stderr tagging is dropped since it's no longer
    # meaningful for a batched build; nothing parses it (kernel reads
    # the structured `materialized_nodes` field, not stdout text).
    targets = [f"Tablet.{name}" for name in order]
    build_started_ns = time.time_ns()
    result = _run_lake_command(
        repo,
        ["build", *targets],
        timeout_secs=timeout_secs,
        bwrap_role=bwrap_role,
    )

    last_returncode = result.get("returncode")
    timed_out = bool(result.get("timed_out"))
    spawn_error = str(result.get("spawn_error") or "")
    stdout = str(result.get("stdout", "") or "")
    stderr = str(result.get("stderr", "") or "")

    if not timed_out and not spawn_error:
        _normalize_recent_shared_build_artifacts(repo, since_ns=build_started_ns)

    # Stat-walk to determine which oleans are current. Even on a
    # nonzero returncode or timeout some nodes may have built before the
    # failure point; the structured set must reflect on-disk truth.
    # Walk the full closure (``order``) rather than just the originally
    # requested set: lake builds the entire dependency graph, and
    # callers (e.g. the kernel's ``ensure_*_tablet_support_available``)
    # rely on ``materialized_nodes`` listing every node that ended up
    # with a current olean — not just the ones the caller named.
    # Use the same source-mtime gate as the checker server's
    # ``_compute_compile_cache_subset``: olean exists, size>0, and
    # mtime>=source mtime. This admits closure dependencies whose
    # oleans lake left untouched because they were already current.
    materialized_nodes: list[str] = []
    for node_name in order:
        olean = _tablet_olean_path(repo, node_name)
        src = _tablet_lean_path(repo, node_name)
        try:
            ost = olean.stat()
            sst = src.stat()
        except (FileNotFoundError, OSError):
            continue
        if ost.st_size == 0:
            continue
        if ost.st_mtime_ns < sst.st_mtime_ns:
            # Olean predates its source — stale, don't claim current.
            continue
        materialized_nodes.append(node_name)

    _progress_emit(
        f"[acceptance]   materialize-tablet-oleans: done "
        f"{len(materialized_nodes)}/{total} in {time.time() - materialize_started:.1f}s "
        f"(returncode={last_returncode}, timed_out={timed_out})"
    )
    return {
        "requested_nodes": requested,
        "materialized_nodes": materialized_nodes,
        "returncode": last_returncode,
        "stdout": stdout,
        "stderr": stderr,
        "timed_out": timed_out,
        "spawn_error": spawn_error,
    }


def _lean_semantic_fingerprint_script_path() -> Path:
    return Path(__file__).resolve().parents[2] / "scripts" / "lean_semantic_fingerprint.lean"


def compile_node(
    repo: Path,
    node_name: str,
    *,
    timeout_secs: float = LEAN_SUPPORT_TIMEOUT_SECS,
    bwrap_role: Optional[str] = None,
) -> Dict[str, Any]:
    # Acceptance routing: when called with ``bwrap_role is None`` (the
    # kernel-invoked acceptance path), the checker socket is MANDATORY.
    # Route this observation through the supervisor-side checker server via
    # the RPC client. The server runs lake under bwrap_role="lake_compiler"
    # on the supervisor's authoritative repo and returns a payload identical
    # in shape to the direct-lake path. The unified-checker UNIX-socket
    # server is the only supported way to run acceptance lake checks; the
    # legacy direct-host-lake fallback has been removed.
    #
    # Recursion guard: the supervisor's checker server calls this
    # function with ``bwrap_role="lake_compiler"`` to invoke confined
    # lake on its own side. That call must NOT route through RPC again
    # — it's already on the authoritative side. The worker burst's
    # acceptance callers in cli.py / check.py omit the kwarg, so a missing
    # bwrap_role means "route through the socket" and the resolved-but-None
    # socket case is an operator misconfiguration that raises loudly (no
    # silent host-lake fallback that would bypass the server's authority
    # and confinement).
    if bwrap_role is None:
        socket_path = _require_socket_path()
        response = client_compile_node(
            socket_path,
            repo,
            node_name,
            timeout_secs=timeout_secs,
        )
        # The server includes ``request_id`` in its response
        # envelope; the direct-lake path does not. Strip it so the
        # dict shape matches exactly what callers see today.
        result = dict(response)
        result.pop("request_id", None)
        return result

    # Direct-lake path: reachable only for explicit ``bwrap_role`` values
    # (the server's own ``"lake_compiler"`` endpoint). Compile through the same
    # import-closure materialization path used by explicit supervisor
    # support hydration so newly introduced Tablet imports produce
    # reusable `.olean` artifacts before later policy checks inspect them.
    payload = materialize_tablet_oleans(
        repo,
        [node_name],
        timeout_secs=timeout_secs,
        bwrap_role=bwrap_role,
    )
    payload["node"] = node_name
    return payload


def build_tablet(
    repo: Path,
    *,
    timeout_secs: float = LEAN_SUPPORT_TIMEOUT_SECS,
    bwrap_role: Optional[str] = None,
) -> Dict[str, Any]:
    # Acceptance routing: socket-mandatory when ``bwrap_role is None``.
    # Route through the supervisor-side checker server. See ``compile_node``
    # for the design rationale (no host-lake fallback). The direct-lake path
    # below is reachable only for explicit ``bwrap_role`` values.
    if bwrap_role is None:
        socket_path = _require_socket_path()
        response = client_build_tablet(
            socket_path,
            repo,
            timeout_secs=timeout_secs,
        )
        result = dict(response)
        result.pop("request_id", None)
        return result

    started = time.time()
    _progress_emit("[acceptance]   build-tablet: starting")
    started_ns = time.time_ns()
    payload = _run_lake_command(
        repo,
        ["build", "Tablet"],
        timeout_secs=timeout_secs,
        bwrap_role=bwrap_role,
    )
    if not payload.get("timed_out") and not payload.get("spawn_error"):
        _normalize_recent_shared_build_artifacts(repo, since_ns=started_ns)
    _progress_emit(
        f"[acceptance]   build-tablet: done returncode={payload.get('returncode')} in {time.time() - started:.1f}s"
    )
    return payload


def prepare_compiled_support(
    repo: Path,
    *,
    timeout_secs: float = LEAN_SUPPORT_TIMEOUT_SECS,
    bwrap_role: Optional[str] = None,
) -> Dict[str, Any]:
    # Acceptance routing: socket-mandatory when ``bwrap_role is None``.
    # Route through the supervisor-side checker server. See ``compile_node``
    # for the design rationale (no host-lake fallback). The direct-lake path
    # below is reachable only for explicit ``bwrap_role`` values.
    if bwrap_role is None:
        socket_path = _require_socket_path()
        response = client_prepare_compiled_support(
            socket_path,
            repo,
            timeout_secs=timeout_secs,
        )
        result = dict(response)
        result.pop("request_id", None)
        return result

    stdout_parts: list[str] = []
    stderr_parts: list[str] = []
    steps_completed: list[str] = []
    last_returncode: Optional[int] = 0
    timed_out = False
    spawn_error = ""

    for step_name, args in (("cache_get", ["exe", "cache", "get"]),):
        started_ns = time.time_ns()
        result = _run_lake_command(
            repo,
            args,
            timeout_secs=timeout_secs,
            bwrap_role=bwrap_role,
        )
        if result.get("stdout"):
            stdout_parts.append(f"[{step_name}]\n{result['stdout']}")
        if result.get("stderr"):
            stderr_parts.append(f"[{step_name}]\n{result['stderr']}")
        last_returncode = result.get("returncode")
        if result.get("timed_out"):
            timed_out = True
            break
        if result.get("spawn_error"):
            spawn_error = str(result["spawn_error"])
            break
        _normalize_recent_shared_build_artifacts(repo, since_ns=started_ns)
        if result.get("returncode") not in (0, None):
            break
        steps_completed.append(step_name)

    return {
        "steps_completed": steps_completed,
        "returncode": last_returncode,
        "stdout": "\n".join(part for part in stdout_parts if part),
        "stderr": "\n".join(part for part in stderr_parts if part),
        "timed_out": timed_out,
        "spawn_error": spawn_error,
    }


def _axiom_probe_temp_dir(repo: Path) -> Path:
    for temp_dir in (
        repo_tmp_subdir(repo, "check"),
        repo / ".trellis" / "staging",
    ):
        try:
            temp_dir.mkdir(parents=True, exist_ok=True)
        except OSError:
            continue
        try:
            os.chmod(temp_dir, 0o2775)
        except OSError:
            pass
        if temp_dir.is_dir() and os.access(temp_dir, os.W_OK | os.X_OK):
            return temp_dir
    raise OSError(f"could not create repo-local axiom audit scratch dir under {repo}")


# Note on ``local_closure_axioms`` (LOCAL_CLOSURE_IMPL_PLAN.md Patch A):
# Unlike every other observation in this module, ``local_closure_axioms``
# is **server-only** (plan §5.7) and has **no host-lake fallback**. There
# is intentionally no Python wrapper for it here — Patch A's Python
# integration ends at the dispatch layer in
# :mod:`trellis.atomic_actions.cli` (subcommand
# ``local-closure-axioms``), which routes via
# :func:`trellis.atomic_actions.checker_client.client_local_closure_axioms`
# when ``TRELLIS_CHECKER_SOCKET`` is set and errors loudly otherwise.
# The Rust kernel (Patch B / Patch C) calls the subcommand directly via
# ``run_repo_command_json``; it does not import this module's Python
# surface for this op. Trust model rationale: per plan §2.3, the server
# derives ``repo_path`` from the socket's runtime root and rejects any
# worker-supplied ``repo_path`` (protocol.py:212-217), so a host-lake
# fallback would silently bypass that authority. Surface the missing
# socket as an error instead, matching the ``compile_node`` precedent
# above of refusing to mask operator misconfiguration with graceful
# fallback.


def print_axioms(
    repo: Path,
    node_name: str,
    *,
    timeout_secs: float = LEAN_SUPPORT_TIMEOUT_SECS,
    bwrap_role: Optional[str] = None,
) -> Dict[str, Any]:
    # Acceptance routing: socket-mandatory when ``bwrap_role is None``.
    # Route through the supervisor-side checker server. See ``compile_node``
    # for the design rationale (no host-lake fallback). The direct-lake path
    # below is reachable only for explicit ``bwrap_role`` values.
    if bwrap_role is None:
        socket_path = _require_socket_path()
        response = client_print_axioms(
            socket_path,
            repo,
            node_name,
            timeout_secs=timeout_secs,
        )
        result = dict(response)
        result.pop("request_id", None)
        return result

    temp_path: Optional[Path] = None
    try:
        temp_dir = _axiom_probe_temp_dir(repo)
        with tempfile.NamedTemporaryFile(
            mode="w",
            suffix=".lean",
            dir=str(temp_dir),
            prefix=f"axioms_{node_name}_",
            delete=False,
            encoding="utf-8",
        ) as handle:
            handle.write(f"import Tablet.{node_name}\n#print axioms {node_name}\n")
            temp_path = Path(handle.name)
        try:
            os.chmod(temp_path, 0o664)
        except OSError:
            pass
        lean_arg = (
            str(temp_path.relative_to(repo))
            if temp_path.is_relative_to(repo)
            else str(temp_path)
        )
        payload = _run_lake_command(
            repo,
            ["env", "lean", lean_arg],
            timeout_secs=timeout_secs,
            bwrap_role=bwrap_role,
        )
        payload["node"] = node_name
        return payload
    finally:
        if temp_path is not None:
            try:
                temp_path.unlink()
            except OSError:
                pass


def observe_lean_semantic_payloads(
    repo: Path,
    node_names: Sequence[str],
    *,
    timeout_secs: float = LEAN_SUPPORT_TIMEOUT_SECS,
    bwrap_role: Optional[str] = None,
    script_path: Optional[Path] = None,
) -> Dict[str, Dict[str, Any]]:
    """Observe Lean semantic payloads for a set of Tablet nodes.

    ``script_path`` lets the caller pin which copy of
    ``lean_semantic_fingerprint.lean`` lake invokes. When ``None`` (default,
    direct-call path), the source-root copy is used. The supervisor-side
    checker server passes an explicit path so the script resolves inside
    the ``lake_compiler`` bwrap (sandbox.py mounts the trellis source
    ``scripts/`` directory read-only for that role).
    """
    # Acceptance routing: socket-mandatory when ``bwrap_role is None``.
    # Route through the supervisor-side checker server (no host-lake
    # fallback). The wire response is ``{"request_id", "nodes": {...}}``;
    # the client already unwraps the ``nodes`` envelope so it returns the
    # per-node mapping the direct-lake path returns. ``script_path`` is
    # honoured by the supervisor side at its own copy of this function — the
    # worker passes node_names only. The direct-lake path below is reachable
    # only for explicit ``bwrap_role`` values. See ``compile_node`` for the
    # rationale.
    if bwrap_role is None:
        socket_path = _require_socket_path()
        response = client_lean_semantic_payloads(
            socket_path,
            repo,
            list(node_names),
            timeout_secs=timeout_secs,
        )
        # ``client_lean_semantic_payloads`` already unwraps ``nodes``
        # and returns the per-node mapping directly; the response has
        # no ``request_id`` to strip. Coerce to a plain dict-of-dicts
        # so the return type matches the direct-lake path exactly.
        return {
            node_name: dict(entry) for node_name, entry in response.items()
        }

    requested_nodes: list[str] = []
    seen: set[str] = set()
    for raw_name in node_names:
        node_name = str(raw_name).strip()
        if not node_name or node_name in seen:
            continue
        seen.add(node_name)
        requested_nodes.append(node_name)

    result: Dict[str, Dict[str, Any]] = {
        node_name: {"ok": False, "payload": "", "error": ""}
        for node_name in requested_nodes
    }
    if not requested_nodes:
        return result

    bare_mathlib = _find_bare_mathlib_imports(repo, requested_nodes)
    if bare_mathlib:
        message = _bare_mathlib_error_message(bare_mathlib)
        for node_name in requested_nodes:
            result[node_name]["ok"] = False
            result[node_name]["payload"] = ""
            result[node_name]["error"] = message
        return result

    resolved_script_path = (
        Path(script_path) if script_path is not None
        else _lean_semantic_fingerprint_script_path()
    )
    if not resolved_script_path.exists():
        message = f"semantic fingerprint script not found: {resolved_script_path}"
        for node_name in requested_nodes:
            result[node_name]["error"] = message
        return result

    # Run one node per Lean process so memory from large transitive closures can
    # be reclaimed between nodes instead of accumulating inside one long-lived
    # `lean --run` process.
    total = len(requested_nodes)
    payloads_started = time.time()
    _progress_emit(
        f"[acceptance]   lean-semantic-payloads: starting on {total} node(s)"
    )
    for idx, node_name in enumerate(requested_nodes, start=1):
        node_started = time.time()
        _progress_emit(
            f"[acceptance]   lean-semantic-payloads ({idx}/{total}) {node_name}"
        )
        raw = _run_lake_command(
            repo,
            ["env", "lean", "--run", str(resolved_script_path), node_name],
            timeout_secs=timeout_secs,
            bwrap_role=bwrap_role,
        )
        stdout = str(raw.get("stdout", "") or "")
        stderr = str(raw.get("stderr", "") or "")
        entry = result[node_name]

        for line in (stdout + "\n" + stderr).splitlines():
            if line.startswith("FP\t"):
                try:
                    _, parsed_node_name, payload = line.split("\t", 2)
                except ValueError:
                    continue
                if parsed_node_name == node_name:
                    entry["ok"] = True
                    entry["payload"] = payload
                    entry["error"] = ""
            elif line.startswith("ERR\t"):
                try:
                    _, parsed_node_name, error = line.split("\t", 2)
                except ValueError:
                    continue
                if parsed_node_name == node_name:
                    entry["ok"] = False
                    entry["payload"] = ""
                    entry["error"] = error

        generic_error = ""
        if raw.get("spawn_error"):
            generic_error = str(raw["spawn_error"])
        elif raw.get("timed_out"):
            generic_error = str(raw.get("stderr") or f"timed out after {timeout_secs}s")
        elif raw.get("returncode") not in (0, None):
            generic_error = (
                f"lean semantic payload extraction failed with exit code {raw['returncode']}"
            )

        if not entry["ok"] and not entry["error"]:
            entry["error"] = generic_error or "no semantic payload emitted"

        node_duration = time.time() - node_started
        if entry["ok"]:
            _progress_emit(
                f"[acceptance]   lean-semantic-payloads ({idx}/{total}) {node_name}: ok in {node_duration:.1f}s"
            )
        else:
            _progress_emit(
                f"[acceptance]   lean-semantic-payloads ({idx}/{total}) {node_name}: fail in {node_duration:.1f}s"
            )

    _progress_emit(
        f"[acceptance]   lean-semantic-payloads: done {total}/{total} in {time.time() - payloads_started:.1f}s"
    )
    return result


__all__ = [
    "build_tablet",
    "compile_node",
    "materialization_order",
    "materialize_tablet_oleans",
    "observe_lean_semantic_payloads",
    "print_axioms",
]
