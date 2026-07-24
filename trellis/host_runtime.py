"""Resolve shared host runtime paths for sandboxing and burst execution."""

from __future__ import annotations

import os
import shutil
from pathlib import Path
from typing import Iterable, List, Optional


DEFAULT_WORKER_PATH = "/usr/local/bin:/usr/bin:/bin"
DEFAULT_ELAN_HOME = os.path.expanduser("~/.elan")


def worker_path_env(burst_home: Optional[Path] = None) -> str:
    """PATH env value for worker bursts, preferring per-burst-user installs.

    When `burst_home` is provided, prepends two locations the burst user
    typically owns its agent CLIs (claude/gemini/codex) under:

      1. `<burst_home>/.trellis-npm/bin` — supervisor-managed gemini install
         (see `tmux_backend.ensure_gemini_cli_updated`).
      2. `<burst_home>/.local/share/npm-global/bin` — conventional per-user
         `npm config set prefix` location for `npm install -g`.

    THEN prepends the directories that actually contain the resolved
    provider CLIs (`codex`/`claude`/`gemini`) — the *same* directories the
    bwrap sandbox bind-mounts read-only via `host_runtime_readonly_roots`
    (GATE H). Without this, the sandbox can *reach* a provider CLI by full
    path (nvm install, operator's real `~/.local/share/npm-global/bin`,
    `/usr/local/bin` wrapper, …) but the bare command name is NOT on PATH,
    so the burst's `codex`/`claude`/`gemini` invocation exits 127, the
    artifact is empty, and the result degrades to a generic `Malformed`.
    Reusing `worker_provider_bin_dirs` (which shares the resolver with the
    read-only binds) keeps PATH and the binds from drifting apart again.

    Falling through to `DEFAULT_WORKER_PATH` for system tools and
    `/usr/local/bin/` wrappers (the canonical `setup_permissions.sh`
    install layout). Without this prepend, hosts that use a per-user
    npm-global install for the burst user (instead of /usr/local/bin
    wrappers) cannot find `claude`/`codex`/`gemini` at burst launch — the
    tmux pane runs `claude` against PATH=/usr/local/bin:..., the binary
    isn't found, the inner shell exits, the pane dies, and
    settle_until_ready then waits the full timeout for a session that
    never existed.

    Returns DEFAULT_WORKER_PATH unchanged when burst_home is None, for
    callers that genuinely don't have a burst user (legacy code paths).

    Additive guarantee: every entry of the previous value is preserved in
    order; resolved provider dirs are only *prepended*, never removed, so a
    host whose CLIs already resolved via the existing entries keeps working.
    """
    if burst_home is None:
        return DEFAULT_WORKER_PATH
    base_entries = [
        f"{burst_home}/.trellis-npm/bin",
        f"{burst_home}/.local/share/npm-global/bin",
        *DEFAULT_WORKER_PATH.split(":"),
    ]
    # Prepend the resolved provider-CLI bin dirs (GATE H) AND the elan bin dir
    # for lake/lean (GATE I) — the same dirs the sandbox binds read-only — so the
    # bare command names resolve on PATH. The elan home is bound via
    # `worker_elan_home()`, but on a host where `lake` lives only under
    # `~/.elan/bin` (not symlinked into /usr/bin or wrapped in /usr/local/bin as
    # the canonical `setup_permissions.sh` layout does) it was off PATH, so the
    # worker's `lake env lean` exited 127. Additive: base entries are preserved.
    candidate_dirs: List[str] = [
        str(worker_elan_home() / "bin"),
        *(str(p) for p in worker_provider_bin_dirs(burst_home=burst_home)),
    ]
    seen: set[str] = set()
    prepend_dirs: List[str] = []
    for d in candidate_dirs:
        if d not in base_entries and d not in seen and Path(d).is_dir():
            seen.add(d)
            prepend_dirs.append(d)
    return ":".join([*prepend_dirs, *base_entries])


def provider_cli_not_found_detail(
    provider: str,
    *,
    exit_code: Optional[int],
    output: str,
    burst_home: Optional[Path] = None,
) -> Optional[str]:
    """Operator-facing message when a burst couldn't find its provider CLI.

    GATE H: a burst that exits 127 (or whose output shows the shell's
    "command not found") almost always means the provider CLI
    (`codex`/`claude`/`gemini`) is installed somewhere the worker sandbox
    PATH (`worker_path_env`) doesn't reach — historically this surfaced as a
    bare generic `Malformed` with an empty artifact. Detect it and name the
    provider + the PATH we looked on, so the operator can fix the install
    instead of chasing a phantom model failure.

    Returns None when this doesn't look like a not-found failure (so callers
    leave their existing error semantics untouched).
    """
    name = str(provider or "").strip() or "<provider>"
    text = output or ""
    looks_not_found = exit_code == 127 or (
        f"{name}: command not found" in text
        or f"{name}: not found" in text
        or "command not found" in text
    )
    if not looks_not_found:
        return None
    path_val = worker_path_env(burst_home)
    return (
        f"provider CLI '{name}' not found in the worker sandbox PATH "
        f"(burst exited {exit_code if exit_code is not None else '?'}). "
        f"Install it where trellis can reach it (per-user npm-global, nvm, "
        f"or /usr/local/bin — see INSTALLATION.md) so it lands on the burst "
        f"PATH={path_val}"
    )


def worker_elan_home() -> Path:
    raw = os.environ.get("ELAN_HOME", DEFAULT_ELAN_HOME).strip() or DEFAULT_ELAN_HOME
    path = Path(raw).expanduser()
    try:
        return path.resolve(strict=False)
    except OSError:
        return path


def _resolved_command_path(name: str) -> Path | None:
    location = shutil.which(name)
    if not location:
        return None
    try:
        return Path(location).resolve(strict=True)
    except FileNotFoundError:
        return Path(location).resolve(strict=False)


def _nvm_version_root(path: Path) -> Path | None:
    for candidate in [path, *path.parents]:
        if not candidate.name.startswith("v"):
            continue
        parent = candidate.parent
        if parent.name != "node":
            continue
        grandparent = parent.parent
        if grandparent.name != "versions":
            continue
        if grandparent.parent.name != ".nvm":
            continue
        return candidate
    return None


def _claude_runtime_root(path: Path) -> Path | None:
    for candidate in [path, *path.parents]:
        if candidate.name != "claude":
            continue
        parent = candidate.parent
        if parent.name != "share":
            continue
        if parent.parent.name != ".local":
            continue
        return candidate
    return None


def _symlink_chain_parent_dirs(path: Path) -> List[Path]:
    """Walk the symlink chain from ``path`` to its final target and return
    the parent directory of every hop along the way.

    Needed so the sandbox mounts each intermediate directory — otherwise a
    launcher that chains through, e.g., ``/usr/local/bin/claude →
    ~/.local/bin/claude → .../versions/X.Y.Z`` breaks inside bwrap whenever
    a middle hop's parent isn't already mounted.
    """
    parents: List[Path] = []
    seen: set[Path] = set()
    current = path
    for _ in range(32):
        if current in seen:
            break
        seen.add(current)
        parents.append(current.parent)
        if not current.is_symlink():
            break
        try:
            raw_target = os.readlink(current)
        except OSError:
            break
        target = Path(raw_target)
        if not target.is_absolute():
            target = current.parent / target
        current = target
    return parents


def _runtime_roots_from_path(raw_path: Path, resolved: Path) -> List[Path]:
    """Compute readonly bind roots from a launcher path + its resolved target."""
    roots: List[Path] = []
    # Cover every hop of the launcher's symlink chain so a multi-level
    # launcher (e.g. /usr/local/bin/claude → ~/.local/bin/claude →
    # versioned binary) stays resolvable inside the sandbox.
    for parent in _symlink_chain_parent_dirs(raw_path):
        if parent not in roots:
            roots.append(parent)

    for resolver in (_nvm_version_root, _claude_runtime_root):
        root = resolver(resolved)
        if root is not None:
            if root not in roots:
                roots.append(root)
            return roots

    target_root = resolved if resolved.is_dir() else resolved.parent
    if target_root not in roots:
        roots.append(target_root)
    return roots


def _burst_user_command_path(
    burst_home: Path, tool: str,
) -> Optional[Path]:
    """Look up `tool` under the burst user's per-user install layouts.

    Mirrors the prefix order used by `worker_path_env`: the
    supervisor-managed gemini install at `<home>/.trellis-npm/bin`
    first, then the conventional npm-global install set by
    `npm config set prefix ~/.local/share/npm-global`.

    Without this, `runtime_roots_for_command` would fall through to
    `shutil.which` (which scans the *supervisor's* PATH), making bwrap
    try to bind-mount the supervisor's npm-global tree, which would
    fail if the candidate isn't actually present.

    Phase 4 bwrap-only migration: the per-burst fake-home is
    supervisor-owned, so the supervisor can stat candidates directly.
    """
    candidates = [
        burst_home / ".trellis-npm" / "bin" / tool,
        burst_home / ".local" / "share" / "npm-global" / "bin" / tool,
    ]
    for candidate in candidates:
        try:
            if candidate.exists():
                return candidate
        except OSError:
            continue
    return None


def runtime_roots_for_command(
    name: str,
    *,
    burst_home: Optional[Path] = None,
) -> List[Path]:
    """Bind-mount roots needed for `name` to be invokable inside bwrap.

    When `burst_home` is provided AND has the tool installed under its
    own home (per-user npm install), we resolve against that copy.

    Falls back to the supervisor's PATH (`shutil.which`) when
    `burst_home` is None or has no copy.
    """
    if burst_home is not None:
        candidate = _burst_user_command_path(burst_home, name)
        if candidate is not None:
            return [candidate.parent]
    raw_location = shutil.which(name)
    if raw_location is None:
        return []
    raw_path = Path(raw_location)
    resolved = _resolved_command_path(name)
    if resolved is None:
        return []
    return _runtime_roots_from_path(raw_path, resolved)


def resolved_command_bin_dir(
    name: str,
    *,
    burst_home: Optional[Path] = None,
) -> Optional[Path]:
    """Directory that actually contains the runnable `name` launcher.

    This is the PATH-facing counterpart to `runtime_roots_for_command`
    (which returns the broader set of bwrap *bind* roots, e.g. a whole nvm
    version tree). Here we return the single directory the bare command
    name resolves from, so it can be placed on the worker PATH:

      - burst-user install: `<burst_home>/.trellis-npm/bin` or
        `<burst_home>/.local/share/npm-global/bin` (the candidate's parent).
      - otherwise: the parent of `shutil.which(name)` — i.e. the launcher
        directory (nvm `.../vX.Y.Z/bin`, `~/.local/bin`, `/usr/local/bin`).

    Both forms are a subset of `runtime_roots_for_command(...)` (the bind
    roots always include the launcher's parent dir as their first hop), so
    PATH and the read-only binds resolve to the same install and cannot
    drift. Returns None when the tool isn't found anywhere.
    """
    if burst_home is not None:
        candidate = _burst_user_command_path(burst_home, name)
        if candidate is not None:
            return candidate.parent
    raw_location = shutil.which(name)
    if raw_location is None:
        return None
    return Path(raw_location).parent


def worker_provider_bin_dirs(
    *,
    burst_home: Optional[Path] = None,
    include_tools: Iterable[str] = ("codex", "claude", "gemini"),
) -> List[Path]:
    """De-duplicated launcher dirs for the provider CLIs, for the PATH.

    Uses the same per-tool resolution (`resolved_command_bin_dir`) that the
    read-only binds derive from, so the worker PATH and the bwrap binds stay
    consistent. Only existing directories are returned; order follows
    `include_tools`.
    """
    dirs: List[Path] = []
    seen: set[Path] = set()
    for tool in include_tools:
        bin_dir = resolved_command_bin_dir(str(tool), burst_home=burst_home)
        if bin_dir is None or bin_dir in seen:
            continue
        try:
            if not bin_dir.exists():
                continue
        except OSError:
            # Burst-user tree may be unreadable from the supervisor UID;
            # `_burst_user_command_path` already stat-confirmed existence
            # for those, and `shutil.which` only returns extant paths, so an
            # OSError here is conservative — include it so PATH still points
            # where the sandbox binds it.
            pass
        dirs.append(bin_dir)
        seen.add(bin_dir)
    return dirs


def host_runtime_readonly_roots(
    *,
    burst_home: Optional[Path] = None,
    include_tools: Iterable[str] = ("codex", "claude", "gemini"),
) -> List[Path]:
    roots: list[Path] = [worker_elan_home()]
    for tool in include_tools:
        roots.extend(runtime_roots_for_command(
            str(tool), burst_home=burst_home,
        ))
    unique: list[Path] = []
    seen: set[Path] = set()
    for root in roots:
        if root in seen:
            continue
        try:
            if not root.exists():
                continue
        except PermissionError:
            # Burst-user tree may be unreadable from the supervisor's UID;
            # bwrap will resolve it from the burst user side, so include it.
            pass
        unique.append(root)
        seen.add(root)
    return unique
