"""Worker-facing scratch workspace helpers."""

from __future__ import annotations

import json
import shutil
from pathlib import Path

from trellis.project_paths import project_scratch_dir, project_state_dir_for_repo


WORKER_SCRATCH_README_FILENAME = "README.md"
WORKER_SCRATCH_NOTES_FILENAME = "notes.md"
# Lean default; isabelle runs use `example.thy` (see
# `worker_scratch_example_filename`). Kept as a module constant for the Lean
# path so existing imports stay valid.
WORKER_SCRATCH_EXAMPLE_FILENAME = "example.lean"
WORKER_SCRATCH_EXAMPLE_FILENAME_ISABELLE = "example.thy"


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
        "`lake env lean .trellis/scratch/example.lean`\n"
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
        "import Tablet.Preamble\n"
        "import Mathlib.Data.Nat.Basic\n\n"
        "-- Example scratch file: project-local, ignored by the main repo, and buildable with\n"
        "--   lake env lean .trellis/scratch/example.lean\n"
        "#check Nat.succ\n"
        "example : Nat.succ 0 = 1 := rfl\n"
    )


def ensure_worker_scratch_workspace(repo_path: Path, *, reset: bool = False) -> dict[str, Path | str]:
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
        status = "reset to the baseline scratch workspace scaffold because the worker context is fresh"
    elif existed:
        status = "carried over from the previous worker context"
    else:
        status = "created the baseline scratch workspace scaffold for this worker request"

    return {
        "workspace_path": scratch_dir,
        "readme_path": worker_scratch_readme_path(repo_path),
        "notes_path": worker_scratch_notes_path(repo_path),
        "example_path": worker_scratch_example_path(repo_path),
        "status_text": status,
    }
