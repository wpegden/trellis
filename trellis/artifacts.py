"""Helpers for supervisor-owned agent result artifacts."""

from __future__ import annotations

from pathlib import Path


def artifact_stem(canonical_name: str) -> str:
    if canonical_name.endswith(".json"):
        return canonical_name[:-5]
    return canonical_name


def staging_dir(state_dir: Path) -> Path:
    return state_dir / "staging"


def raw_json_path(state_dir: Path, canonical_name: str) -> Path:
    return staging_dir(state_dir) / f"{artifact_stem(canonical_name)}.raw.json"


def done_marker_path(state_dir: Path, canonical_name: str) -> Path:
    return staging_dir(state_dir) / f"{artifact_stem(canonical_name)}.done"


def prompt_artifact_paths(state_dir: Path, repo_path: Path, canonical_name: str) -> dict[str, Path]:
    return {
        "canonical": repo_path / canonical_name,
        "raw": raw_json_path(state_dir, canonical_name),
        "done": done_marker_path(state_dir, canonical_name),
    }
