"""Project-local runtime snapshot for sandboxed agent bursts."""

from __future__ import annotations

import os
import shlex
import shutil
import tempfile
from pathlib import Path

from trellis.project_paths import (
    project_runtime_bin_dir,
    project_runtime_dir,
    project_runtime_skills_dir,
    project_runtime_src_dir,
)


SOURCE_ROOT = Path(__file__).resolve().parent.parent
PACKAGE_SOURCE_DIR = SOURCE_ROOT / "trellis"
KERNEL_SOURCE_DIR = SOURCE_ROOT / "kernel"


def _resolve_skills_source_dir() -> Path:
    direct = SOURCE_ROOT / "skills"
    if direct.exists():
        return direct
    bundled = SOURCE_ROOT.parent / "skills"
    if bundled.exists():
        return bundled
    return direct


SKILLS_SOURCE_DIR = _resolve_skills_source_dir()
DOC_SOURCES = (
    SOURCE_ROOT / "FILESPEC.md",
)
SCRIPT_SOURCES = (
    SOURCE_ROOT / "scripts" / "lean_semantic_fingerprint.lean",
    SOURCE_ROOT / "scripts" / "loogle_json.sh",
)
IGNORED_SNAPSHOT_NAMES = {
    "__pycache__",
    "target",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
}


def _should_ignore_snapshot_name(name: str) -> bool:
    return (
        name in IGNORED_SNAPSHOT_NAMES
        or name.endswith(".pyc")
        or name.startswith(".tmp")
        or name.startswith(".#")
        or (name.startswith("#") and name.endswith("#"))
        or name.endswith("~")
        or name.endswith((".swp", ".swo", ".swx"))
    )


def _copytree_filtered(src: Path, dst: Path) -> None:
    def _ignore(_dir: str, names: list[str]) -> set[str]:
        ignored: set[str] = set()
        for name in names:
            if _should_ignore_snapshot_name(name):
                ignored.add(name)
        return ignored

    shutil.copytree(src, dst, ignore=_ignore, dirs_exist_ok=True)


def _replace_dir_atomic(src: Path, dst: Path) -> None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    tmp_root = Path(tempfile.mkdtemp(prefix=f"{dst.name}.tmp.", dir=str(dst.parent)))
    backup: Path | None = None
    try:
        staged = tmp_root / dst.name
        if src.is_dir():
            _copytree_filtered(src, staged)
        else:
            staged.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, staged)
        if dst.exists():
            backup = tmp_root / f"{dst.name}.old"
            dst.rename(backup)
        staged.rename(dst)
    finally:
        if backup is not None:
            shutil.rmtree(backup, ignore_errors=True)
        shutil.rmtree(tmp_root, ignore_errors=True)


def _normalize_permissions(root: Path) -> None:
    if not root.exists():
        return
    for current, dirs, files in os.walk(root):
        current_path = Path(current)
        current_path.chmod(0o755)
        for name in dirs:
            (current_path / name).chmod(0o755)
        for name in files:
            path = current_path / name
            mode = 0o755 if path.suffix == ".sh" or path.parent.name == "bin" else 0o644
            path.chmod(mode)


def _active_kernel_binary_path() -> Path | None:
    raw = os.environ.get("TRELLIS_TRELLIS_KERNEL_CMD", "").strip()
    if not raw:
        fallback = SOURCE_ROOT / "kernel" / "target" / "debug" / "trellis_runtime_cli"
        return fallback.resolve() if fallback.is_file() else None
    try:
        parts = shlex.split(raw)
    except ValueError:
        return None
    if not parts:
        return None
    kernel = Path(parts[0])
    if not kernel.is_absolute():
        resolved = shutil.which(parts[0])
        if not resolved:
            return None
        kernel = Path(resolved)
    if kernel.is_file():
        return kernel.resolve()
    fallback = SOURCE_ROOT / "kernel" / "target" / "debug" / "trellis_runtime_cli"
    return fallback.resolve() if fallback.is_file() else None


def materialize_project_runtime(repo_path: Path, state_dir: Path) -> None:
    """Refresh the project-local runtime snapshot used by sandboxed agents."""
    runtime_dir = project_runtime_dir(state_dir)
    runtime_bin_dir = project_runtime_bin_dir(state_dir)
    runtime_src_dir = project_runtime_src_dir(state_dir)
    runtime_skills_dir = project_runtime_skills_dir(state_dir)

    runtime_dir.mkdir(parents=True, exist_ok=True)

    _replace_dir_atomic(PACKAGE_SOURCE_DIR, runtime_src_dir / "trellis")
    _replace_dir_atomic(KERNEL_SOURCE_DIR, runtime_src_dir / "kernel")
    for doc_src in DOC_SOURCES:
        if not doc_src.exists():
            raise FileNotFoundError(
                f"runtime snapshot DOC_SOURCES entry missing: {doc_src}. "
                "A declared documentation source did not materialize — this "
                "would silently ship a runtime tree missing operator docs. "
                "Fix the source path or the curation that dropped it."
            )
        doc_dst = runtime_src_dir / doc_src.name
        doc_dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(doc_src, doc_dst)
        repo_dst = repo_path / doc_src.name
        if not repo_dst.exists():
            repo_dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(doc_src, repo_dst)
    for script_src in SCRIPT_SOURCES:
        if not script_src.exists():
            raise FileNotFoundError(
                f"runtime snapshot SCRIPT_SOURCES entry missing: {script_src}. "
                "A declared runtime script did not materialize — this would "
                "silently ship a runtime tree missing a script the checker / "
                "observations layer needs. Fix the source path or the "
                "curation that dropped it."
            )
        script_dst = runtime_src_dir / "scripts" / script_src.name
        script_dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(script_src, script_dst)
    runtime_bin_dir.mkdir(parents=True, exist_ok=True)
    kernel_binary_dst = runtime_bin_dir / "trellis_runtime_cli"
    active_kernel = _active_kernel_binary_path()
    if active_kernel is not None:
        # Hard-link instead of copy when source and destination are
        # on the same filesystem: every materialize call previously
        # wrote a 76 MB copy of the kernel binary to disk (production
        # supervisor restarts and especially every pytest run that
        # materialized a fixture tree). The hard-link costs only a
        # directory entry — 0 bytes of payload writes — and is
        # transparent to downstream consumers (the supervisor reads
        # `runtime/bin/trellis_runtime_cli` like any regular file).
        # Cargo's atomic-rename rebuild flow is not perturbed: it
        # creates a new inode for the source, leaving our hard-link
        # pointing at the previous inode until the next
        # materialize call (which unlinks and re-links to the new
        # source inode). Cross-filesystem destinations fall back to
        # `shutil.copy2`.
        kernel_binary_dst.unlink(missing_ok=True)
        try:
            os.link(active_kernel, kernel_binary_dst)
        except OSError:
            shutil.copy2(active_kernel, kernel_binary_dst)
    else:
        kernel_binary_dst.unlink(missing_ok=True)

    _replace_dir_atomic(SKILLS_SOURCE_DIR, runtime_skills_dir)

    _normalize_permissions(runtime_bin_dir)
    _normalize_permissions(runtime_src_dir)
    _normalize_permissions(runtime_skills_dir)
