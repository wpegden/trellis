"""Atomic support-file writing for tablet repos.

These helpers only write exact file bodies supplied by the kernel. They do not
normalize snapshots, group nodes, infer defaults, or decide when support files
should exist.
"""

from __future__ import annotations

import os
import tempfile
from pathlib import Path
from typing import Any, Dict, List


def _tablet_dir(repo_path: Path) -> Path:
    return repo_path / "Tablet"


def _index_md_path(repo_path: Path) -> Path:
    return _tablet_dir(repo_path) / "INDEX.md"


def _readme_md_path(repo_path: Path) -> Path:
    return _tablet_dir(repo_path) / "README.md"


def _header_tex_path(repo_path: Path) -> Path:
    return _tablet_dir(repo_path) / "header.tex"


def _node_lean_path(repo_path: Path, name: str) -> Path:
    return _tablet_dir(repo_path) / f"{name}.lean"


def _node_tex_path(repo_path: Path, name: str) -> Path:
    return _tablet_dir(repo_path) / f"{name}.tex"


def _normalize_render_output(render: Dict[str, Any]) -> Dict[str, Any]:
    def _required_path(name: str) -> Path:
        raw = render.get(name)
        if not isinstance(raw, str) or not raw.strip():
            raise ValueError(f"render output is missing {name}")
        return Path(raw).resolve()

    def _required_text(name: str) -> str:
        raw = render.get(name)
        if not isinstance(raw, str):
            raise ValueError(f"render output is missing {name}")
        return raw

    raw_header_content = render.get("header_tex_content")
    if raw_header_content is not None and not isinstance(raw_header_content, str):
        raise ValueError("render output has invalid header_tex_content")

    return {
        "index_md_path": _required_path("index_md_path"),
        "index_md_content": _required_text("index_md_content"),
        "readme_md_path": _required_path("readme_md_path"),
        "readme_md_content": _required_text("readme_md_content"),
        "header_tex_path": _required_path("header_tex_path"),
        "header_tex_content": raw_header_content,
    }


def _safe_write(path: Path, content: str) -> None:
    # Always write via tempfile + os.replace. The atomic rename is the only
    # primitive that's correct under concurrent writers: each writer creates
    # its own tempfile (owned by the writer, so the in-tmp chmod always
    # succeeds) and the final `os.replace` swaps the inode in one syscall.
    # Last-writer-wins, no in-place modification of a possibly-other-user-owned
    # file, no chmod EPERM on the destination.
    #
    # Earlier "write_text then chmod, fall back on PermissionError" path was
    # racy: when the kernel's worker-side observe_nodes_parallel pool (6
    # threads) fired concurrent `sync-tablet-support` subprocesses, one
    # thread's `unlink + os.replace` swapped the inode between another
    # thread's `chmod EPERM` and its `unlink`, producing
    # `FileNotFoundError` that escaped sync_tablet_support and broke
    # acceptance with "sync-tablet-support returned invalid JSON".
    fd, tmp = tempfile.mkstemp(dir=str(path.parent), prefix=f".{path.name}.")
    try:
        os.write(fd, content.encode("utf-8"))
        os.close(fd)
        os.chmod(tmp, 0o664)
        os.replace(tmp, str(path))
    except Exception:
        try:
            os.close(fd)
        except OSError:
            pass
        try:
            os.unlink(tmp)
        except FileNotFoundError:
            pass
        raise


def sync_tablet_support(repo_path: Path, render: Dict[str, Any]) -> Dict[str, Any]:
    tablet_dir = _tablet_dir(repo_path)
    tablet_dir.mkdir(parents=True, exist_ok=True)
    normalized = _normalize_render_output(render)
    updated_paths: List[str] = []

    index_path = Path(normalized["index_md_path"])
    _safe_write(index_path, str(normalized["index_md_content"]))
    updated_paths.append(str(index_path))

    readme_path = Path(normalized["readme_md_path"])
    _safe_write(readme_path, str(normalized["readme_md_content"]))
    updated_paths.append(str(readme_path))

    header_path = Path(normalized["header_tex_path"])
    header_content = normalized["header_tex_content"]
    if isinstance(header_content, str):
        _safe_write(header_path, header_content)
        updated_paths.append(str(header_path))

    return {
        "updated_paths": updated_paths,
        "header_tex_path": str(header_path),
        "index_md_path": str(index_path),
        "readme_md_path": str(readme_path),
    }


__all__ = ["sync_tablet_support"]
