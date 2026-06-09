"""Canonical git-tracked supervisor artifact paths."""

from __future__ import annotations

from pathlib import Path


PROJECT_HISTORY_DIRNAME = ".trellis-history"
WORKER_STATE_DIRNAME = "worker_state"

SUPERVISOR_STATE_FILENAME = "supervisor_state.json"
WORKER_HANDOFF_FILENAME = "worker_handoff.json"
PAPER_RESULT_FILENAME = "paper_faithfulness_result.json"
CORR_RESULT_FILENAME = "correspondence_result.json"
SOUND_RESULT_FILENAME = "soundness_result.json"
REVIEW_RESULT_FILENAME = "reviewer_decision.json"
LAST_INVALID_DIRNAME = "last_invalid"
LAST_INVALID_METADATA_FILENAME = "metadata.json"


def project_history_dir(repo_path: Path) -> Path:
    return repo_path / PROJECT_HISTORY_DIRNAME


def supervisor_state_path(repo_path: Path) -> Path:
    return project_history_dir(repo_path) / SUPERVISOR_STATE_FILENAME


def worker_handoff_path(repo_path: Path) -> Path:
    return project_history_dir(repo_path) / WORKER_HANDOFF_FILENAME


def paper_result_path(repo_path: Path) -> Path:
    return project_history_dir(repo_path) / PAPER_RESULT_FILENAME


def corr_result_path(repo_path: Path) -> Path:
    return project_history_dir(repo_path) / CORR_RESULT_FILENAME


def sound_result_path(repo_path: Path) -> Path:
    return project_history_dir(repo_path) / SOUND_RESULT_FILENAME


def review_result_path(repo_path: Path) -> Path:
    return project_history_dir(repo_path) / REVIEW_RESULT_FILENAME


def worker_state_dir(repo_path: Path) -> Path:
    return project_history_dir(repo_path) / WORKER_STATE_DIRNAME


def last_invalid_dir(repo_path: Path) -> Path:
    return worker_state_dir(repo_path) / LAST_INVALID_DIRNAME


def last_invalid_tablet_dir(repo_path: Path) -> Path:
    return last_invalid_dir(repo_path) / "Tablet"


def last_invalid_metadata_path(repo_path: Path) -> Path:
    return last_invalid_dir(repo_path) / LAST_INVALID_METADATA_FILENAME
