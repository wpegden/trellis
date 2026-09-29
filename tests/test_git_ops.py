from __future__ import annotations

import subprocess
from pathlib import Path

from trellis.git_ops import init_repo


def test_init_repo_gitignores_stop_after_checkpoint_sentinel(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    init_repo(repo)
    sentinel = repo / ".trellis-stop-after-checkpoint"
    sentinel.touch()

    ignored = subprocess.run(
        ["git", "check-ignore", "--quiet", sentinel.name],
        cwd=repo,
        check=False,
    )

    assert ignored.returncode == 0
