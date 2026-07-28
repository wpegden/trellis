"""Per-burst fake-home materialization for the Phase 4 bwrap-only sandbox.

Each burst gets a DISTINCT, per-burst fake home under
``<runtime>/burst-homes/<burst_id>/``. The fake-home is seeded by
hard-linking the supervisor's ``~/.codex``, ``~/.claude``, and ``~/.gemini``
trees into it. Hard-links (rather than copies) make provider state
round-trip back to the supervisor's home: when the codex CLI refreshes
its OAuth token mid-burst and rewrites ``auth.json``, the inode is
shared, so the supervisor's view stays in sync; same for gemini
``sessions/``, ``tmp/<projdir>/chats/``, and codex
``~/.codex/sessions/``.

This module is intentionally side-effect-light and free of project
imports beyond stdlib — it is invoked by the bridge dispatch path
(see ``trellis.runtime.bridge._single_request_common``) and from tests
without dragging in heavyweight kernel/agent modules.
"""

from __future__ import annotations

import os
import shutil
from pathlib import Path
from typing import Iterable


# Provider state subdirectories the burst CLIs read from ``$HOME``.
# Kept narrow on purpose: the bwrap binds the fake-home, NOT the
# supervisor's real home, so anything the providers need at runtime must
# be either seeded here OR independently available inside the sandbox.
_PROVIDER_HOME_SUBDIRS: tuple[str, ...] = (
    ".codex",
    ".claude",
    ".gemini",
)


# Root-level provider state FILES (siblings of the dirs above) the burst
# CLIs read from ``$HOME``. ``~/.claude.json`` holds claude's onboarding
# state (theme/login completed); without it a burst-home is authed (via
# ``.claude/.credentials.json``) but not onboarded, so headless `claude`
# re-enters its interactive first-run flow and the burst hangs. codex and
# gemini keep all their state inside their dirs, so there is nothing to add
# for them.
_PROVIDER_HOME_ROOT_FILES: tuple[str, ...] = (
    ".claude.json",
)


def _hardlink_file(src: Path, dst: Path) -> None:
    """Hard-link a single regular file ``src`` into ``dst`` (refreshing any
    stale entry), falling back to a copy across devices. Every failure mode
    is tolerated — a missing or unreadable provider root file must never
    abort the seeding pass."""
    try:
        if not src.is_file():
            return
    except OSError:
        return
    try:
        dst.parent.mkdir(parents=True, exist_ok=True)
    except OSError:
        return
    try:
        if dst.exists() or dst.is_symlink():
            dst.unlink()
    except OSError:
        pass
    try:
        os.link(src, dst)
    except FileNotFoundError:
        return
    except OSError:
        try:
            shutil.copy2(src, dst)
        except OSError:
            return


def burst_homes_root(runtime_root: Path) -> Path:
    """Parent dir under ``<runtime>/`` that holds every per-burst home."""
    return runtime_root / "burst-homes"


def burst_home_path(runtime_root: Path, burst_id: str) -> Path:
    """Resolve the per-burst fake-home for ``burst_id``.

    ``burst_id`` is sanitized: anything outside ``[A-Za-z0-9._-]`` is
    replaced with ``_``. This is defense in depth: callers pass tmux
    session names / request ids which are already safe, but the path
    is bind-mounted into bwrap and the safety bar is a hair higher.
    """
    safe = "".join(
        ch if ch.isalnum() or ch in {"-", "_", "."} else "_" for ch in burst_id
    )
    safe = safe.strip("._-") or "burst"
    return burst_homes_root(runtime_root) / safe[:120]


def _hardlink_tree(src: Path, dst: Path) -> None:
    """Recursively hard-link every regular file under ``src`` into ``dst``.

    Directories are created with the same mode bits but not hard-linked
    (directories can't be hard-linked on Linux). Symlinks are recreated
    as symlinks; non-regular special files are skipped. Failures on
    individual entries are tolerated so an unreadable file inside
    ``~/.gemini/tmp/`` does not abort the whole seeding pass.

    Concurrent-write tolerance: provider CLIs (notably gemini) churn
    files in `~/.gemini/tmp/` while the supervisor is mid-seed; both
    the os.walk enumeration and the per-file os.link can race against
    deletions. All such races are caught and skipped.
    """
    try:
        src = src.resolve()
    except OSError:
        return
    if not src.is_dir():
        return
    try:
        walker = os.walk(src, followlinks=False)
    except OSError:
        return
    for root, dirs, files in walker:
        rel = Path(root).relative_to(src)
        target_dir = dst / rel
        try:
            target_dir.mkdir(parents=True, exist_ok=True)
        except OSError:
            continue
        try:
            mode = os.stat(root).st_mode & 0o7777
            os.chmod(target_dir, mode)
        except OSError:
            pass
        for name in files:
            src_path = Path(root) / name
            dst_path = target_dir / name
            # Tolerate every failure mode: the file may have vanished
            # between walk-time and link-time (gemini tool-outputs are
            # famously short-lived), or be a special-file (socket,
            # fifo) we can't link. Skip silently.
            try:
                try:
                    is_symlink = src_path.is_symlink()
                except OSError:
                    continue
                if is_symlink:
                    try:
                        raw_target = os.readlink(src_path)
                    except OSError:
                        continue
                    if dst_path.exists() or dst_path.is_symlink():
                        try:
                            dst_path.unlink()
                        except OSError:
                            continue
                    try:
                        os.symlink(raw_target, dst_path)
                    except OSError:
                        continue
                    continue
                try:
                    if not src_path.is_file():
                        continue
                except OSError:
                    continue
                if dst_path.exists():
                    try:
                        dst_path.unlink()
                    except OSError:
                        continue
                try:
                    os.link(src_path, dst_path)
                except FileNotFoundError:
                    # Source vanished mid-seed (gemini tmp churn).
                    continue
                except OSError:
                    # Cross-device or other link-failure: fall back to
                    # a regular copy, also tolerantly.
                    try:
                        shutil.copy2(src_path, dst_path, follow_symlinks=False)
                    except OSError:
                        continue
            except Exception:
                # Defensive catch-all; never let a single bad file
                # abort the whole seeding pass.
                continue


def seed_burst_home(
    runtime_root: Path,
    burst_id: str,
    *,
    source_home: Path | None = None,
    subdirs: Iterable[str] = _PROVIDER_HOME_SUBDIRS,
    root_files: Iterable[str] = _PROVIDER_HOME_ROOT_FILES,
    persistent: bool = False,
) -> Path:
    """Materialize the per-burst fake-home and return its path.

    Hard-links each subdir in ``subdirs`` from ``source_home`` into
    ``<runtime>/burst-homes/<burst_id>/``. Idempotent: if the dir
    already exists it is removed and reseeded so a re-dispatch picks up
    auth refreshes the supervisor has performed in the meantime.

    ``persistent=True`` skips the destructive reseed (so codex rollouts
    written by previous bursts at this same stable path survive for the
    next burst's ``codex exec resume <thread-id>`` to resolve). New
    files in supervisor's home are still hard-linked in for auth
    refreshes; existing files in the burst-home are left alone.

    Mode bits: the burst-home root is created mode 0o700 (burst CLIs
    bind to ``$HOME`` and many providers refuse to read configs in
    world-readable dirs).
    """
    source = (source_home or Path.home()).resolve()
    target = burst_home_path(runtime_root, burst_id)
    parent = target.parent
    try:
        parent.mkdir(parents=True, exist_ok=True)
        os.chmod(parent, 0o700)
    except OSError:
        pass
    # Reseed: previous bursts with the same id (unlikely but possible
    # under deterministic retry/replay paths) get a clean slate.
    # ignore_errors=True tolerates concurrent writes by an already-live
    # provider CLI in the legacy fake-home (rare but observed in tests
    # where bridge state isn't quiesced between runs).
    #
    # The persistent variant skips the wipe — codex stores absolute
    # rollout paths in its state DB and any burst that wipes the prior
    # burst's rollouts breaks the next burst's `codex exec resume`.
    if target.exists() and not persistent:
        shutil.rmtree(target, ignore_errors=True)
    target.mkdir(parents=True, exist_ok=True)
    try:
        os.chmod(target, 0o700)
    except OSError:
        pass
    for name in subdirs:
        src_subdir = source / name
        if not src_subdir.exists() or not src_subdir.is_dir():
            continue
        _hardlink_tree(src_subdir, target / name)
    # Root-level provider state files (e.g. ``~/.claude.json`` onboarding).
    # Always (re)linked, including the persistent path, so onboarding/auth
    # refreshes in the supervisor's home propagate to the next burst.
    for fname in root_files:
        _hardlink_file(source / fname, target / fname)
    return target


def cleanup_burst_home(burst_home: Path) -> None:
    """Delete the per-burst fake-home at burst exit.

    Best-effort: failures are swallowed so a cleanup hiccup never
    leaks into burst result accounting. Files hard-linked to the
    supervisor's real home have their dst-side link dropped here;
    the supervisor's copy stays intact (hard-link refcount stays
    >= 1 as long as the supervisor still holds the file).
    """
    try:
        path = burst_home.resolve()
    except OSError:
        path = burst_home
    if not str(path).strip():
        return
    # Defense in depth: refuse to recurse anything outside the
    # ``burst-homes/`` parent. A pathological caller can't trick this
    # into nuking the supervisor's real home.
    if "burst-homes" not in path.parts:
        return
    try:
        shutil.rmtree(path, ignore_errors=True)
    except OSError:
        pass


def cleanup_stale_burst_homes(
    runtime_root: Path,
    *,
    keep: Iterable[str] = (),
    older_than_seconds: int = 86400,
) -> int:
    """Sweep stale per-burst homes under ``<runtime>/burst-homes/``.

    Returns the count removed. ``keep`` is a set of burst_ids whose
    homes are still live (caller's responsibility to enumerate);
    anything older than ``older_than_seconds`` and not in ``keep`` is
    removed. Defensive sweep: the per-burst cleanup hook should already
    have run, but a crash mid-burst could leak directories.
    """
    import time

    root = burst_homes_root(runtime_root)
    if not root.is_dir():
        return 0
    now = time.time()
    keep_set = {str(k) for k in keep}
    removed = 0
    for child in root.iterdir():
        if child.name in keep_set:
            continue
        try:
            stat = child.stat()
        except OSError:
            continue
        if (now - stat.st_mtime) < older_than_seconds:
            continue
        cleanup_burst_home(child)
        removed += 1
    return removed
