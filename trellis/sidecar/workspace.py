"""Sidecar workspace manager (E§1.3 + amendment A8 + queue redesign §2.2).

One persistent checkout of the tablet repo PER GRUNT under
``<runtime>/sidecar/grunts/<k>/repo``, each with its OWN ``.lake``
(a bad-behaving grunt can only trash its own tree; reset rebuilds it).
Bootstrap runs per grunt — packages hardlinked (cheap), Tablet oleans
COPIED per the two-writer argument (~370 MB × N, N=2 default):

* **Bootstrap** (one-time): local ``git clone`` of the live repo
  (object store shared/hardlinked — objects are content-addressed and
  never rewritten); ``.lake/packages`` hardlink-seeded via the existing
  ``_ensure_supervisor_lake_packages`` (imported, not copied); Tablet
  oleans + ``.olean.srcclosure`` sidecars COPIED — never hardlinked —
  because the sidecar is a second independent writer of those files
  and an in-place rebuild through a hardlink would corrupt the live
  copy (E§1.3's two-writer argument).
* **Per-attempt refresh** (amendment A8): FORCED fetch of master only
  — NO tag fetching at all (the ``supervisor2/checkpoint-*`` namespace
  wraps after LastClean rewinds and a non-forced tag fetch wedges
  forever) — then ``reset --hard <exported snapshot_sha>`` iff that
  SHA is present locally after the fetch; otherwise IDLE until the
  next export (mid-interval rewind / abandoned line).
* **Stale-olean correctness**: after every reset, the content-hash
  srcclosure gate (``olean_is_content_current`` /
  ``_purge_olean_artifacts`` from ``trellis/atomic_actions/
  observations.py`` — imported, never re-derived; mtime never
  consulted) purges any stale artifact in the target node's transitive
  Tablet closure so lake rebuilds content-correctly.
* **Wipe-safety**: every ``lake``/``lean`` invocation is built for the
  ``lake_compiler`` bwrap role (packages ro-bound → EROFS on any
  re-clone attempt) at lowest priority (``nice -n 19`` + ``ionice
  -c3`` + ``LEAN_NUM_THREADS=<cfg>``).

The workspace is a CACHE: fetch+reset rebuilds it from the live repo;
losing it costs bootstrap time only.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Dict, List, Optional, Sequence, Set, Tuple

from trellis.atomic_actions.observations import (
    _kernel_recursive_imports,
    _purge_olean_artifacts,
    olean_is_content_current,
    read_olean_srcclosure,
    tablet_source_closure_hash,
    write_olean_srcclosure,
)
from trellis.supervisor_workspace import _ensure_supervisor_lake_packages


TABLET_BUILD_REL = Path(".lake/build/lib/lean/Tablet")


def workspace_repo_path(runtime_root: Path, grunt: int = 0) -> Path:
    """Grunt `k`'s isolated checkout (queue redesign §2.2)."""
    return Path(runtime_root) / "sidecar" / "grunts" / str(int(grunt)) / "repo"


def _git(repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    proc = subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
    )
    if check and proc.returncode != 0:
        raise RuntimeError(
            f"git {' '.join(args)} failed in {repo}: {proc.stderr.strip()}"
        )
    return proc


@dataclass(frozen=True)
class BootstrapResult:
    cloned: bool
    packages: Dict[str, int]
    tablet_artifacts_copied: int
    srcclosure_seeded: int = 0


def bootstrap_workspace(
    live_repo: Path, runtime_root: Path, grunt: int = 0
) -> BootstrapResult:
    """One-time PER-GRUNT workspace creation. Idempotent: an existing
    clone is left in place (refresh handles updates)."""
    repo = workspace_repo_path(runtime_root, grunt)
    cloned = False
    if not (repo / ".git").exists():
        repo.parent.mkdir(parents=True, exist_ok=True)
        proc = subprocess.run(
            ["git", "clone", "--no-tags", str(live_repo), str(repo)],
            capture_output=True,
            text=True,
        )
        if proc.returncode != 0:
            raise RuntimeError(f"sidecar bootstrap clone failed: {proc.stderr.strip()}")
        cloned = True
    # Hardlink-seed .lake/packages from the live repo (idempotent;
    # prebuilt package oleans REQUIRED under the ro-bind — the S2
    # incident class).
    packages = _ensure_supervisor_lake_packages(Path(live_repo), repo)
    copy_freshness_check_script(Path(live_repo), repo)
    copied = copy_tablet_build_artifacts(Path(live_repo), repo)
    seeded = seed_missing_srcclosure_sidecars(repo)
    return BootstrapResult(
        cloned=cloned,
        packages=packages,
        tablet_artifacts_copied=copied,
        srcclosure_seeded=seeded,
    )


def copy_freshness_check_script(live_repo: Path, sidecar_repo: Path) -> bool:
    """Copy ``.trellis/scripts/check.py`` from the live repo into the
    grunt workspace.

    ``.trellis/`` is gitignored, so the bootstrap ``git clone`` never
    brings it across — but ``tablet_source_closure_hash`` (the content-
    hash freshness gate) hashes this script as part of every node's
    source closure and returns ``None`` (fail-closed => "not current")
    when it is unreadable. Without this copy, EVERY olean in the
    workspace reads as stale and ``purge_stale_oleans_for`` rebuilds
    each target's whole transitive closure on the first attempt,
    defeating the copied-olean warm-replay design (grunt-bench measured:
    51/51 purged, 203 s server open; after the fix 1/51, 5.9 s).
    Best-effort: a missing script leaves the fail-closed behavior
    intact."""
    src = Path(live_repo) / ".trellis" / "scripts" / "check.py"
    if not src.is_file():
        return False
    dst = Path(sidecar_repo) / ".trellis" / "scripts" / "check.py"
    try:
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, dst)
        return True
    except OSError:
        return False


def seed_missing_srcclosure_sidecars(sidecar_repo: Path) -> int:
    """Stamp a ``.olean.srcclosure`` sidecar for every Tablet node whose
    olean was copied but carries no sidecar, using the CURRENT source-
    closure hash.

    Without this, ``copy_tablet_build_artifacts``'s stated purpose (a
    first build that is a content-hash replay, not a full-tablet
    rebuild) silently fails whenever the live repo's ``.lake`` lacks
    sidecars — a completed run does not necessarily persist them, and
    ``olean_is_content_current`` fails closed on a missing sidecar, so
    the freshness gate then reads EVERY copied olean as stale and
    ``purge_stale_oleans_for`` nukes each target's whole transitive
    closure on the first attempt.

    Correctness: the workspace was just cloned at the live HEAD and the
    oleans were copied from that same HEAD's ``.lake``, so the olean's
    build-time source closure IS the current source closure — stamping
    ``tablet_source_closure_hash`` is exact, not an approximation.
    Nodes that already have a sidecar are left untouched. Both the
    sidecars and the copied check script survive ``refresh``'s ``git
    clean`` (ignored paths are skipped).
    """
    repo = Path(sidecar_repo)
    tablet_dir = repo / "Tablet"
    if not tablet_dir.is_dir():
        return 0
    build_dir = repo / TABLET_BUILD_REL
    seeded = 0
    for lean_file in tablet_dir.glob("*.lean"):
        node = lean_file.stem
        olean = build_dir / f"{node}.olean"
        try:
            if not olean.exists() or olean.stat().st_size == 0:
                continue
        except OSError:
            continue
        if read_olean_srcclosure(repo, node) is not None:
            continue
        closure_hash = tablet_source_closure_hash(repo, node)
        if closure_hash is None:
            continue
        write_olean_srcclosure(repo, node, closure_hash)
        seeded += 1
    return seeded


def copy_tablet_build_artifacts(live_repo: Path, sidecar_repo: Path) -> int:
    """COPY (never hardlink) the live repo's Tablet build dir — oleans,
    hash/trace files, and ``.olean.srcclosure`` sidecars — so the first
    build is a content-hash replay instead of a full-tablet rebuild.
    Copies are the two-writer safety requirement: the sidecar
    legitimately rebuilds these files. Idempotent by size compare."""
    src = Path(live_repo) / TABLET_BUILD_REL
    if not src.is_dir():
        return 0
    dst = Path(sidecar_repo) / TABLET_BUILD_REL
    dst.mkdir(parents=True, exist_ok=True)
    copied = 0
    for entry in src.iterdir():
        if not entry.is_file():
            continue
        target = dst / entry.name
        try:
            if target.exists() and target.stat().st_size == entry.stat().st_size:
                continue
            shutil.copy2(entry, target)
            copied += 1
        except OSError:
            continue
    return copied


@dataclass(frozen=True)
class RefreshResult:
    ok: bool
    reason: str
    head: str = ""


def refresh_to_snapshot(
    runtime_root: Path, snapshot_sha: str, grunt: int = 0
) -> RefreshResult:
    """Amendment A8 refresh (per grunt): forced master fetch, NO tags,
    reset iff the exported SHA is locally present after the fetch; else
    idle. Doubles as the cancel/rewind rollback: reset --hard + clean
    leave the grunt's tree byte-equal to the snapshot."""
    repo = workspace_repo_path(runtime_root, grunt)
    if not (repo / ".git").exists():
        return RefreshResult(ok=False, reason="workspace missing (bootstrap first)")
    if not snapshot_sha:
        return RefreshResult(ok=False, reason="empty snapshot_sha")
    # Forced, tag-free: history rewrites (LastClean rewinds) move master
    # non-fast-forward; the checkpoint-tag namespace wraps and is never
    # fetched at all.
    fetch = _git(
        repo,
        "fetch",
        "--no-tags",
        "--force",
        "origin",
        "+master:refs/remotes/origin/master",
        check=False,
    )
    if fetch.returncode != 0:
        return RefreshResult(
            ok=False, reason=f"fetch failed: {fetch.stderr.strip()[:200]}"
        )
    present = _git(repo, "cat-file", "-e", f"{snapshot_sha}^{{commit}}", check=False)
    if present.returncode != 0:
        return RefreshResult(
            ok=False,
            reason=(
                "snapshot sha unreachable after fetch (mid-interval rewind / "
                "abandoned line); idling until the next export"
            ),
        )
    _git(repo, "reset", "--hard", snapshot_sha)
    _git(
        repo,
        "clean",
        "-fd",
        "-e",
        ".lake",
        "-e",
        ".trellis-sidecar",
        check=False,
    )
    head = _git(repo, "rev-parse", "HEAD").stdout.strip()
    return RefreshResult(ok=True, reason="reset", head=head)


def purge_stale_oleans_for(repo: Path, node: str) -> List[str]:
    """Content-hash freshness pass over ``node``'s transitive Tablet
    closure (self included): purge artifacts whose srcclosure sidecar
    no longer matches the source content, so the next lake build
    recompiles exactly the stale set."""
    closure: Set[str] = set()
    _kernel_recursive_imports(Path(repo), node, closure)
    closure.add(node)
    purged: List[str] = []
    for name in sorted(closure):
        if not olean_is_content_current(Path(repo), name):
            _purge_olean_artifacts(Path(repo), name)
            purged.append(name)
    return purged


class SandboxUnavailableError(RuntimeError):
    """A sandbox role is configured but bwrap is not installed. The
    hardlinked-package-olean two-writer safety rests on the bwrap
    ro-bind, so this is fail-closed: NO silent bare fallback (F5)."""


def _sandbox_unavailable_message(sandbox_role: str) -> str:
    return (
        f"sidecar: sandbox role {sandbox_role!r} is configured but bwrap is "
        "not installed; the wipe-safety of the sidecar workspace (ro-bound "
        ".lake/packages) REQUIRES bwrap. Install bubblewrap, or — for an "
        "offline harness only, never a live run — set "
        '`sidecar.daemon.allow_unsandboxed: true`.'
    )


def ensure_sandbox_available(
    sandbox_role: str, allow_unsandboxed: bool = False
) -> None:
    """Startup gate (F5): with a sandbox role configured and bwrap
    absent, refuse — unless the explicit ``allow_unsandboxed`` config
    escape hatch is set, which logs loudly instead."""
    from trellis.sandbox import bwrap_available

    if not sandbox_role or bwrap_available():
        return
    if allow_unsandboxed:
        print(
            "sidecar: WARNING — bwrap unavailable and allow_unsandboxed=true; "
            "lake/lean commands will run UNSANDBOXED (no packages ro-bind). "
            "Never use this on a live run.",
            file=sys.stderr,
        )
        return
    raise SandboxUnavailableError(_sandbox_unavailable_message(sandbox_role))


def _wrapped_inner_command(
    repo: Path,
    inner: Sequence[str],
    *,
    lean_threads: int,
    sandbox_role: str,
    allow_unsandboxed: bool,
    burst_home: Optional[Path],
) -> List[str]:
    """The shared bwrap wrapping for lake commands AND the warm lean
    server (F2): role-wrapped with ``LEAN_NUM_THREADS`` threaded through
    bwrap's env. Empty role = explicitly unsandboxed (offline harness /
    test path); role set + bwrap missing = fail-closed refusal via
    ``ensure_sandbox_available``."""
    from trellis.config import SandboxConfig
    from trellis.sandbox import bwrap_available, wrap_command

    inner_cmd = list(inner)
    if not sandbox_role:
        return inner_cmd
    ensure_sandbox_available(sandbox_role, allow_unsandboxed)
    if not bwrap_available():
        return inner_cmd  # allow_unsandboxed path (already logged loudly)
    wrapped = wrap_command(
        inner_cmd,
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=Path(repo),
        burst_home=burst_home,
        role=sandbox_role,
    )
    # Thread the core cap through bwrap's env (the prewarm-server
    # precedent: --setenv right after argv[0]).
    return (
        [wrapped[0], "--setenv", "LEAN_NUM_THREADS", str(lean_threads)]
        + wrapped[1:]
    )


def build_lake_command(
    repo: Path,
    inner: Sequence[str],
    *,
    lean_threads: int,
    sandbox_role: str,
    allow_unsandboxed: bool = False,
    burst_home: Optional[Path] = None,
) -> List[str]:
    """Assemble the lowest-priority, wipe-safe command line for a lake/
    lean invocation in the sidecar workspace: ``nice -n 19 ionice -c3``
    around the (bwrap-wrapped) inner command. With a non-empty
    ``sandbox_role`` and no bwrap this REFUSES (F5 fail-closed) unless
    ``allow_unsandboxed`` is explicitly set; an empty role runs bare
    (offline harness / test path only)."""
    inner_cmd = _wrapped_inner_command(
        repo,
        inner,
        lean_threads=lean_threads,
        sandbox_role=sandbox_role,
        allow_unsandboxed=allow_unsandboxed,
        burst_home=burst_home,
    )
    return ["nice", "-n", "19", "ionice", "-c3", *inner_cmd]


def build_lean_server_command(
    repo: Path,
    *,
    lean_threads: int,
    sandbox_role: str,
    allow_unsandboxed: bool = False,
    burst_home: Optional[Path] = None,
) -> Tuple[List[str], Dict[str, str]]:
    """Command + launch env for the sidecar's warm ``lake env lean
    --server`` — the SAME resource discipline as ``build_lake_command``
    (F2: nice 19 / ionice idle / bwrap role / LEAN_NUM_THREADS cap),
    mirroring the prewarm server's ``_bwrap_lean_server_cmd`` recipe:
    ``ELAN_HOME`` made explicit inside the bwrap (HOME is rebound) and
    the resolved elan bin prepended to the outer PATH so bwrap's
    argv[0] resolves."""
    inner = ["lake", "env", "lean", "--server"]
    cmd = _wrapped_inner_command(
        repo,
        inner,
        lean_threads=lean_threads,
        sandbox_role=sandbox_role,
        allow_unsandboxed=allow_unsandboxed,
        burst_home=burst_home,
    )
    env = dict(os.environ)
    if cmd != inner:  # bwrap-wrapped: pin ELAN_HOME + outer PATH
        from trellis.host_runtime import worker_elan_home

        cmd = cmd[:1] + ["--setenv", "ELAN_HOME", str(worker_elan_home())] + cmd[1:]
        elan_bin = str(worker_elan_home() / "bin")
        existing_path = env.get("PATH", "")
        if elan_bin not in existing_path.split(os.pathsep):
            env["PATH"] = elan_bin + os.pathsep + existing_path
    else:
        # Bare path (empty role / allow_unsandboxed): cap the thread
        # pool through the launch env instead of bwrap --setenv.
        env["LEAN_NUM_THREADS"] = str(lean_threads)
    return ["nice", "-n", "19", "ionice", "-c3", *cmd], env
