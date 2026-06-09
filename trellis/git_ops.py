"""Minimal git bootstrap helpers for trellis project setup.

This module intentionally keeps only the repo-initialization surface that the
new setup path needs. Runtime checkpointing is handled by the dedicated
trellis checkpoint hook and kernel runtime, not by a large Python git layer.
"""

from __future__ import annotations

import subprocess
from pathlib import Path


GITIGNORE_CONTENT = """\
# Build artifacts
.lake/

# Trellis runtime artifacts
.trellis/

# Editor / OS
.DS_Store
*.swp
*~
"""


def _git(repo: Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", *args],
        cwd=str(repo),
        capture_output=True,
        text=True,
        check=True,
        timeout=30,
    )


def _ensure_gitignore(repo: Path) -> None:
    gitignore = repo / ".gitignore"
    current = gitignore.read_text(encoding="utf-8") if gitignore.exists() else ""
    if current != GITIGNORE_CONTENT:
        gitignore.write_text(GITIGNORE_CONTENT, encoding="utf-8")


def init_repo(
    repo: Path,
    *,
    author_name: str = ".trellis",
    author_email: str = "trellis@localhost",
) -> None:
    """Initialize a git repo if missing and ensure the trellis ignore policy."""

    if not (repo / ".git").exists():
        _git(repo, "init")
    _ensure_gitignore(repo)
    _git(repo, "config", "user.name", author_name)
    _git(repo, "config", "user.email", author_email)
