"""Filesystem sandbox helpers for agent bursts."""

from __future__ import annotations

import os
import subprocess
import shutil
import shlex
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Tuple

from trellis.config import SandboxConfig
from trellis.host_runtime import (
    host_runtime_readonly_roots,
    worker_path_env,
)
from trellis.worker_scratch import worker_scratch_dir, worker_scratch_notes_path


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
# the supervisor. Bwrap strips the parent environment by default, so anything
# the kernel-inside-burst needs to honor must be explicitly forwarded.
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
    """Return the parent directory of the checker UNIX socket if set.

    When ``TRELLIS_CHECKER_SOCKET`` is exported (i.e. the operator opted
    into ``--with-checker-rpc`` mode) the worker bwrap must be able to
    ``connect()`` to the supervisor-side socket file. Forwarding the env
    var via ``--setenv`` is not enough on its own — bwrap strips the host
    filesystem by default, so the socket file would be missing inside the
    sandbox. This helper returns the *parent directory* of the socket
    path so the caller can ``--ro-bind`` it (the directory exists by the
    time the worker bwrap launches; the socket inside it becomes
    ``connect()``-able). Read-only is sufficient — the worker only needs
    to connect, not create the socket node. ``None`` when the env var is
    unset, the resolved path lives at the filesystem root, or the parent
    directory does not yet exist on the host.
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
    return parent.resolve()



def wrap_command(
    inner_cmd: List[str],
    *,
    sandbox: Optional[SandboxConfig],
    work_dir: Path,
    burst_home: Optional[Path] = None,
    role: str = "worker",
) -> List[str]:
    """Wrap a command in bubblewrap if sandboxing is enabled."""
    if sandbox is None or not sandbox.enabled:
        return inner_cmd
    if sandbox.backend != "bwrap":
        raise ValueError(f"Unsupported sandbox backend: {sandbox.backend}")
    if not bwrap_available():
        raise RuntimeError("bwrap is required for sandboxed bursts but is not installed")

    repo = work_dir.resolve()
    home = (burst_home or Path.home()).resolve()
    extra_readonly = _host_extra_readonly_paths()
    # The lake_compiler role only needs the lean toolchain (elan); explicitly
    # excluding the LLM-provider runtime trees (codex/claude/gemini) means lake
    # cannot read provider auth tokens even via host_runtime_readonly_roots.
    if role == "lake_compiler":
        host_runtime_tools: tuple[str, ...] = ()
    else:
        host_runtime_tools = ("codex", "claude", "gemini")
    host_runtime_dirs = host_runtime_readonly_roots(
        burst_home=burst_home,
        include_tools=host_runtime_tools,
    )
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
    elif role == "worker":
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
    repo_writable = _repo_writable_paths(repo, role=role)
    passthrough_envs = _passthrough_path_envs()
    passthrough_values = _passthrough_value_envs()

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
        *passthrough_envs.values(),
        *[p for p in _SYSTEM_READONLY_DIRS if p.exists()],
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
        "--tmpfs", str(_SANDBOX_TMPDIR),
    ]
    for parent in _ancestor_dirs(bind_targets):
        cmd.extend(["--dir", str(parent)])

    for path in _SYSTEM_READONLY_DIRS:
        if path.exists():
            cmd.extend(["--ro-bind", str(path), str(path)])
    for path in host_runtime_dirs:
        cmd.extend(["--ro-bind", str(path), str(path)])
    for path in extra_readonly:
        cmd.extend(["--ro-bind", str(path), str(path)])
    for path in extra_role_readonly:
        cmd.extend(["--ro-bind", str(path), str(path)])
    # The per-burst fake-home hard-links ~/.codex/, ~/.claude/, ~/.gemini/
    # from the supervisor's home so OAuth/auth state round-trips. But codex
    # CLI's session DB stores rollout paths as absolute (e.g.
    # ${TRELLIS_ROOT:-/path/to/trellis}/.codex/sessions/...). Inside the sandbox, those
    # absolute paths must resolve so resume can find prior rollouts.
    # Ro-bind the supervisor's provider state dirs at their absolute paths.
    for sup_dir in (
        Path.home() / ".codex",
        Path.home() / ".claude",
        Path.home() / ".gemini",
    ):
        if sup_dir.exists() and sup_dir.resolve() != home.resolve() and sup_dir != home:
            cmd.extend(["--ro-bind", str(sup_dir), str(sup_dir)])

    cmd.extend(["--bind", str(home), str(home)])
    cmd.extend(["--ro-bind", str(repo), str(repo)])
    for path in repo_writable:
        if path.exists():
            cmd.extend(["--bind", str(path), str(path)])
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
    cmd.extend(["--setenv", "HOME", str(home)])
    for name, path in passthrough_envs.items():
        cmd.extend(["--setenv", name, str(path)])
    for name, value in passthrough_values.items():
        cmd.extend(["--setenv", name, value])
    cmd.extend(["--setenv", "TMPDIR", str(_SANDBOX_TMPDIR)])
    cmd.extend(["--setenv", "TMP", str(_SANDBOX_TMPDIR)])
    cmd.extend(["--setenv", "TEMP", str(_SANDBOX_TMPDIR)])
    for name, value in _passthrough_value_envs().items():
        cmd.extend(["--setenv", name, value])
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
    cmd.extend(["--chdir", str(repo)])
    cmd.extend(inner_cmd)
    return cmd


def _repo_writable_paths(repo: Path, *, role: str) -> List[Path]:
    state_dir = repo / ".trellis"
    if role == "lake_compiler":
        # Narrow allowlist for supervisor-side bwrap'd lake invocations.
        # Threat-model mitigation 1: the unified-checker server runs lake
        # inside this bwrap to confine elaboration RCE blast radius. The
        # supervisor repo lives under <worker_repo>/.trellis/supervisor/
        # repo (see supervisor_workspace.py); only build outputs and the
        # Tablet source tree are writable, plus the scratch dirs that
        # print_axioms uses for its tempfile (.trellis/tmp/check and
        # .trellis/staging are exercised by observations.print_axioms).
        # Crucially excluded: the supervisor home, runtime/<*>/private (kernel
        # baseline), runtime tools, every other state_dir subtree.
        compiler_paths: List[Path] = [
            repo / ".lake" / "build",
            # `lake exe cache get` writes cache-derived state under
            # `.lake/config/` (e.g. cache-hashes.json) on the supervisor
            # repo. Without this entry the first `prepare_compiled_support`
            # invocation under --with-checker-rpc fails with EROFS the
            # moment lake updates its config index. Per-package
            # `<pkg>/.lake/config/` trees are role-specific and not
            # currently mirrored — if a future lake build path needs them
            # writable, mirror the per-package iteration that already
            # covers `<pkg>/.lake/build`.
            repo / ".lake" / "config",
            repo / "Tablet",
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
                # (mirrors the top-level `.lake/config/` entry above).
                # Surfaced when the supervisor workspace is bootstrapped
                # via `_ensure_supervisor_lake_packages` (which skips
                # per-package `.lake/` so each role keeps its own writable
                # state) — the first cache_get invocation under
                # --with-checker-rpc fails with EROFS on mathlib's
                # `.lake/config/` without this entry.
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
        return unique_lc

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
        repo / ".lake" / "build",
    ]
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
        # rewrite, and `rm` these files; the dir is created on first use here.
        paths.append(repo / "reference")
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


def declared_repo_writable_paths(repo: Path, *, role: str) -> List[Path]:
    return [path.resolve() for path in _repo_writable_paths(repo.resolve(), role=role)]


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
) -> list[Path]:
    allowlist = declared_repo_writable_paths(repo, role=role)
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
    script_parts = [
        "set -euo pipefail",
        f"test -d {shlex.quote(str(repo / 'Tablet'))}",
        f"test -w {shlex.quote(str(repo / 'Tablet'))}",
        f"test -d {shlex.quote(str(scratch_dir))}",
        f"touch {shlex.quote(str(scratch_notes))}",
        f"test -w {shlex.quote(str(scratch_notes))}",
        "command -v lake >/dev/null",
        "command -v lean >/dev/null",
        "command -v python3 >/dev/null",
        "if touch __trellis_sandbox_repo_root_probe 2>/dev/null; then rm -f __trellis_sandbox_repo_root_probe; exit 97; fi",
    ]
    for provider in provider_commands:
        provider_name = str(provider).strip()
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
    probe_env["PATH"] = worker_path_env(burst_home)
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
