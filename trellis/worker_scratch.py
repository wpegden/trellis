"""Worker-facing scratch workspace helpers."""

from __future__ import annotations

import json
import os
import re
import shutil
import stat
from pathlib import Path, PurePosixPath

from trellis.project_paths import project_scratch_dir, project_state_dir_for_repo


WORKER_SCRATCH_README_FILENAME = "README.md"
WORKER_SCRATCH_NOTES_FILENAME = "notes.md"
# Lean default; isabelle runs use `example.thy` (see
# `worker_scratch_example_filename`). Kept as a module constant for the Lean
# path so existing imports stay valid.
WORKER_SCRATCH_EXAMPLE_FILENAME = "example.lean"
WORKER_SCRATCH_EXAMPLE_FILENAME_ISABELLE = "example.thy"
_MAX_HANDOFF_FILE_BYTES = 8 * 1024 * 1024
_MAX_HANDOFF_TOTAL_BYTES = 32 * 1024 * 1024


def _repo_targets_isabelle(repo_path: Path) -> bool:
    """True when `<repo>/trellis.config.json` selects the Isabelle backend.

    Mirrors `sandbox._tablet_target_for_repo` / the bridge / the kernel.
    Defaults to Lean (False) when the config is absent/unreadable, so a Lean
    repo keeps the `example.lean` scaffold byte-for-byte.
    """
    config_file = repo_path.resolve() / "trellis.config.json"
    try:
        raw = json.loads(config_file.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return False
    workflow = raw.get("workflow") if isinstance(raw, dict) else None
    if isinstance(workflow, dict):
        value = workflow.get("default_target")
        if isinstance(value, str):
            return value.strip() == "isabelle_hol"
    return False


def worker_scratch_example_filename(repo_path: Path) -> str:
    return (
        WORKER_SCRATCH_EXAMPLE_FILENAME_ISABELLE
        if _repo_targets_isabelle(repo_path)
        else WORKER_SCRATCH_EXAMPLE_FILENAME
    )


def worker_scratch_dir(repo_path: Path) -> Path:
    return project_scratch_dir(project_state_dir_for_repo(repo_path))


def worker_scratch_readme_path(repo_path: Path) -> Path:
    return worker_scratch_dir(repo_path) / WORKER_SCRATCH_README_FILENAME


def worker_scratch_notes_path(repo_path: Path) -> Path:
    return worker_scratch_dir(repo_path) / WORKER_SCRATCH_NOTES_FILENAME


def worker_scratch_example_path(repo_path: Path) -> Path:
    return worker_scratch_dir(repo_path) / worker_scratch_example_filename(repo_path)


def _scratch_readme_text(*, isabelle: bool = False) -> str:
    if isabelle:
        return (
            "# Worker Scratch Workspace\n\n"
            "Use this directory for repo-local Isabelle experiments and temporary notes.\n"
            "It is not canonical tablet state and may be reset when the worker request uses fresh context.\n\n"
            "To check a scratch theory here, run:\n\n"
            "`isabelle build -D .trellis/scratch` (or open `example.thy` in an isabelle session)\n"
        )
    return (
        "# Worker Scratch Workspace\n\n"
        "Use this directory for repo-local Lean experiments and temporary notes.\n"
        "It is not canonical tablet state and may be reset when the worker request uses fresh context.\n\n"
        "To compile a scratch Lean file here, run:\n\n"
        "`.trellis/scripts/check-job scratch .trellis/scratch/example.lean`\n\n"
        "The baseline example uses only Lean's core library. Put any additional imports before declarations.\n"
        "The managed checker builds imported Tablet modules first. If the central service is disabled, "
        "build those imports with `.trellis/scripts/check-job build Tablet.NodeName` before checking the scratch file. "
        "During Modeling, the extracted "
        "Preamble may still need the support definitions recorded in `modeling-input/manifest.json`.\n"
    )


def _scratch_notes_text() -> str:
    return (
        "# Worker Notes\n\n"
        "Use this file or create your own files in this directory for temporary notes.\n"
        "This workspace is non-canonical and may be reset on fresh-context worker requests.\n"
    )


def _scratch_example_text(*, isabelle: bool = False) -> str:
    if isabelle:
        return (
            "theory Scratch\n"
            "  imports Main\n"
            "begin\n\n"
            "(* Example scratch theory: project-local, ignored by the main repo. *)\n"
            "lemma \"Suc 0 = 1\" by simp\n\n"
            "end\n"
        )
    return (
        "import Init\n\n"
        "-- Example scratch file: project-local, ignored by the main repo, and buildable with\n"
        "--   .trellis/scripts/check-job scratch .trellis/scratch/example.lean\n"
        "#check Nat.succ\n"
        "example : Nat.succ 0 = 1 := rfl\n"
    )


def reviewer_scratch_paths(comments: str, *, include_audit: bool = False) -> list[str]:
    """Recognize repo-relative scratch references in the explicit handoff text."""
    paths = set()
    namespace = r"(?:scratch|stuck-math-audit)" if include_audit else "scratch"
    for match in re.finditer(r"(?<![\w./-])\.trellis/" + namespace + r"/[^\s`'\"<>()\[\]{}]+", comments):
        relative = re.sub(r":\d+(?::\d+)?$", "", match.group().rstrip(".,;:"))
        path = PurePosixPath(relative)
        # A trailing slash in prose denotes a directory, not a malformed
        # file handoff. Keep it for capture to report as unavailable; all
        # other noncanonical spellings and traversal remain rejected.
        if (str(path) != relative.removesuffix('/') or '..' in path.parts or len(path.parts) < 3):
            raise ValueError(f"Reviewer note must name a file within its advisory directory: {relative}")
        paths.add(relative)
    return sorted(paths)


def capture_reviewer_scratch(repo_path: Path, comments: str, *,
                             include_audit: bool = False) -> tuple[dict[str, bytes], list[str]]:
    """Read advisory files without following links, including renamed parents.

    The caller has reaped the producing reviewer. These bytes are instructions,
    never accepted source or proof evidence. Missing notes are reported to the
    worker; they must not be substituted from a different review's workspace.
    """
    captured, missing = {}, []
    remaining = _MAX_HANDOFF_TOTAL_BYTES
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    for relative in reviewer_scratch_paths(comments, include_audit=include_audit):
        fd = None
        try:
            fd = os.open(repo_path, flags)
            parts = PurePosixPath(relative).parts
            for component in parts[:-1]:
                child = os.open(component, flags, dir_fd=fd)
                os.close(fd)
                fd = child
            opened = os.open(parts[-1], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                             dir_fd=fd)
            if stat.S_ISDIR(os.fstat(opened).st_mode):
                os.close(opened)
                missing.append(f"{relative} (directory reference; name an exact advisory file)")
                continue
            with os.fdopen(opened, 'rb') as stream:
                info = os.fstat(stream.fileno())
                if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                    raise ValueError(f"Reviewer note must be a regular file with no hardlinks: {relative}")
                if relative.endswith('/'):
                    missing.append(f"{relative} (directory reference; name an exact advisory file)")
                    continue
                limit = min(_MAX_HANDOFF_FILE_BYTES, remaining)
                data = stream.read(limit + 1) if info.st_size <= limit else None
                if data is None or len(data) > limit:
                    missing.append(f"{relative} (exceeds handoff limit of {limit} bytes)")
                    continue
                captured[relative] = data
                remaining -= len(data)
        except FileNotFoundError:
            missing.append(relative)
        except OSError as exc:
            raise ValueError(f"Cannot read reviewer note: {relative}: {exc}") from exc
        finally:
            if fd is not None:
                os.close(fd)
    return captured, missing


def ensure_worker_scratch_workspace(repo_path: Path, *, reset: bool = False,
                                    reviewer_comments: str = '') -> dict[str, Path | str]:
    handoffs, missing = capture_reviewer_scratch(repo_path, reviewer_comments)
    scratch_dir = worker_scratch_dir(repo_path)
    existed = scratch_dir.exists()
    if reset and scratch_dir.exists():
        shutil.rmtree(scratch_dir)
        existed = False
    scratch_dir.mkdir(parents=True, exist_ok=True)

    isabelle = _repo_targets_isabelle(repo_path)
    scaffold = {
        worker_scratch_readme_path(repo_path): _scratch_readme_text(isabelle=isabelle),
        worker_scratch_notes_path(repo_path): _scratch_notes_text(),
        worker_scratch_example_path(repo_path): _scratch_example_text(isabelle=isabelle),
    }
    for path, content in scaffold.items():
        if reset or not path.exists():
            path.write_text(content, encoding="utf-8")

    if reset:
        for relative, content in handoffs.items():
            path = scratch_dir.joinpath(*PurePosixPath(relative).parts[2:])
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)

    if reset:
        status = "reset to the baseline scratch workspace scaffold because the worker context is fresh"
    elif existed:
        status = "using the existing scratch workspace"
    else:
        status = "created the baseline scratch workspace scaffold for this worker request"
    if reset and handoffs:
        status += "; retained the files referenced in the reviewer comments"

    return {
        "workspace_path": scratch_dir,
        "readme_path": worker_scratch_readme_path(repo_path),
        "notes_path": worker_scratch_notes_path(repo_path),
        "example_path": worker_scratch_example_path(repo_path),
        "status_text": status,
        "handoff_status_text": "Reviewer notes unavailable: " + ", ".join(missing) if missing else "",
    }
