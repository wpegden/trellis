from __future__ import annotations

import json
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def test_init_materializes_project_runtime_scripts(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    config_path = repo / "trellis.config.json"
    runtime_root = tmp_path / "runtime"

    config_path.write_text(
        json.dumps(
            {
                "repo_path": str(repo),
                "state_dir": ".trellis",
                "policy_path": "trellis.policy.json",
                "worker": {"provider": "codex", "model": "worker-a"},
                "reviewer": {"provider": "codex", "model": "reviewer-a"},
                "verification": {},
                "tmux": {"burst_user": "sandbox-user"},
                "workflow": {"start_phase": "theorem_stating"},
            }
        ),
        encoding="utf-8",
    )

    subprocess.run(
        [
            "bash",
            str(ROOT / "scripts" / "trellis.sh"),
            "init",
            str(config_path),
            str(runtime_root),
        ],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )

    assert (repo / ".trellis" / "scripts" / "check.py").is_file()
    assert (repo / ".trellis" / "runtime" / "src" / "trellis").is_dir()
