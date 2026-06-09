from __future__ import annotations

from pathlib import Path

from trellis.worker_scratch import (
    ensure_worker_scratch_workspace,
    worker_scratch_dir,
    worker_scratch_example_path,
    worker_scratch_notes_path,
    worker_scratch_readme_path,
)


def test_ensure_worker_scratch_workspace_creates_scaffold(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    scratch = ensure_worker_scratch_workspace(repo, reset=True)

    assert scratch["workspace_path"] == worker_scratch_dir(repo)
    assert worker_scratch_readme_path(repo).is_file()
    assert worker_scratch_notes_path(repo).is_file()
    assert worker_scratch_example_path(repo).is_file()
    assert "fresh" in str(scratch["status_text"])


def test_ensure_worker_scratch_workspace_reset_clears_extra_files(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    ensure_worker_scratch_workspace(repo, reset=True)
    worker_scratch_notes_path(repo).write_text("carry over\n", encoding="utf-8")
    extra = worker_scratch_dir(repo) / "scratch-proof.lean"
    extra.write_text("example : True := trivial\n", encoding="utf-8")

    ensure_worker_scratch_workspace(repo, reset=False)
    assert worker_scratch_notes_path(repo).read_text(encoding="utf-8") == "carry over\n"
    assert extra.exists()

    ensure_worker_scratch_workspace(repo, reset=True)
    assert "carry over" not in worker_scratch_notes_path(repo).read_text(encoding="utf-8")
    assert not extra.exists()
