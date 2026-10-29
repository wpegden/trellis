"""Filesystem sandbox helpers for agent bursts."""

from __future__ import annotations

import json
import os
import posixpath
import subprocess
import shutil
import shlex
import sys
import uuid
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Tuple

from trellis.config import SandboxConfig
from trellis.git_trust import inject_burst_git_safe_directories
from trellis.host_runtime import (
    host_runtime_readonly_roots,
    worker_isabelle_home,
    worker_isabelle_home_user,
    worker_path_env,
)
from trellis.worker_scratch import worker_scratch_dir, worker_scratch_notes_path


# Roles whose sandbox runs the actual agent's interactive proof loop and so
# needs the Isabelle toolchain on an isabelle_hol run. lake_compiler is
# supervisor-side and Lean-only (it never elaborates .thy), so it is excluded.
_ISABELLE_TOOLCHAIN_ROLES = ("worker", "reviewer", "verifier")
_MANAGED_CHECK_ROLES = {"worker", "reviewer", "verifier", "stuck_math_audit", "source_adaptation_worker", "source_adaptation_reviewer"}


def _tablet_target_for_repo(repo_path: Path) -> str:
    """Resolve the tablet backend target (`"lean"` / `"isabelle_hol"`).

    Reads `workflow.default_target` from `<repo>/trellis.config.json`,
    mirroring the kernel `worker_normalization::tablet_target_for_repo` and
    the bridge's `_tablet_target_for_repo`. Defaults to `"lean"` when the
    config is absent/unreadable or the key is missing, so a Lean repo (and any
    pre-field repo) selects the Lean backend and the sandbox binds/PATH stay
    byte-identical to the pre-isabelle behavior.
    """
    config_file = repo_path.resolve() / "trellis.config.json"
    try:
        raw = json.loads(config_file.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return "lean"
    workflow = raw.get("workflow") if isinstance(raw, dict) else None
    if isinstance(workflow, dict):
        value = workflow.get("default_target")
        if isinstance(value, str) and value.strip():
            return value.strip()
    return "lean"


def repo_targets_isabelle(repo_path: Path) -> bool:
    """True when the repo's `default_target` selects the Isabelle backend."""
    return _tablet_target_for_repo(repo_path) == "isabelle_hol"


_SYSTEM_READONLY_DIRS = (
    Path("/usr"),
    Path("/bin"),
    Path("/sbin"),
    Path("/lib"),
    Path("/lib64"),
    Path("/etc"),
    Path("/opt"),
)
_HOST_CONFIG_SYMLINKS = (
    Path("/etc/resolv.conf"),
)
_SANDBOX_TMPDIR = Path("/trellis-tmp")
_PASSTHROUGH_PATH_ENV_VARS = ("XDG_CACHE_HOME",)
# Scalar env vars that are propagated through the bwrap boundary when set on
# the supervisor.
#
# NOTE: bwrap does NOT strip the parent environment by default — an earlier
# version of this comment claimed it did, and that false claim is why the
# forwarding list reads as load-bearing. Verified: `FOO=leaked bwrap ... env`
# prints FOO. Only `--clearenv` strips. These entries therefore RE-ASSERT
# values that already arrive by inheritance; withholding a name here does
# not withhold it from the burst.
# - TRELLIS_LEAN_PARALLELISM: enables `observe_nodes_parallel` in the
#   kernel binary the worker spawns via check.py; without it the worker's
#   internal acceptance pipeline runs lake compiles serially.
# - TRELLIS_CHECKER_SOCKET: path to the supervisor-side unified-checker
#   UNIX socket (see trellis.checker.server). When set, the worker's
#   in-burst observation calls (compile_node and friends, via
#   trellis.atomic_actions.checker_client) route lake invocations
#   through the supervisor's confined-bwrap server instead of running
#   lake locally inside the worker burst. Without this passthrough the
#   worker burst's check.py would never see the env var and would
#   silently fall back to direct lake despite the operator opting in.
# - TRELLIS_REVIEWER_SOURCE_SNAPSHOT / TRELLIS_REVIEWER_SOURCE_SHA: the
#   reviewer source-recourse snapshot path + SHA, materialized once at
#   supervisor startup by scripts/trellis.sh. The reviewer bwrap mounts
#   the snapshot read-only and the prompt fragment substitutes both.
_PASSTHROUGH_VALUE_ENV_VARS = (
    "TRELLIS_LEAN_PARALLELISM",
    "TRELLIS_CHECKER_SOCKET",
    "TRELLIS_CHECK_JOBS_SOCKET",
    "TRELLIS_CHECK_JOBS_TOKEN",
    "TRELLIS_REVIEWER_SOURCE_SNAPSHOT",
    "TRELLIS_REVIEWER_SOURCE_SHA",
    # Phase 2 of the bwrap-only migration plan
    # (SANDBOX_BWRAP_ONLY_MIGRATION_PLAN_2026-06-03.md §3): per-burst
    # HMAC token minted by the bridge at dispatch time, forwarded into
    # the burst so the worker's `checker_client` can include it in
    # every request envelope. The runtime root that hosts
    # `<runtime>/checker-state/burst-tokens.json` is NOT bind-mounted
    # into the burst, so the env-var path is the only way the burst
    # learns its own token.
    "TRELLIS_CHECKER_TOKEN",
    "TRELLIS_PHASE0_CHECKER_SOCKET",
    "TRELLIS_PHASE0_CHECKER_TOKEN",
    # active_node_prewarm (off by default): the unix socket of the
    # supervisor-side warm active-node `lean --server`. When the operator
    # opts in, the launcher places this socket alongside the checker socket
    # under `<runtime_root>/sockets/` (already ro-bound into the worker bwrap
    # for the worker role) and exports its path here, so the worker's
    # `incremental-check` can prefer the already-warm active-node server for
    # its first call. Unset by default => the worker-side preference falls
    # straight through to the in-burst broker (inert).
    "INCREMENTAL_CHECK_ACTIVE_PREWARM_SOCK",
    # active_node_prewarm (off by default): the in-burst `--prewarm` hook in the
    # worker burst script fires only when both of these are set. The bridge sets
    # them (gated on `active_node_prewarm.enabled`) to the kernel-held active
    # node so the in-burst broker pays its cold start before the worker's first
    # edit. Unset by default => the hook is a no-op.
    "TRELLIS_INCREMENTAL_PREWARM",
    "TRELLIS_PREWARM_NODE",
    # active_node_prewarm giant-node skip thresholds: forwarded so the
    # standalone worker-side `incremental_check.py` (no trellis package imports)
    # classifies giant nodes identically to the supervisor server. Unset => the
    # worker copy uses its built-in defaults (3000 lines / 2e6 heartbeats).
    "INCREMENTAL_CHECK_GIANT_MAX_LINES",
    "INCREMENTAL_CHECK_GIANT_HEARTBEAT_CEILING",
)

# Complete environment admitted by the framework-owned Rust witness role.
# Its runner resolves and validates these paths before entering bwrap.
_RUST_WITNESS_ENV_VARS = (
    "PATH",
    "CARGO_HOME",
    "RUSTC",
    "RUSTDOC",
    "TMPDIR",
    "TMP",
    "TEMP",
)
_SEALED_CHECKER_ROLES = ("rust_witness_artifact", "source_adaptation_checker")

# Subdirectories of the operator's provider state dirs that hold session
# transcripts, job scratch and history rather than provider auth/config.
# Bound as empty tmpfs inside every burst sandbox (see the provider-state
# bind loop in `wrap_command`); kept in step with
# `trellis.burst_home._PROVIDER_HOME_SEED_PRUNE`.
_PROVIDER_STATE_PRIVATE_SUBDIRS: dict[str, tuple[str, ...]] = {
    ".claude": (
        "jobs", "projects", "file-history", "sessions", "tasks", "todos",
        "shell-snapshots", "debug", "daemon", "uploads", "backups",
        "session-env", "paste-cache",
    ),
    # `sessions`/`archived_sessions` are the operator's rollouts; a burst's own
    # rollouts live under its $HOME/.codex and are unaffected by this mask.
    ".codex": ("log", "db-backups", "tmp", ".tmp", "shell_snapshots", "sessions", "archived_sessions"),
    ".gemini": ("tmp",),
}
_SOURCE_ADAPTATION_CHECKER_ENV_VARS = _RUST_WITNESS_ENV_VARS + (
    "LANG",
    "LC_ALL",
    "RUSTUP_TOOLCHAIN",
    "OPAM_SWITCH_PREFIX",
    "OPAMSWITCH",
    "CAML_LD_LIBRARY_PATH",
    "OCAML_TOPLEVEL_PATH",
    "OCAMLTOP_INCLUDE_PATH",
    "CARGO_NET_OFFLINE",
    # charon-driver links librustc_driver from the pinned toolchain lib; the runner sets it.
    "LD_LIBRARY_PATH",
    "CHARON_TOOLCHAIN_IS_IN_PATH",
)


# Subdirectory under the runtime root where worker-context kernel CLI
# subprocesses write their disk-cache entries. Sibling to
# `<runtime>/checker-state/kernel-cache/` (the supervisor's writable
# cache, mounted read-only into worker bwraps so the worker can hit
# entries the supervisor wrote — see `wrap_command`).
_WORKER_KERNEL_CACHE_SUBDIR = "worker-cache"


def _kernel_cache_runtime_root() -> Optional[Path]:
    """Resolve the supervisor's runtime root from
    ``TRELLIS_KERNEL_CACHE_ROOT``.

    The supervisor sets this env var at startup; sandbox.py runs in the
    supervisor's process, so the var is visible here. Returns ``None``
    when unset (kernel disk cache is disabled — sandbox falls back to
    the legacy no-cache wiring).

    The supervisor's value is the runtime root (e.g.
    ``<math-root>/<run-name>-runtime``); the kernel binary
    appends the standard ``checker-state/kernel-cache`` subpath.
    """
    raw = os.environ.get("TRELLIS_KERNEL_CACHE_ROOT", "").strip()
    if not raw:
        return None
    try:
        resolved = Path(raw).resolve()
    except OSError:
        return None
    if not resolved.is_dir():
        return None
    return resolved


def _ensure_worker_kernel_cache_root(runtime_root: Path) -> Optional[Path]:
    """Create the worker-writable kernel cache base under the runtime
    root. Idempotent; returns the path on success, ``None`` on failure.

    Layout: ``<runtime>/worker-cache/`` (the kernel binary appends
    ``checker-state/kernel-cache/<namespace>/`` so the on-disk layout
    inside this base matches the supervisor's). The dir is mode ``0o770``,
    the supervisor's group: the burst user is a member
    of that group, so it can read and write inside via group perms.
    Files the worker writes are owned by the burst user;
    cross-uid sharing of the writable cache is intentionally not
    supported (the supervisor never reads from this directory — that's
    the trust-direction invariant of the two-cache split).

    On failure (e.g. mkdir or chmod denied), returns ``None`` and the
    caller skips wiring up the cache for this worker bwrap; observation
    calls inside the worker fall through to lake unchanged.
    """
    cache_base = runtime_root / _WORKER_KERNEL_CACHE_SUBDIR
    try:
        cache_base.mkdir(parents=True, exist_ok=True)
        # Tighten perms in case parent umask widened them on first
        # creation. Idempotent on re-runs.
        os.chmod(cache_base, 0o770)
    except OSError:
        return None
    return cache_base


def _trellis_source_scripts_dir() -> Path:
    """Resolve the trellis source ``scripts/`` directory.

    The ``lake_compiler`` bwrap mounts this directory read-only so
    ``observe_lean_semantic_payloads`` can spawn ``lean --run
    scripts/lean_semantic_fingerprint.lean``. The supervisor repo
    (``<worker>/.trellis/supervisor/repo``) does NOT contain a
    ``scripts/`` tree of its own — it's a checkout of the user's repo —
    so the source tree's scripts have to come from outside.
    """
    return Path(__file__).resolve().parent.parent / "scripts"


def bwrap_available() -> bool:
    return shutil.which("bwrap") is not None


def _ancestor_dirs(paths: Iterable[Path]) -> List[Path]:
    ordered: list[Path] = []
    seen: set[Path] = set()
    for path in paths:
        current = path.resolve()
        parents: list[Path] = []
        while True:
            current = current.parent
            if current == Path("/") or str(current) == ".":
                break
            parents.append(current)
        for parent in reversed(parents):
            if parent not in seen:
                ordered.append(parent)
                seen.add(parent)
    return ordered


def _host_extra_readonly_paths() -> List[Path]:
    """Return extra host paths that must be mounted because config symlinks escape /etc."""
    extra: list[Path] = []
    seen: set[Path] = set()
    for path in _HOST_CONFIG_SYMLINKS:
        try:
            resolved = path.resolve(strict=True)
        except FileNotFoundError:
            continue
        if resolved.is_relative_to(Path("/etc")):
            continue
        if resolved not in seen:
            extra.append(resolved)
            seen.add(resolved)
    return extra


def _env_extra_readonly_paths(environment: Optional[dict[str, str]] = None) -> List[Path]:
    """Extra host paths to bind read-only, from ``TRELLIS_SANDBOX_EXTRA_RO_PATHS``
    (os.pathsep-separated). Used by PV tablets whose lakefile path-``require``s an
    external support tree (the Aeneas/Lean backend lib + its built oleans) that
    lives outside the repo, so it is otherwise invisible inside the bwrap bind set."""
    raw = (os.environ if environment is None else environment).get("TRELLIS_SANDBOX_EXTRA_RO_PATHS", "").strip()
    if not raw:
        return []
    out: list[Path] = []
    seen: set[Path] = set()
    for piece in raw.split(os.pathsep):
        piece = piece.strip()
        if not piece:
            continue
        try:
            resolved = Path(piece).expanduser().resolve(strict=True)
        except FileNotFoundError:
            continue
        if resolved not in seen:
            out.append(resolved)
            seen.add(resolved)
    return out


def _passthrough_path_envs() -> dict[str, Path]:
    envs: dict[str, Path] = {}
    for name in _PASSTHROUGH_PATH_ENV_VARS:
        raw = os.environ.get(name)
        if not raw:
            continue
        path = Path(raw).expanduser().resolve()
        path.mkdir(parents=True, exist_ok=True)
        envs[name] = path
    return envs


def _passthrough_value_envs() -> dict[str, str]:
    """Return plain-string env vars to forward into the sandbox unchanged."""
    envs: dict[str, str] = {}
    for name in _PASSTHROUGH_VALUE_ENV_VARS:
        raw = os.environ.get(name)
        if raw is None or not str(raw).strip():
            continue
        # Resolve symlinks on absolute-path values so the path stays valid
        # inside bwrap sandboxes — bind mounts use realpath (e.g. when the
        # math root is a symlink onto a different physical disk, the bwrap
        # binds the realpath of the run directory, not the symlink path).
        if raw.startswith("/"):
            try:
                resolved = os.path.realpath(raw)
                if os.path.exists(resolved):
                    raw = resolved
            except OSError:
                pass
        envs[name] = raw
    # Worker-burst-specific override: TRELLIS_BURST_LEAN_PARALLELISM lets
    # the supervisor scale worker-side parallelism independently of its own
    # in-process kernel batch (which is fixed at startup via
    # TRELLIS_LEAN_PARALLELISM and can't be raised without a restart). When
    # set, it overrides what the worker sees for TRELLIS_LEAN_PARALLELISM.
    burst_par = os.environ.get("TRELLIS_BURST_LEAN_PARALLELISM", "").strip()
    if burst_par:
        envs["TRELLIS_LEAN_PARALLELISM"] = burst_par
    return envs


def _reviewer_source_snapshot() -> Optional[Path]:
    """Return the reviewer source-recourse snapshot dir if materialized.

    The snapshot is created once per run by `scripts/trellis.sh` (before
    the runtime CLI is invoked) at a specific git SHA — defaulting to HEAD
    of the trellis source tree, overridable via
    `TRELLIS_REVIEWER_SOURCE_SHA`. The reviewer's bwrap mounts this
    read-only so the reviewer can consult kernel + Python source as a
    fallback when process semantics seem to block forward progress. If the
    env var is unset (snapshot not materialized this run), the reviewer
    simply runs without source access.
    """
    raw = os.environ.get("TRELLIS_REVIEWER_SOURCE_SNAPSHOT")
    if not raw or not str(raw).strip():
        return None
    return Path(raw)


def _checker_socket_dir() -> Optional[Path]:
    """Locate the configured checker socket for read-only agent sandbox access.

    Binding its parent directory lets clients connect across socket replacement.
    Return None for an unset path, filesystem root, or missing parent directory.
    """
    raw = os.environ.get("TRELLIS_CHECKER_SOCKET", "")
    if not raw or not raw.strip():
        return None
    socket_path = Path(raw.strip())
    if socket_path.is_absolute():
        try:
            socket_path = Path(os.path.realpath(str(socket_path)))
        except OSError:
            pass
    parent = socket_path.parent
    # A bare filename ("checker.sock") yields parent == Path('.'); a path
    # like "/checker.sock" yields parent == Path('/'). Neither is something
    # we can safely bind into the sandbox, and neither matches the
    # documented runtime layout (<runtime_root>/sockets/checker.sock).
    if str(parent) in ("", ".") or parent == Path("/"):
        return None
    if not parent.exists():
        return None
    if parent == Path("/tmp"):
        # Mount only the configured socket, never the host temporary partition.
        return socket_path if socket_path.exists() else None
    return parent.resolve()


def _git_preflight_interpreter(readonly_roots: Iterable[Path]) -> Path:
    """Use the running supervisor's interpreter only if its runtime is mounted."""
    try:
        interpreter = Path(sys.executable).resolve(strict=True)
    except OSError as exc:
        raise RuntimeError(f"Git preflight bootstrap interpreter is missing: {sys.executable}") from exc
    roots = [p.resolve() for p in readonly_roots if p.is_dir()]
    if (not interpreter.is_file() or not os.access(interpreter, os.X_OK)
            or not any(interpreter.is_relative_to(root) for root in roots)
            or not any(Path(sys.base_prefix).resolve().is_relative_to(root) for root in roots)):
        raise RuntimeError(f"Git preflight interpreter/runtime is not executable and read-only mounted: {interpreter}")
    return interpreter


def _check_engineering_home_mounts(setup: List[str], home: Path) -> None:
    """Check the final namespace mounts, using its aliases rather than host paths."""
    aliases: dict[Path, Path] = {}
    mounts: dict[Path, bool] = {}

    def namespace_path(raw) -> Path:
        path = Path(posixpath.normpath(str(raw)))
        if not path.is_absolute():
            raise ValueError("Engineering namespace paths must be absolute")
        for _ in range(40):
            prefix = next((p for p in (path, *path.parents) if p in aliases), None)
            if prefix is None:
                return path
            path = Path(posixpath.normpath(str(aliases[prefix] / path.relative_to(prefix))))
        raise ValueError("Engineering namespace alias cycle")

    for index, option in enumerate(setup):
        if option == "--symlink":
            destination = namespace_path(setup[index + 2])
            target = Path(setup[index + 1])
            aliases[destination] = target if target.is_absolute() else destination.parent / target
        elif option in {"--bind", "--dev-bind", "--ro-bind", "--tmpfs", "--proc", "--dev"}:
            destination = namespace_path(setup[index + (2 if option.endswith("bind") else 1)])
            # A later parent mount hides earlier mounts beneath that destination.
            mounts = {path: writable for path, writable in mounts.items()
                      if not path.is_relative_to(destination)}
            mounts[destination] = option != "--ro-bind"
        elif option == "--remount-ro":
            mounts[namespace_path(setup[index + 1])] = False
    namespace_home = namespace_path(home)
    if any(writable and namespace_home != path and namespace_home.is_relative_to(path)
           for path, writable in mounts.items()):
        raise ValueError("Engineering HOME is beneath a writable namespace mount")


def wrap_command(
    inner_cmd: List[str],
    *,
    sandbox: Optional[SandboxConfig],
    work_dir: Path,
    burst_home: Optional[Path] = None,
    role: str = "worker",
    git_preflight_path: Optional[str] = None,
    environment: Optional[dict[str, str]] = None,
    engineering_request: Optional[dict] = None,
    engineering_files: tuple = (),
) -> List[str]:
    """Wrap a command in bubblewrap if sandboxing is enabled."""
    engineering = role == "engineering"
    system_readonly = [path for path in _SYSTEM_READONLY_DIRS if not engineering or path != Path("/opt")]
    if engineering:
        from trellis.agents import engineering_context
        if burst_home is None or sandbox is None or not sandbox.enabled:
            raise ValueError("Engineering requires an isolated home and bwrap")
        engineering_context.validate(burst_home, request=engineering_request)
        if engineering_request is None:
            raise ValueError("Engineering namespace requires an admitted request")
    if sandbox is None or not sandbox.enabled:
        return inner_cmd
    if sandbox.backend != "bwrap":
        raise ValueError(f"Unsupported sandbox backend: {sandbox.backend}")
    if not bwrap_available():
        raise RuntimeError("bwrap is required for sandboxed bursts but is not installed")

    if (role in {"source_adaptation_worker", "source_adaptation_reviewer"}
            or role in _SEALED_CHECKER_ROLES) and burst_home is None:
        # An untrusted role must never fall back to `Path.home()`: that
        # binds the operator's real home READ-WRITE, shadowing the ro-bind
        # of ~/.codex and exposing ~/math, ~/src and ~/.ssh.
        raise ValueError(f"role {role!r} requires an explicit burst_home")
    repo = work_dir.resolve()
    home = (burst_home or Path.home()).resolve()
    dependency_cache = role in {"lake_dependency_cache", "lake_dependency_cache_probe"}
    if dependency_cache:
        # This role is reachable only for the host's fixed cache command, not
        # an arbitrary Lake/Lean program with writable package artifacts.
        from trellis.supervisor_workspace import dependency_cache_invocation
        if burst_home is None:
            raise ValueError("dependency cache requires a host-only home")
        executable, expected_env = dependency_cache_invocation(repo, home.parent)
        if inner_cmd != [str(executable), "get"] or environment != expected_env:
            raise ValueError("dependency cache requires the fixed host-prepared invocation")
        # The host cache lock is already held before compiler admission.
        # Keep only the manifest/toolchain in the cache program's cwd; neither
        # candidate Tablet nor a project lakefile can become startup code.
        cache_view = home.parent / "view"
        (cache_view / ".lake").mkdir(parents=True, exist_ok=True)
        for name in ("lake-manifest.json", "lean-toolchain"):
            shutil.copyfile(repo / name, cache_view / name)
        packages_alias = cache_view / ".lake/packages"
        packages_alias.unlink(missing_ok=True)
        packages_alias.symlink_to(repo / ".lake/packages", target_is_directory=True)
    # Isabelle backend (GAP 1): on an isabelle_hol run the worker/reviewer/
    # verifier need the Isabelle distribution + warm session heaps inside the
    # sandbox so the agent's interactive Draft→Sketch→Prove loop can actually
    # run `isabelle`. Gated on the repo's `default_target`; a Lean run yields
    # False here, so every isabelle-specific bind/setenv/PATH addition below is
    # skipped and the Lean bwrap argv stays byte-identical.
    is_isabelle = (
        role in _ISABELLE_TOOLCHAIN_ROLES and repo_targets_isabelle(repo)
    )
    isabelle_home = worker_isabelle_home() if is_isabelle else None
    isabelle_home_user = worker_isabelle_home_user() if is_isabelle else None
    temporary = repo / ".trellis/tmp" / uuid.uuid4().hex
    temporary.mkdir(parents=True, exist_ok=True)
    source_env = {}
    if not engineering and role not in {"lake_compiler", "lake_dependency_cache", "lake_dependency_cache_probe"} and role not in _SEALED_CHECKER_ROLES:
        from trellis.modeling.environment import source_burst_environment
        source_env = source_burst_environment(repo, repo / ".trellis/scratch/source-bursts" / temporary.name)
    extra_readonly = _host_extra_readonly_paths() + ([] if engineering else _env_extra_readonly_paths(environment))
    from trellis.pv_toolchain import pinned_library_mounts
    library_mounts = {} if engineering else pinned_library_mounts(repo, environment)
    extra_readonly.extend(library_mounts.values())
    # The lake_compiler role only needs the lean toolchain (elan); it never
    # execs a provider CLI, so their binaries are not bound.
    if role in {"lake_compiler", "lake_dependency_cache", "lake_dependency_cache_probe"} or role in _SEALED_CHECKER_ROLES:
        host_runtime_tools: tuple[str, ...] = ()
    else:
        # "agy" is the Antigravity CLI binary (~/.local/bin/agy); its bin dir
        # is bound read-only so the antigravity provider can exec it inside
        # bwrap. Its auth/conversation state lives under ~/.gemini, already
        # bound below.
        host_runtime_tools = ("codex", "claude", "gemini", "agy")
    data_view = os.environ.get("TRELLIS_COMPILER_DATA_VIEW", "")
    native_bwrap = os.environ.get("TRELLIS_COMPILER_BWRAP", "")
    compiler_confined = bool(data_view or native_bwrap) and role in _MANAGED_CHECK_ROLES
    managed_agent = role in _MANAGED_CHECK_ROLES and (compiler_confined or os.environ.get("TRELLIS_MANAGED_AGENT_CHECKS") == "1")
    compiler_capability = os.environ.get("TRELLIS_AGENT_COMPILER_CAPABILITY", "managed")
    no_compiler = managed_agent and compiler_capability == "none"
    compiler_envs = {"TRELLIS_CHECKER_SOCKET", "TRELLIS_CHECKER_TOKEN", "TRELLIS_CHECK_JOBS_SOCKET",
                     "TRELLIS_CHECK_JOBS_TOKEN", "TRELLIS_PHASE0_CHECKER_SOCKET", "TRELLIS_PHASE0_CHECKER_TOKEN",
                     "INCREMENTAL_CHECK_ACTIVE_PREWARM_SOCK", "TRELLIS_INCREMENTAL_PREWARM", "TRELLIS_PREWARM_NODE"}
    host_runtime_dirs = [] if engineering else host_runtime_readonly_roots(
        burst_home=None if managed_agent else burst_home,
        include_tools=host_runtime_tools,
    )
    if engineering:
        _, host_runtime_dirs = engineering_context.runtime(environment)
    # The lake_compiler role needs the trellis source `scripts/` dir
    # mounted read-only so `observe_lean_semantic_payloads` can spawn
    # `lean --run scripts/lean_semantic_fingerprint.lean`. The supervisor
    # repo does not contain `scripts/`, and the script path resolved by
    # `_lean_semantic_fingerprint_script_path()` lives outside the
    # supervisor workspace tree.
    #
    # The stuck_math_audit role also needs `scripts/` so the audit prompt
    # can invoke `scripts/cone_clean_impact.py` to estimate cone-clean
    # impact before recommending one (see
    # prompt_fragments/stuck_math_audit/common/04b_cone_clean.md).
    extra_role_readonly: list[Path] = []
    if dependency_cache:
        extra_role_readonly.append(cache_view)
    if role in ("lake_compiler", "stuck_math_audit"):
        scripts_dir = _trellis_source_scripts_dir()
        if scripts_dir.exists():
            extra_role_readonly.append(scripts_dir.resolve())
    elif role == "reviewer":
        # The reviewer alone gets a snapshot of the trellis source tree
        # as a "source recourse" — see
        # trellis/prompt_fragments/review/common/05_source_recourse.md.
        # The snapshot dir is materialized at supervisor startup
        # (scripts/trellis.sh) at a specific git SHA so the reviewer
        # reads what was true at that SHA, not whatever the live tree
        # happens to contain right now. Defaults to HEAD; operator can
        # pin via TRELLIS_REVIEWER_SOURCE_SHA.
        snapshot = _reviewer_source_snapshot()
        if snapshot is not None and snapshot.exists():
            extra_role_readonly.append(snapshot.resolve())
    elif role in {"worker", "source_adaptation_worker"} and not no_compiler:
        # When ``TRELLIS_CHECKER_SOCKET`` is set (``--with-checker-rpc``
        # mode), the worker burst's check.py routes lake invocations
        # through the supervisor-side unified-checker UNIX socket. The
        # env var is forwarded via ``_passthrough_value_envs``, but the
        # socket *file* lives at ``<runtime_root>/sockets/checker.sock``
        # which is outside the repo and outside ``$HOME``. Without an
        # explicit bind, the path resolves to nothing inside bwrap and
        # ``connect()`` raises FileNotFoundError, surfacing as a
        # ``supervisor_unavailable`` RPC error from the worker. Bind the
        # parent directory read-only — the worker only needs to connect
        # to the socket, never to create it (the supervisor owns the
        # socket node). The lake_compiler role is supervisor-side and
        # services the RPC directly without re-entering the socket
        # client (see ``observations.py`` ``bwrap_role`` recursion
        # guard), so it does not need this bind.
        socket_dir = _checker_socket_dir()
        if socket_dir is not None:
            extra_role_readonly.append(socket_dir)
        phase0_socket = os.environ.get("TRELLIS_PHASE0_CHECKER_SOCKET", "").strip()
        if phase0_socket:
            extra_role_readonly.append(Path(phase0_socket).resolve().parent)
    # Isabelle backend (GAP 1): bind the distribution + the warm heaps RO.
    # Whole-tree RO bind of `<ISABELLE_HOME>` (signed off) puts `isabelle` on
    # PATH (its bin/ is prepended via `worker_path_env(isabelle=True)`) and
    # exposes the component tree + the prebuilt logic sources. The per-user
    # heap dir is bound RO so an in-burst `isabelle build` finds the prebuilt
    # `HOL` image yet physically cannot rebuild/clobber it (read-only mount).
    if is_isabelle:
        if isabelle_home is not None and isabelle_home.exists():
            extra_role_readonly.append(isabelle_home)
        if isabelle_home_user is not None and isabelle_home_user.exists():
            # Defense-in-depth: the heap RO bind only protects the prebuilt heap
            # if the heap dir is OUTSIDE the writable HOME bind. If a future
            # caller passed `$HOME` (or any ancestor of the heap dir) as
            # `burst_home`, the writable `--bind home home` would shadow this
            # `--ro-bind`, silently granting an in-burst `isabelle build` a
            # WRITABLE heap it could clobber/rebuild. Refuse loudly rather than
            # bind-then-hope. The production burst home is a dedicated
            # `<runtime>/burst-homes/<role>/` sibling, never `$HOME`, so the
            # normal path is unaffected.
            if isabelle_home_user.is_relative_to(home):
                raise RuntimeError(
                    "Isabelle heap dir "
                    f"{isabelle_home_user} is inside the sandbox HOME {home}; "
                    "the read-only heap bind would be shadowed by the writable "
                    "HOME bind, giving an in-burst `isabelle build` a writable "
                    "heap. Use a dedicated burst home outside $HOME."
                )
            extra_role_readonly.append(isabelle_home_user)
    if dependency_cache:
        if sandbox.extra_writable_paths:
            raise ValueError("dependency cache does not accept extra writable paths")
        # Only the pinned manifest roots can receive cache outputs, including
        # git dependencies with subDir layouts. A warm probe has no such writes.
        repo_writable = ([Path(root) / ".lake/build" for root in environment["LEAN_SRC_PATH"].split(os.pathsep)]
                         if role == "lake_dependency_cache" else [])
        for path in repo_writable:
            path.mkdir(parents=True, exist_ok=True)
    else:
        repo_writable = _repo_writable_paths(
            repo, role=role, is_isabelle=is_isabelle,
            extra_writable_paths=sandbox.extra_writable_paths,
        )
    # Replay provenance lives beside Tablet oleans so it shares their purge /
    # copy lifecycle, but `.lake/build` must otherwise stay writable to Lean.
    # The trusted host pre-creates every `*.olean.srcclosure` path before a
    # build. Overlay those individual files read-only *after* the writable
    # build-dir bind below, preventing elaborator code from manufacturing the
    # attestation that would let its own unchecked olean skip leanchecker.
    repo_protected_readonly: list[Path] = []
    if role == "lake_compiler":
        provenance_dir = repo / ".lake" / "build" / "lib" / "lean" / "Tablet"
        try:
            repo_protected_readonly = sorted(
                path.resolve()
                for path in provenance_dir.glob("*.olean.srcclosure")
                if path.is_file()
            )
        except OSError:
            repo_protected_readonly = []
    if role in _MANAGED_CHECK_ROLES and not no_compiler:
        jobs_socket = os.environ.get("TRELLIS_CHECK_JOBS_SOCKET", "").strip()
        if jobs_socket:
            extra_role_readonly.append(Path(jobs_socket).parent.resolve())
    passthrough_envs = ({"XDG_CACHE_HOME": Path(environment["XDG_CACHE_HOME"])}
                        if dependency_cache else _passthrough_path_envs())
    passthrough_values = {} if dependency_cache else _passthrough_value_envs()
    if engineering:
        passthrough_envs = {}
        passthrough_values = {}
    if no_compiler:
        for name in compiler_envs:
            passthrough_envs.pop(name, None)
            passthrough_values.pop(name, None)
    if role in _SEALED_CHECKER_ROLES:
        # This role gets its complete read-only set from the runner's closed
        # request.  Do not inherit worker cache mounts or scalar controls.
        passthrough_envs = {}
        passthrough_values = {}

    if role not in _MANAGED_CHECK_ROLES:
        for name in ("TRELLIS_CHECK_JOBS_SOCKET", "TRELLIS_CHECK_JOBS_TOKEN"):
            passthrough_values.pop(name, None)

    # Kernel disk-cache wiring (worker role only). Two binds:
    #   - read-only bind of the supervisor's writable cache, exposed
    #     under its real path so the kernel binary's
    #     `cache_readonly_dir_for_namespace` can read entries the
    #     supervisor wrote.
    #   - read+write bind of the worker's own cache base.
    # Plus two `--setenv`s downstream:
    #   - `TRELLIS_KERNEL_CACHE_ROOT` → worker's writable base
    #   - `TRELLIS_KERNEL_CACHE_READONLY_ROOT` → supervisor's runtime
    #     root (kernel binary appends the standard subpath internally).
    # The trust direction: workers can read supervisor entries (saves
    # work) but never write to them (so a poisoned worker can't
    # influence what the supervisor's own lookups see — the supervisor
    # never sets the readonly env var, so its cache reads stay confined
    # to its own writable cache).
    worker_kernel_cache_writable: Optional[Path] = None
    worker_kernel_cache_readonly_bind: Optional[Path] = None
    worker_kernel_cache_readonly_root: Optional[Path] = None
    if role == "worker":
        runtime_root = _kernel_cache_runtime_root()
        if runtime_root is not None:
            worker_kernel_cache_writable = _ensure_worker_kernel_cache_root(runtime_root)
            super_cache_dir = runtime_root / "checker-state" / "kernel-cache"
            if super_cache_dir.is_dir():
                worker_kernel_cache_readonly_bind = super_cache_dir.resolve()
                worker_kernel_cache_readonly_root = runtime_root

    bind_targets = [
        repo,
        home,
        *repo_writable,
        *repo_protected_readonly,
        *passthrough_envs.values(),
        *[p for p in system_readonly if p.exists()],
        *host_runtime_dirs,
        *extra_readonly,
        *extra_role_readonly,
        *(
            [worker_kernel_cache_writable]
            if worker_kernel_cache_writable is not None
            else []
        ),
        *(
            [worker_kernel_cache_readonly_bind]
            if worker_kernel_cache_readonly_bind is not None
            else []
        ),
    ]
    cmd: List[str] = [
        "bwrap",
        "--die-with-parent",
        "--proc", "/proc",
        "--dev-bind", "/dev", "/dev",
        # Bwrap-only sandbox hardening (Phase 1 of the bwrap-only migration,
        # SANDBOX_BWRAP_ONLY_MIGRATION_PLAN_2026-06-03.md §3). Namespacing
        # hides host PIDs / SysV IPC / UTS from inside the sandbox so a
        # compromised burst cannot enumerate sibling bursts via
        # `/proc/<pid>/environ` (Claim B) or stomp on host hostname/IPC
        # resources. `--cap-drop ALL` is the explicit form of bwrap's
        # behavioral default and removes any capability the parent shell
        # may have left in the effective set.
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--cap-drop", "ALL",
    ]
    if role in _SEALED_CHECKER_ROLES:
        cmd.extend(["--unshare-net", "--clearenv"])
    else:
        if dependency_cache or engineering:
            cmd.extend(["--clearenv"])
        cmd.extend(["--bind", str(temporary), str(_SANDBOX_TMPDIR)])
        cmd.extend(["--symlink", str(_SANDBOX_TMPDIR), "/tmp"])
    for parent in _ancestor_dirs(bind_targets):
        if parent == Path("/tmp"):
            continue
        cmd.extend(["--dir", str(parent)])

    for path in system_readonly:
        if path.exists():
            cmd.extend(["--ro-bind", str(path), str(path)])
    for path in host_runtime_dirs:
        cmd.extend(["--ro-bind", str(path), str(path)])
    for path in extra_readonly:
        if path == Path("/tmp"):
            raise ValueError("The host /tmp partition cannot be mounted into an agent sandbox")
        cmd.extend(["--ro-bind", str(path), str(path)])
    for path in extra_role_readonly:
        cmd.extend(["--ro-bind", str(path), str(path)])
    # The per-burst fake-home hard-links ~/.codex/, ~/.claude/, ~/.gemini/
    # from the supervisor's home so OAuth/auth state round-trips. But codex
    # CLI's session DB stores rollout paths as absolute (e.g.
    # /path/to/trellis/.codex/sessions/...). Inside the sandbox, those
    # absolute paths must resolve so resume can find prior rollouts.
    # Ro-bind the supervisor's provider state dirs at their absolute paths.
    #
    # Role-scoped. This loop used to bind all three unconditionally, which
    # silently contradicted the `lake_compiler` comment above claiming that
    # role "cannot read provider auth tokens": it could, because this bind
    # ran for every role regardless of `host_runtime_tools`. A role gets a
    # provider's state dir only when it authenticates as that provider.
    # Every trusted role keeps the original unconditional three; only the
    # untrusted roles are narrowed.
    provider_state_dirs: tuple[Path, ...]
    if role in _SEALED_CHECKER_ROLES or dependency_cache or engineering:
        provider_state_dirs = ()
    else:
        provider_state_dirs = (
            Path.home() / ".codex",
            Path.home() / ".claude",
            Path.home() / ".gemini",
        )
    for sup_dir in provider_state_dirs:
        if sup_dir.exists() and sup_dir.resolve() != home.resolve() and sup_dir != home:
            cmd.extend(["--ro-bind", str(sup_dir), str(sup_dir)])
            # The absolute-path bind exists so codex rollout paths resolve;
            # it must not also expose the operator's own Claude Code session
            # transcripts, job scratch and file history to the burst. Mask
            # those subtrees with empty tmpfs mounts (bwrap applies them
            # after the ro-bind, so the mask wins).
            for private_name in _PROVIDER_STATE_PRIVATE_SUBDIRS.get(sup_dir.name, ()):
                private_dir = sup_dir / private_name
                if private_dir.is_dir():
                    cmd.extend(["--tmpfs", str(private_dir)])

    # bwrap applies binds in argv order and a later mount shadows an earlier
    # one at the same path. The usual layout (a burst home outside the repo)
    # binds the home first so the repo's read-only bind can sit inside a
    # legacy `Path.home()`; Phase 0 puts the burst home *inside* its
    # read-only attempt root, where that order hides the writable home and
    # codex dies at startup with EROFS. Bind whichever is nested second.
    home_inside_repo = home != repo and _path_is_within(home, repo)
    if not home_inside_repo:
        cmd.extend(["--bind", str(home), str(home)])
    cmd.extend(["--ro-bind", str(repo), str(repo)])
    if home_inside_repo:
        cmd.extend(["--bind", str(home), str(home)])
    for path in repo_writable:
        if path.exists():
            cmd.extend(["--bind", str(path), str(path)])
    for path in repo_protected_readonly:
        if path.exists():
            cmd.extend(["--ro-bind", str(path), str(path)])
    for path in passthrough_envs.values():
        if path.exists():
            cmd.extend(["--bind", str(path), str(path)])
    if worker_kernel_cache_writable is not None:
        cmd.extend([
            "--bind",
            str(worker_kernel_cache_writable),
            str(worker_kernel_cache_writable),
        ])
    if worker_kernel_cache_readonly_bind is not None:
        cmd.extend([
            "--ro-bind",
            str(worker_kernel_cache_readonly_bind),
            str(worker_kernel_cache_readonly_bind),
        ])
    if role in {"lake_compiler", "lake_dependency_cache_probe"}:
        # Dependency source, compiled libraries and Lake configuration are
        # host-prewarmed inputs. They are hardlinked into checker workspaces;
        # elaborating a candidate runs arbitrary code as the checker UID, so
        # writable dependency outputs would also mutate the authoritative
        # imports in sibling workspaces. Cover the whole package tree, including
        # subDir builds and package-internal aliases, after every writable bind.
        # Setup/prewarm runs separately, before candidate code is admitted.
        packages = repo / ".lake" / "packages"
        if packages.is_dir():
            cmd.extend(["--ro-bind", str(packages), str(packages)])
    # Bind after writable package build overlays and the package read-only
    # overlay so selected immutable libraries still replace their local aliases.
    for destination, source in library_mounts.items():
        cmd.extend(["--ro-bind", str(source), str(destination)])
    if engineering:
        codex_home = home / ".codex"
        rules = codex_home / "rules"
        # Pin the writable parent before protecting its canonical rules path.
        cmd.extend(["--bind", str(codex_home), str(codex_home),
                    "--ro-bind", str(rules), str(rules)])
    if dependency_cache:
        # The installer may replace dependency outputs, never its own trusted
        # executable. The rest of the package (source/config) is already RO.
        cmd.extend(["--ro-bind", inner_cmd[0], inner_cmd[0]])
        for name, value in environment.items():
            if name not in {"HOME", "XDG_CACHE_HOME"}:
                cmd.extend(["--setenv", name, value])
    cmd.extend(["--setenv", "HOME", str(home)])
    # Interior role marker. The deterministic acceptance check
    # (`trellis/checking.py` `_node_main` / `_tablet_main`, reached as
    # `.trellis/scripts/check.py node` and `check_tablet.sh`) is the
    # supervisor's to run; it reads this marker to refuse, in one sentence at
    # the entry point, the roles whose bwrap withholds the writable `Tablet/`
    # and checker-socket bind that the check needs.
    cmd.extend(["--setenv", "TRELLIS_SANDBOX_ROLE", role])
    if role in _MANAGED_CHECK_ROLES and (managed_agent or os.environ.get("TRELLIS_CHECK_JOBS_SOCKET")):
        shims = repo / ".trellis/scripts/check-bin"
        if not all((shims / name).is_file() for name in ("lean", "lake")):
            raise RuntimeError("managed checker clients are not provisioned in this workspace")
        from trellis.host_runtime import worker_elan_home, worker_path_env
        elan_bin = worker_elan_home() / "bin"
        cmd.extend(["--setenv", "PATH", str(shims) + os.pathsep + worker_path_env(home)])
        for name in ("lean", "lake"):
            cmd.extend(["--setenv", "TRELLIS_CHECK_JOBS_REAL_" + name.upper(), str(elan_bin / name)])
    if role == "source_adaptation_worker":
        # A local `cargo check` inside the candidate would drop `target/`
        # there, and the kernel refuses every unlogged byte in the candidate
        # tree. Keep cargo's build output in the writable burst home instead.
        cmd.extend(["--setenv", "CARGO_TARGET_DIR", str(home / ".trellis-cargo-target")])
    for name, value in source_env.items():
        cmd.extend(["--setenv", name, value])
    for name, path in passthrough_envs.items():
        cmd.extend(["--setenv", name, str(path)])
    for name, value in passthrough_values.items():
        cmd.extend(["--setenv", name, value])
    if role not in _MANAGED_CHECK_ROLES:
        for name in ("TRELLIS_CHECK_JOBS_SOCKET", "TRELLIS_CHECK_JOBS_TOKEN"):
            cmd.extend(["--unsetenv", name])
    if role in _SEALED_CHECKER_ROLES:
        admitted_env = (
            _SOURCE_ADAPTATION_CHECKER_ENV_VARS
            if role == "source_adaptation_checker"
            else _RUST_WITNESS_ENV_VARS
        )
        for name in admitted_env:
            value = os.environ.get(name, "").strip()
            if value:
                cmd.extend(["--setenv", name, value])
    else:
        cmd.extend(["--setenv", "TMPDIR", str(_SANDBOX_TMPDIR)])
        cmd.extend(["--setenv", "TMP", str(_SANDBOX_TMPDIR)])
        cmd.extend(["--setenv", "TEMP", str(_SANDBOX_TMPDIR)])
    # Isabelle backend (GAP 1): point the in-burst Isabelle at the RO-bound
    # per-user heap dir and expose the absolute `isabelle` launcher path so the
    # worker skill can invoke it without depending on PATH resolution. Both are
    # emitted only on isabelle runs (`is_isabelle` is False for every Lean
    # burst), keeping the Lean argv byte-identical.
    #
    # NOTE (merge): the base's direct `_passthrough_value_envs()` loop that
    # used to follow was deleted on master (5b682220) — it was the duplicate
    # --setenv that defeated the role-scoped env sanitization; the values now
    # arrive once via the `passthrough_values` parameter. Do not restore it.
    if is_isabelle and isabelle_home_user is not None:
        cmd.extend(["--setenv", "ISABELLE_HOME_USER", str(isabelle_home_user)])
    if is_isabelle and isabelle_home is not None:
        cmd.extend([
            "--setenv",
            "TRELLIS_ISABELLE_BIN",
            str(isabelle_home / "bin" / "isabelle"),
        ])
    if worker_kernel_cache_writable is not None:
        cmd.extend([
            "--setenv",
            "TRELLIS_KERNEL_CACHE_ROOT",
            str(worker_kernel_cache_writable),
        ])
    if worker_kernel_cache_readonly_root is not None:
        cmd.extend([
            "--setenv",
            "TRELLIS_KERNEL_CACHE_READONLY_ROOT",
            str(worker_kernel_cache_readonly_root),
        ])
    if role in {"worker", "reviewer", "verifier", "stuck_math_audit"} and not repo_targets_isabelle(repo):
        # Ordinary bursts inherit env, but an old tmux server may carry different
        # Git pairs. Reassert the complete composed set explicitly at bwrap.
        # Sealed --clearenv roles and managed lake_compiler stay separate.
        git_env = dict(os.environ)
        inject_burst_git_safe_directories(git_env, repo)
        cmd.extend(["--setenv", "GIT_CONFIG_COUNT", git_env["GIT_CONFIG_COUNT"]])
        for idx in range(int(git_env["GIT_CONFIG_COUNT"])):
            for field in ("KEY", "VALUE"):
                name = f"GIT_CONFIG_{field}_{idx}"
                cmd.extend(["--setenv", name, git_env[name]])
        # One read-only preflight in the actual burst environment, before any
        # provider can run raw Lake. No HOME config, shell wrapper or PATH change.
        preflight = Path(__file__).with_name("git_trust.py").resolve()
        cmd.extend(["--ro-bind", str(preflight), str(preflight)])
        interpreter = _git_preflight_interpreter([*_SYSTEM_READONLY_DIRS, *host_runtime_dirs])
        # Allocate only an identifier here. HOME may not exist/be writable until
        # launch; the gate/recorder creates its diagnostic directory lazily.
        report_dir = home / ".trellis" / "git-preflight" / f"launch-{uuid.uuid4().hex}"
        # Empty path is the documented inherit-PATH sentinel; it never means
        # an explicitly empty search path. The gate restores exec-time env bytes.
        inner_cmd = [str(interpreter), "-I", "-S", str(preflight), "--gate",
                     str(repo), role, git_preflight_path or "", str(report_dir), *inner_cmd]
    cmd.extend(["--chdir", str(cache_view if dependency_cache else repo)])
    cmd.extend(inner_cmd)
    if managed_agent:
        from trellis.dispatch.confinement import restrict_bwrap
        from trellis.host_runtime import worker_elan_home
        if burst_home is None:
            raise RuntimeError("parallel agents require a private burst home")
        # Package inputs may be hardlinked between isolated checkouts. Only
        # managed checker mirrors may build them; agent writes must never
        # mutate a sibling's package through a shared inode.
        packages = repo / ".lake/packages"
        if packages.exists():
            cmd[-len(inner_cmd):-len(inner_cmd)] = ["--ro-bind", str(packages), str(packages)]
        # Provider installs in writable task HOME are data, never executable
        # authority. Host provider runtimes and the deployed kernel are immutable.
        executable = [p for p in [*_SYSTEM_READONLY_DIRS, *host_runtime_dirs]
                      if p.exists() and p.resolve() != worker_elan_home().resolve()
                      and not p.resolve().is_relative_to(home)]
        # A standalone provider can live under its own state directory. The
        # later noexec state bind shadows the earlier executable bind there.
        # Restore only those already-approved immutable runtime subtrees;
        # surrounding provider state and the task home remain noexec data.
        for runtime_dir in host_runtime_dirs:
            if runtime_dir in executable and any(
                runtime_dir.resolve().is_relative_to(state_dir.resolve())
                for state_dir in provider_state_dirs if state_dir.exists()
            ):
                # State may contain an absolute `current` symlink. Mount the
                # resolved subtree; bwrap cannot create through that symlink
                # after the parent data mount has been installed.
                resolved_runtime = runtime_dir.resolve()
                cmd[-len(inner_cmd):-len(inner_cmd)] = ["--ro-bind", str(resolved_runtime), str(resolved_runtime)]
        kernel = os.environ.get("TRELLIS_TRELLIS_KERNEL_CMD", "")
        parts = shlex.split(kernel)
        if len(parts) == 1 and Path(parts[0]).is_file():
            executable.append(Path(parts[0]).resolve())
            # The existing extra-readonly paths may bind its enclosing tree;
            # only this file becomes executable within the data tree.
            cmd[-len(inner_cmd):-len(inner_cmd)] = ["--ro-bind", parts[0], parts[0]]
            # check.py prefers its materialized runtime binary. Permit that
            # exact deployed implementation as well, never an arbitrary native
            # executable merely because it lives under .trellis/runtime/bin.
            local_kernel = repo / ".trellis/runtime/bin/trellis_runtime_cli"
            if local_kernel.is_file():
                import hashlib
                def file_hash(path):
                    with Path(path).open('rb') as stream:
                        return hashlib.file_digest(stream, 'sha256').digest()
                if not os.path.samefile(local_kernel, parts[0]) and file_hash(local_kernel) != file_hash(parts[0]):
                    raise RuntimeError("parallel task kernel binary differs from the deployed implementation")
                executable.append(local_kernel.resolve())
                cmd[-len(inner_cmd):-len(inner_cmd)] = ["--ro-bind", str(local_kernel), str(local_kernel)]
        # These host-provisioned scripts must remain directly invocable using
        # the exact commands in prompts. Their native Lean work is brokered.
        for name in ("check-bin/lean", "check-bin/lake", "check-job", "check_job.py", "compiler-command-guard", "incremental-check",
                     "check.py", "check_node.sh", "check_tablet.sh"):
            shim = repo / ".trellis/scripts" / name
            if shim.is_file():
                executable.append(shim)
                cmd[-len(inner_cmd):-len(inner_cmd)] = ["--ro-bind", str(shim), str(shim)]
        if compiler_confined:
            cmd = restrict_bwrap(cmd, view=data_view or None, native_helper=native_bwrap or None, executable_roots=executable,
                                 scratch=temporary / "confinement", compiler=compiler_capability)
        else:
            # Cooperative resource guard: block the ordinary absolute elan
            # entrypoints as well as routing PATH commands to the checker.
            # Leave toolchain bytes readable for existing identity hashing.
            guard = repo / ".trellis/scripts/compiler-command-guard"
            if not guard.is_file():
                raise RuntimeError("managed compiler command guard is not provisioned in this workspace")
            for name in ("elan", "lean", "lake", "leanc", "leanchecker", "leanmake", "leanpkg"):
                entry = worker_elan_home() / "bin" / name
                if entry.is_file():
                    cmd[-len(inner_cmd):-len(inner_cmd)] = ["--ro-bind", str(guard), str(entry)]
            cmd[-len(inner_cmd):-len(inner_cmd)] = ["--setenv", "TRELLIS_COMPILER_CAPABILITY", compiler_capability]
            if no_compiler:
                for name in sorted(compiler_envs):
                    cmd[-len(inner_cmd):-len(inner_cmd)] = ["--unsetenv", name]
    if engineering:
        # The admitted host entrypoint supplies the whole environment before
        # exec. Inheritance keeps values out of command arguments and scripts.
        clean, index = [], 0
        while index < len(cmd):
            if cmd[index] == "--chdir":
                clean.extend([*cmd[index:index + 2], "--remount-ro", "/"])
                _check_engineering_home_mounts(clean, home)
                clean.extend(cmd[index + 2:])
                break
            count = {"--clearenv": 1, "--setenv": 3, "--unsetenv": 2}.get(cmd[index])
            if count:
                index += count
            else:
                clean.append(cmd[index]); index += 1
        return engineering_context.admit_command(clean, home=home, work=repo,
            request=engineering_request, inherited=environment, files=engineering_files)
    return cmd


def _repo_writable_paths(
    repo: Path, *, role: str, is_isabelle: Optional[bool] = None,
    extra_writable_paths: Iterable[str] = (),
) -> List[Path]:
    # `.lake/build` (the local lake olean scratch) is a LEAN-only writable dir:
    # the worker's inner edit-compile-fix loop runs `lake build`/`lake env lean`
    # there. An Isabelle worker has no lake step (it checks via the socket-side
    # `isabelle build`), so creating `<repo>/.lake/build` on an Isabelle repo is
    # pure Lean residue. Gate every `.lake/build` writable entry on the backend,
    # matching `wrap_command`'s `is_isabelle` (role-gated, so a Lean repo and any
    # non-toolchain role are byte-identical to before). Auto-detect from the repo
    # when the caller does not pass an explicit value.
    if is_isabelle is None:
        is_isabelle = (
            role in _ISABELLE_TOOLCHAIN_ROLES and repo_targets_isabelle(repo)
        )
    extra: List[Path] = []
    for raw in extra_writable_paths:
        relative = Path(raw)
        path = (repo / relative).resolve()
        if relative.is_absolute() or path == repo.resolve() or not path.is_relative_to(repo.resolve()):
            raise ValueError(f"Extra writable path must be inside the repository: {raw}")
        extra.append(path)
    state_dir = repo / ".trellis"
    if role == "engineering":
        paths = [*extra, *[state_dir / name for name in ("logs", "staging", "chats", "tmp")]]
        for path in paths:
            if not path.exists():
                path.mkdir(parents=True)
            if path.is_symlink():
                raise ValueError("Engineering writable paths cannot be aliases")
        return paths
    if role == "lake_compiler":
        # Narrow allowlist for supervisor-side bwrap'd lake invocations.
        # Threat-model mitigation 1: the unified-checker server runs lake
        # inside this bwrap to confine elaboration RCE blast radius. The
        # supervisor repo lives under <worker_repo>/.trellis/supervisor/
        # repo (see supervisor_workspace.py); only build outputs and the
        # scratch dirs used by read-only probes are writable. Tablet sources
        # stay under the repo-wide read-only bind: elaboration executes
        # arbitrary metaprogram code, so letting that process rewrite the
        # source it is certifying would break the source→artifact causal bind.
        # Crucially excluded: the supervisor home, runtime/<*>/private (kernel
        # baseline), runtime tools, every other state_dir subtree.
        compiler_paths: List[Path] = [
            repo / ".lake" / "build",
            # `lake exe cache get` writes cache-derived state under
            # `.lake/config/` (e.g. cache-hashes.json) on the supervisor
            # repo. Without this entry the first `prepare_compiled_support`
            # invocation under --with-checker-rpc fails with EROFS the
            # moment lake updates its config index. Prewarmed dependency
            # configs are mirrored read-only inside package checkouts; only
            # role-local config roots explicitly listed below are writable.
            repo / ".lake" / "config",
            state_dir / "tmp",
            state_dir / "staging",
        ]
        # `.lake/manifest.json` is an at-most-one-file manifest cache
        # rewritten by some `lake exe cache get` paths. Treat as a writable
        # FILE (pre-touched, then bwrap binds the file rather than a dir).
        compiler_files: List[Path] = [
            repo / ".lake" / "manifest.json",
        ]
        packages_root_lc = repo / ".lake" / "packages"
        if packages_root_lc.exists():
            for package_dir in packages_root_lc.iterdir():
                if not package_dir.is_dir():
                    continue
                compiler_paths.append(package_dir / ".lake" / "build")
                # `lake exe cache get` writes per-package config state
                # under `<pkg>/.lake/config/` during cache-state checks
                # (mirrors the top-level `.lake/config/` entry above). This
                # covers packages rooted at the checkout top level; a
                # subdirectory dependency such as Aeneas instead uses its
                # prewarmed, mirrored config without mutating package source.
                compiler_paths.append(package_dir / ".lake" / "config")
                # proofwidgets's `widget/` task refreshes
                # `package-lock.json.hash` during full lake builds
                # (see lake's "Replaying proofwidgets/widgetPackageLock"
                # step). Pre-RPC the worker bwrap had this writable;
                # the lake_compiler bwrap's narrower allowlist needs it
                # too or `lean_build_tablet` fails with "read-only file
                # system" mid-build.
                widget_dir_lc = package_dir / "widget"
                if widget_dir_lc.exists():
                    compiler_paths.append(widget_dir_lc)
        unique_lc: List[Path] = []
        seen_lc: set[Path] = set()
        for path in compiler_paths:
            if path in seen_lc:
                continue
            path.mkdir(parents=True, exist_ok=True)
            unique_lc.append(path)
            seen_lc.add(path)
        for path in compiler_files:
            if path in seen_lc:
                continue
            path.parent.mkdir(parents=True, exist_ok=True)
            if not path.exists():
                path.touch()
            unique_lc.append(path)
            seen_lc.add(path)
        if extra:
            raise ValueError("The lake_compiler write boundary does not accept extra paths")
        return unique_lc

    if role in _SEALED_CHECKER_ROLES:
        return []

    # Phase 0 is pre-campaign and has a deliberately smaller write surface.
    # The complete attempt root is read-only; these pre-created overlays are
    # the candidate and agent-delivery channel named by the strict request,
    # plus the transcript store the provider backends write their live
    # `output.log`/`prompt.txt` into (`<work_dir>/.trellis/chats`, the same
    # directory a campaign worker gets via `state_dir / "chats"` below).
    # Without it the burst script's `> "$LOG_FILE"` redirect hits EROFS and
    # the agent exits before it starts. `.trellis/scripts/check.py` stays
    # read-only because only the `chats` subtree is bound writable.
    if role == "source_adaptation_worker":
        paths = [repo / "candidate", repo / "agent-output", repo / ".trellis" / "chats"]
        for path in paths:
            path.mkdir(parents=True, exist_ok=True)
        return paths
    if role == "source_adaptation_reviewer":
        paths = [repo / "agent-output", repo / ".trellis" / "chats"]
        for path in paths:
            path.mkdir(parents=True, exist_ok=True)
        return paths

    # `.lake/build` and the cheat-trace `checker/` dir stay writable for the
    # worker even in RPC mode: the worker still needs `.lake/build` for the
    # fast inner edit-compile-fix loop (`lake build Tablet.NodeName`,
    # `lake env lean Tablet/X.lean`, `lake env lean .trellis/scratch/foo.lean`
    # all need .olean writes). Sign-off authority still flows through the
    # supervisor's separate `.trellis/supervisor/repo/.lake/build/`, so any
    # local olean drift is caught at the deterministic-check gate.
    paths: List[Path] = [
        state_dir / "chats",
        state_dir / "logs",
        state_dir / "scratch",
        state_dir / "tmp",
        state_dir / "sessions",
        state_dir / "staging",
        state_dir / "checker",
        # Reviewer Lean scratchpad for StuckMathAudit mode (per-burst
        # subdirs created by bridge_prompts.py). Writable so probe.lean
        # and reviewer_notes.md actually persist to host — previously the
        # scratch lived under <runtime>/stuck-math-audit/ which is not in
        # this allowlist, so writes died in the bwrap tmpfs overlay.
        state_dir / "stuck-math-audit",
    ]
    if not is_isabelle:
        # Lean-only: the worker's local `lake build`/`lake env lean` olean scratch.
        paths.append(repo / ".lake" / "build")
    runtime_root = state_dir / "runtime"
    if runtime_root.exists():
        for child in runtime_root.iterdir():
            if not child.is_dir():
                continue
            paths.append(child / "logs")
            paths.append(child / "staging")
            # NOTE: `child / "private"` is intentionally NOT added.
            # The bridge writes `<canonical>.acceptance.json` there
            # and the SIGHUP-recovery path loads it back as the
            # trusted normalization baseline (see
            # `_bridge_private_state_dir` and
            # `_finalize_recovered_worker_response` in
            # `trellis/runtime/bridge.py`). If `private/` ever joins
            # this allowlist, a dishonest worker can overwrite the
            # baseline between writing `.done` and a supervisor
            # restart, so the recovered normalization absorbs
            # unauthorized writes into the baseline. Worker still
            # gets read access via the repo-wide `--ro-bind` below.
    # Per-package lake build outputs (mathlib etc.) and widget rebuild
    # output. The worker needs these writable to run lake locally for the
    # inner edit-compile-fix loop; without them, even
    # `lake env lean .trellis/scratch/foo.lean` fails because lean can't
    # find Mathlib oleans during import resolution.
    #
    # Isabelle runs skip the whole loop (`is_isabelle`): there is no lake
    # step, so no package build closure to bind.
    if not is_isabelle:
        packages_root = repo / ".lake" / "packages"
        if packages_root.exists():
            for package_dir in packages_root.iterdir():
                if not package_dir.is_dir():
                    continue
                paths.append(package_dir / ".lake" / "build")
                widget_dir = package_dir / "widget"
                if widget_dir.exists():
                    paths.append(widget_dir)
    if role == "worker":
        paths.append(repo / "Tablet")
        # Deviation files live at `reference/<id>.tex` (see artifact_validation.rs
        # and the worker prompt's DEVIATIONS section). Worker must be able to add,
        # rewrite, and `rm` these files; PV primary evidence uses
        # reference/model-evidence/. The dir is created on first use here.
        paths.append(repo / "reference")
    paths.extend(extra)
    unique: List[Path] = []
    seen: set[Path] = set()
    for path in paths:
        if path in seen:
            continue
        if path.suffix:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.touch(exist_ok=True)
        else:
            path.mkdir(parents=True, exist_ok=True)
        unique.append(path)
        seen.add(path)
    return unique


def declared_repo_writable_paths(
    repo: Path, *, role: str, sandbox: Optional[SandboxConfig] = None
) -> List[Path]:
    return [path.resolve() for path in _repo_writable_paths(
        repo.resolve(), role=role,
        extra_writable_paths=sandbox.extra_writable_paths if sandbox is not None else (),
    )]


def _path_is_within(path: Path, root: Path) -> bool:
    return path == root or path.is_relative_to(root)


def _snapshot_repo_tree(repo: Path) -> Dict[Path, tuple[str, int, int]]:
    repo = repo.resolve()
    snapshot: Dict[Path, tuple[str, int, int]] = {}
    for path in [repo, *repo.rglob("*")]:
        try:
            stat = path.lstat()
        except OSError:
            continue
        kind = "dir" if path.is_dir() else "file"
        snapshot[path.resolve()] = (kind, stat.st_size, stat.st_mtime_ns)
    return snapshot


def _repo_changed_paths(
    before: Dict[Path, tuple[str, int, int]],
    after: Dict[Path, tuple[str, int, int]],
) -> list[Path]:
    changed: list[Path] = []
    for path in sorted(set(before) | set(after)):
        if before.get(path) != after.get(path):
            changed.append(path)
    return changed


def _repo_write_violations(
    repo: Path,
    *,
    role: str,
    changed_paths: Iterable[Path],
    sandbox: Optional[SandboxConfig] = None,
) -> list[Path]:
    allowlist = declared_repo_writable_paths(repo, role=role, sandbox=sandbox)
    violations: list[Path] = []
    for raw_path in changed_paths:
        path = raw_path.resolve()
        if not _path_is_within(path, repo.resolve()):
            continue
        if any(_path_is_within(path, allowed) for allowed in allowlist):
            continue
        violations.append(path)
    return violations


def _sandbox_exec_command(
    *,
    inner: List[str],
    sandbox: Optional[SandboxConfig],
    repo: Path,
    burst_home: Optional[Path],
    role: str,
) -> List[str]:
    """Wrap `inner` in the sandbox (bwrap). Phase 4 of the bwrap-only
    migration: this no longer applies a `sudo -n -u burst_user env ...`
    outer wrap. The burst runs as the supervisor user inside bwrap; provider
    auth + per-user state live in the per-burst fake-home that the
    bridge materializes under `<runtime>/burst-homes/<burst_id>/` and
    threads through `burst_home`."""
    return wrap_command(
        inner,
        sandbox=sandbox,
        work_dir=repo,
        burst_home=burst_home,
        role=role,
    )

def _first_probe_node(repo: Path) -> Optional[str]:
    tablet_dir = repo / "Tablet"
    if not tablet_dir.exists():
        return None
    for lean_file in sorted(tablet_dir.glob("*.lean")):
        stem = lean_file.stem
        if stem in {"Preamble", "Axioms"}:
            continue
        return stem
    return None


def certify_worker_checker_surface(
    *,
    sandbox: Optional[SandboxConfig],
    repo_path: Path,
    burst_home: Optional[Path] = None,
    probe_node: Optional[str] = None,
) -> Tuple[bool, str]:
    """Run real worker-check commands and reject undeclared repo writes."""
    repo = repo_path.resolve()
    check_script = repo / ".trellis" / "scripts" / "check.py"
    if not check_script.is_file():
        return False, f"worker checker script is missing: {check_script}"

    commands: list[tuple[str, list[str]]] = [
        (
            "tablet",
            ["python3", str(check_script), "tablet", str(repo)],
        )
    ]
    node_name = (probe_node or _first_probe_node(repo) or "").strip()
    if node_name:
        commands.append(
            (
                f"node:{node_name}",
                ["python3", str(check_script), "node", node_name, str(repo)],
            )
        )

    failures: list[str] = []
    for label, inner in commands:
        before = _snapshot_repo_tree(repo)
        proc = subprocess.run(
            _sandbox_exec_command(
                inner=inner,
                sandbox=sandbox,
                repo=repo,
                burst_home=burst_home,
                role="worker",
            ),
            capture_output=True,
            text=True,
        )
        after = _snapshot_repo_tree(repo)
        violations = _repo_write_violations(
            repo,
            role="worker",
            changed_paths=_repo_changed_paths(before, after),
            sandbox=sandbox,
        )
        if violations:
            rel = ", ".join(sorted(str(path.relative_to(repo)) for path in violations))
            failures.append(f"{label} wrote outside worker allowlist: {rel}")
        if proc.returncode != 0:
            detail = (proc.stderr or proc.stdout or f"exit {proc.returncode}").strip()
            failures.append(f"{label} failed during sandbox certification: {detail}")
    if failures:
        return False, "; ".join(failures)
    return True, ""


def probe_sandbox(
    *,
    sandbox: Optional[SandboxConfig],
    work_dir: Path,
    burst_home: Optional[Path] = None,
) -> Tuple[bool, str]:
    """Return whether the configured sandbox can successfully execute a trivial command."""
    if sandbox is None or not sandbox.enabled:
        return True, ""
    try:
        inner = wrap_command(
            ["/bin/bash", "-c", "true"],
            sandbox=sandbox,
            work_dir=work_dir,
            burst_home=burst_home,
        )
    except Exception as exc:
        return False, str(exc)

    # Phase 4: probe runs directly as the supervisor user; no sudo wrap.
    proc = subprocess.run(inner, capture_output=True, text=True)
    if proc.returncode == 0:
        return True, ""
    detail = (proc.stderr or proc.stdout or f"exit {proc.returncode}").strip()
    return False, detail


def probe_worker_environment(
    *,
    sandbox: Optional[SandboxConfig],
    repo_path: Path,
    burst_home: Optional[Path] = None,
    provider_commands: Iterable[str] = (),
    certify_checker_surface: bool = False,
) -> Tuple[bool, str]:
    """Verify that the real worker sandbox surface is usable."""
    repo = repo_path.resolve()
    scratch_dir = worker_scratch_dir(repo)
    scratch_notes = worker_scratch_notes_path(repo)
    is_isabelle = repo_targets_isabelle(repo)
    if is_isabelle:
        # GAP 1 gate: on an isabelle_hol run the worker's interactive proof loop
        # needs `isabelle` on PATH and the warm session heaps RO-mounted. Assert
        # both so an env missing the bind/PATH fails setup loudly (the original
        # symptom: the worker's notes.md said "Isabelle NOT installed → gate is
        # STRUCTURAL only" and it blind-transcribed paper proofs).
        toolchain_checks = [
            "command -v isabelle >/dev/null",
            'test -d "${ISABELLE_HOME_USER:-}/heaps"',
        ]
    else:
        # Lean run: the toolchain the worker drives is lake/lean.
        toolchain_checks = [
            "command -v lake >/dev/null",
            "command -v lean >/dev/null",
        ]
    script_parts = [
        "set -euo pipefail",
        f"test -d {shlex.quote(str(repo / 'Tablet'))}",
        f"test -w {shlex.quote(str(repo / 'Tablet'))}",
        f"test -d {shlex.quote(str(scratch_dir))}",
        f"touch {shlex.quote(str(scratch_notes))}",
        f"test -w {shlex.quote(str(scratch_notes))}",
        *toolchain_checks,
        "command -v python3 >/dev/null",
        "if touch __trellis_sandbox_repo_root_probe 2>/dev/null; then rm -f __trellis_sandbox_repo_root_probe; exit 97; fi",
    ]
    for provider in provider_commands:
        provider_name = str(provider).strip()
        if provider_name in {"deepseek", "kimi"}:
            # Direct HTTP backends run in the supervisor; their shell tool only
            # needs the system tools already checked above, not a provider CLI.
            continue
        if provider_name:
            # GATE H: name the provider on failure instead of a bare non-zero
            # exit, so setup says *which* CLI the worker sandbox PATH couldn't
            # find rather than an opaque "probe failed".
            q = shlex.quote(provider_name)
            script_parts.append(
                f"command -v {q} >/dev/null || {{ "
                f"echo \"provider CLI '{provider_name}' not found on the worker "
                f"sandbox PATH (PATH=$PATH)\" >&2; exit 1; }}"
            )
    inner = wrap_command(
        ["/bin/bash", "-c", "; ".join(script_parts)],
        sandbox=sandbox,
        work_dir=repo,
        burst_home=burst_home,
        role="worker",
    )
    # Phase 4: probe runs directly as the supervisor user inside bwrap; no sudo wrap.
    # GATE H: run the probe with EXACTLY the burst's PATH (`worker_path_env`),
    # not the supervisor's inherited PATH. The real burst launches as
    # `env PATH=worker_path_env(...) bwrap ... <provider>` and bwrap inherits
    # PATH from the parent env (no `--setenv PATH`). If we probed under the
    # supervisor's richer PATH, `command -v <provider>` could pass here yet
    # the burst would still exit 127 — the false-confidence gap this fixes.
    probe_env = dict(os.environ)
    probe_env["PATH"] = worker_path_env(burst_home, isabelle=is_isabelle)
    proc = subprocess.run(inner, capture_output=True, text=True, env=probe_env)
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or f"exit {proc.returncode}").strip()
        if proc.returncode == 97:
            detail = "sandbox allowed an unexpected write at repo root"
        return False, detail
    # Probe succeeded. Optionally also verify the cheat-detection surface
    # — `.trellis/checker/` writability + worker-side checker write
    # behavior — which is the load-bearing trace for
    # `bridge._checker_mismatch_detail`. Without this, an out-of-spec
    # sandbox losing the checker dir silently disables cheat detection.
    if certify_checker_surface:
        return certify_worker_checker_surface(
            sandbox=sandbox,
            repo_path=repo,
            burst_home=burst_home,
        )
    return True, ""
