"""Regression tests for supervisor package-tree materialization."""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from trellis.supervisor_workspace import _ensure_supervisor_lake_packages


def _seed_package(tmp_path: Path) -> tuple[Path, Path, Path]:
    worker = tmp_path / "worker"
    supervisor = tmp_path / "supervisor"
    package = worker / ".lake" / "packages" / "aeneas"
    build = package / "backends" / "lean" / ".lake" / "build" / "lib" / "lean"
    build.mkdir(parents=True)
    config = package / "backends" / "lean" / ".lake" / "config" / "aeneas"
    config.mkdir(parents=True)
    (package / "README.md").write_text("aeneas\n", encoding="utf-8")
    (build / "Aeneas.olean").write_bytes(b"olean")
    (config / "lakefile.olean").write_bytes(b"config")
    return worker, supervisor, package


def test_lake_package_sync_preserves_safe_broken_symlink(tmp_path: Path) -> None:
    worker, supervisor, package = _seed_package(tmp_path)
    target = "src/_build/default/_doc/_html/aeneas/index.html"
    os.symlink(target, package / "doc.html")

    result = _ensure_supervisor_lake_packages(worker, supervisor)

    copied_link = supervisor / ".lake" / "packages" / "aeneas" / "doc.html"
    assert result["packages_linked"] == 1
    assert copied_link.is_symlink()
    assert os.readlink(copied_link) == target
    assert not copied_link.exists()  # The generated documentation is absent.


def test_lake_package_sync_resumes_partially_materialized_package(
    tmp_path: Path,
) -> None:
    worker, supervisor, package = _seed_package(tmp_path)
    os.symlink("missing.html", package / "doc.html")
    partial = supervisor / ".lake" / "packages" / "aeneas"
    partial.mkdir(parents=True)
    (partial / "README.md").write_text("aeneas\n", encoding="utf-8")

    result = _ensure_supervisor_lake_packages(worker, supervisor)

    assert result["packages_linked"] == 0
    assert (partial / "doc.html").is_symlink()
    assert (
        partial
        / "backends"
        / "lean"
        / ".lake"
        / "build"
        / "lib"
        / "lean"
        / "Aeneas.olean"
    ).read_bytes() == b"olean"
    assert (
        partial
        / "backends"
        / "lean"
        / ".lake"
        / "config"
        / "aeneas"
        / "lakefile.olean"
    ).read_bytes() == b"config"


def test_lake_package_sync_rejects_escaping_symlink(tmp_path: Path) -> None:
    worker, supervisor, package = _seed_package(tmp_path)
    os.symlink("../../../../outside", package / "escape")

    with pytest.raises(RuntimeError, match="escaping symlink"):
        _ensure_supervisor_lake_packages(worker, supervisor)
