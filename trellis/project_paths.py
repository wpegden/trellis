"""Canonical project-local paths for mutable trellis state."""

from __future__ import annotations

import hashlib
import tempfile
from pathlib import Path


PROJECT_CONFIG_FILENAME = "trellis.config.json"
PROJECT_POLICY_FILENAME = "trellis.policy.json"
PROJECT_STATE_DIRNAME = ".trellis"
PROJECT_HISTORY_DIRNAME = ".trellis-history"
PROJECT_TMP_DIRNAME = "tmp"


def project_config_path(repo_path: Path) -> Path:
    return repo_path / PROJECT_CONFIG_FILENAME


def project_policy_path(repo_path: Path) -> Path:
    return repo_path / PROJECT_POLICY_FILENAME


def project_state_dir_for_repo(repo_path: Path) -> Path:
    return repo_path / PROJECT_STATE_DIRNAME


def project_history_dir(repo_path: Path) -> Path:
    return repo_path / PROJECT_HISTORY_DIRNAME


def project_chats_dir(state_dir: Path) -> Path:
    return state_dir / "chats"


def project_scratch_dir(state_dir: Path) -> Path:
    return state_dir / "scratch"


def project_scratch_subdir(state_dir: Path, *parts: str) -> Path:
    path = project_scratch_dir(state_dir).joinpath(*parts)
    path.mkdir(parents=True, exist_ok=True)
    return path


def project_tmp_dir(state_dir: Path) -> Path:
    return state_dir / PROJECT_TMP_DIRNAME


def project_tmp_subdir(state_dir: Path, *parts: str) -> Path:
    path = project_tmp_dir(state_dir).joinpath(*parts)
    path.mkdir(parents=True, exist_ok=True)
    return path


def project_temp_dir(state_dir: Path, *, purpose: str, prefix: str) -> Path:
    root = project_tmp_subdir(state_dir, purpose)
    return Path(tempfile.mkdtemp(prefix=prefix, dir=str(root)))


def repo_scratch_subdir(repo_path: Path, *parts: str) -> Path:
    return project_scratch_subdir(project_state_dir_for_repo(repo_path), *parts)


def repo_tmp_subdir(repo_path: Path, *parts: str) -> Path:
    return project_tmp_subdir(project_state_dir_for_repo(repo_path), *parts)


def repo_temp_dir(repo_path: Path, *, purpose: str, prefix: str) -> Path:
    return project_temp_dir(project_state_dir_for_repo(repo_path), purpose=purpose, prefix=prefix)


def project_feedback_log_path(state_dir: Path) -> Path:
    repo_path = state_dir.parent.resolve()
    digest = hashlib.sha256(str(repo_path).encode("utf-8")).hexdigest()[:12]
    root = Path.home() / ".trellis-feedback"
    root.mkdir(parents=True, exist_ok=True)
    return root / f"{repo_path.name}-{digest}.jsonl"


def project_runtime_dir(state_dir: Path) -> Path:
    return state_dir / "runtime"


def project_runtime_src_dir(state_dir: Path) -> Path:
    return project_runtime_dir(state_dir) / "src"


def project_runtime_bin_dir(state_dir: Path) -> Path:
    return project_runtime_dir(state_dir) / "bin"


def project_runtime_skills_dir(state_dir: Path) -> Path:
    return project_runtime_dir(state_dir) / "skills"


def project_viewer_dir(state_dir: Path) -> Path:
    return state_dir / "viewer"


def project_viewer_state_path(state_dir: Path) -> Path:
    return project_viewer_dir(state_dir) / "viewer-state.json"


def project_viewer_cycles_path(state_dir: Path) -> Path:
    return project_viewer_dir(state_dir) / "cycles.json"


def project_viewer_chats_path(state_dir: Path) -> Path:
    return project_viewer_dir(state_dir) / "chats.json"


def project_viewer_chats_at_dir(state_dir: Path) -> Path:
    return project_viewer_dir(state_dir) / "chats-at"


def project_viewer_state_at_dir(state_dir: Path) -> Path:
    return project_viewer_dir(state_dir) / "state-at"


def supervisor_root_dir(worker_repo_path: Path) -> Path:
    return project_state_dir_for_repo(worker_repo_path) / "supervisor"


def supervisor_workspace_repo_path(worker_repo_path: Path) -> Path:
    return supervisor_root_dir(worker_repo_path) / "repo"


def supervisor_workspace_home_path(worker_repo_path: Path) -> Path:
    return supervisor_root_dir(worker_repo_path) / "home"


def supervisor_workspace_cache_path(worker_repo_path: Path) -> Path:
    return supervisor_root_dir(worker_repo_path) / "cache"


def supervisor_workspace_marker_path(repo_path: Path) -> Path:
    return repo_path / ".trellis-supervisor.json"


def project_checker_dir(state_dir: Path) -> Path:
    return state_dir / "checker"
