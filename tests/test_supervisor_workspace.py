"""Regression tests for supervisor package-tree materialization."""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from trellis.atomic_actions.observations import (
    tablet_source_closure_hash,
    write_olean_srcclosure,
)
from trellis.supervisor_workspace import (
    _ensure_supervisor_lake_packages,
    _sync_repo_tree,
    propagate_tablet_back_to_worker,
)


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


def test_sync_repo_tree_leaves_checker_owned_isabelle_subtree_alone(
    tmp_path: Path,
) -> None:
    worker = tmp_path / "worker"
    supervisor = tmp_path / "supervisor"
    (worker / "Tablet").mkdir(parents=True)
    (worker / "Tablet" / "A.thy").write_text("theory A\n", encoding="utf-8")
    (worker / "isabelle").mkdir()
    (worker / "isabelle" / "ROOT").write_text("stale-seed\n", encoding="utf-8")
    (supervisor / "isabelle" / "base").mkdir(parents=True)
    (supervisor / "isabelle" / "ROOT").write_text("current\n", encoding="utf-8")
    (supervisor / "isabelle" / "Tablet_A.thy").write_text(
        "projection\n", encoding="utf-8"
    )
    (supervisor / "isabelle" / "Tablet_Preamble.thy").write_text(
        "preamble\n", encoding="utf-8"
    )

    _sync_repo_tree(worker, supervisor)

    isa = supervisor / "isabelle"
    assert (isa / "Tablet_A.thy").read_text(encoding="utf-8") == "projection\n"
    assert (isa / "Tablet_Preamble.thy").read_text(encoding="utf-8") == "preamble\n"
    assert (isa / "base").is_dir()
    assert (isa / "ROOT").read_text(encoding="utf-8") == "current\n"
    assert (supervisor / "Tablet" / "A.thy").read_text(encoding="utf-8") == (
        "theory A\n"
    )


def test_sync_repo_tree_is_inert_without_worker_isabelle_dir(
    tmp_path: Path,
) -> None:
    worker = tmp_path / "worker"
    supervisor = tmp_path / "supervisor"
    (worker / "Tablet").mkdir(parents=True)
    (worker / "Tablet" / "A.thy").write_text("theory A\n", encoding="utf-8")
    supervisor.mkdir()

    _sync_repo_tree(worker, supervisor)

    assert not (supervisor / "isabelle").exists()
    assert (supervisor / "Tablet" / "A.thy").read_text(encoding="utf-8") == (
        "theory A\n"
    )


def test_propagation_mirrors_complete_split_olean_bundle(tmp_path: Path) -> None:
    worker = tmp_path / "worker"
    supervisor = tmp_path / "supervisor"
    for repo in (worker, supervisor):
        (repo / "Tablet").mkdir(parents=True)
    (supervisor / "Tablet" / "A.lean").write_text("module\n", encoding="utf-8")
    build = supervisor / ".lake" / "build" / "lib" / "lean" / "Tablet"
    build.mkdir(parents=True)
    suffixes = (
        ".olean",
        ".olean.server",
        ".olean.private",
        ".olean.hash",
        ".olean.server.hash",
        ".olean.private.hash",
    )
    for suffix in suffixes:
        (build / f"A{suffix}").write_bytes(suffix.encode())

    result = propagate_tablet_back_to_worker(supervisor, worker)

    worker_build = worker / ".lake" / "build" / "lib" / "lean" / "Tablet"
    assert result["oleans_mirrored"] == len(suffixes)
    for suffix in suffixes:
        assert (worker_build / f"A{suffix}").read_bytes() == suffix.encode()


def _seed_closure_fixture(worker: Path, supervisor: Path) -> Path:
    """Minimal repo pair where source-closure hashing is computable.

    ``tablet_source_closure_hash`` fails closed (``None``) without the check
    script and the preamble, so both repos get a Tablet/Preamble.lean and the
    supervisor gets ``.trellis/scripts/check.py`` (``.trellis`` is preserved,
    never synced). Returns the supervisor build dir.
    """
    for repo in (worker, supervisor):
        (repo / "Tablet").mkdir(parents=True, exist_ok=True)
        (repo / "Tablet" / "Preamble.lean").write_text(
            "-- preamble\n", encoding="utf-8"
        )
    scripts = supervisor / ".trellis" / "scripts"
    scripts.mkdir(parents=True, exist_ok=True)
    (scripts / "check.py").write_text("# check\n", encoding="utf-8")
    build_dir = supervisor / ".lake" / "build" / "lib" / "lean" / "Tablet"
    build_dir.mkdir(parents=True, exist_ok=True)
    return build_dir


def test_source_reset_invalidates_content_stale_supervisor_build_artifacts(
    tmp_path: Path,
) -> None:
    """A rewind-style sync purges artifacts whose provenance sidecar
    disagrees with the freshly synced source content."""
    worker = tmp_path / "worker"
    supervisor = tmp_path / "supervisor"
    build_dir = _seed_closure_fixture(worker, supervisor)
    worker_source = worker / "Tablet" / "A.lean"
    supervisor_source = supervisor / "Tablet" / "A.lean"
    stale_olean = build_dir / "A.olean"

    # The supervisor still holds the abandoned line's source, olean, and the
    # sidecar recording the closure hash that olean was built from.
    supervisor_source.write_text("theorem A : False := by simp\n", encoding="utf-8")
    stale_olean.write_bytes(b"pre-rewind olean")
    abandoned_hash = tablet_source_closure_hash(supervisor, "A")
    assert abandoned_hash is not None
    write_olean_srcclosure(supervisor, "A", abandoned_hash)

    # The rewound (worker) line carries different content for A.
    worker_source.write_text("theorem A : True := by trivial\n", encoding="utf-8")

    result = _sync_repo_tree(worker, supervisor)

    assert supervisor_source.read_bytes() == worker_source.read_bytes()
    assert not stale_olean.exists(), (
        "a sidecar recording an abandoned line's closure hash must not "
        "preserve that line's build artifacts across a source reset"
    )
    assert not (build_dir / "A.olean.srcclosure").exists()
    assert result["stale_build_artifacts_removed"] == 2


def test_mass_source_restamp_does_not_mass_delete_current_artifacts(
    tmp_path: Path,
) -> None:
    """Regression for the mtime purge: a sync that restamps every source
    mtime (the normal burst shape) must delete nothing whose content is
    current, while a genuinely content-stale node's artifacts still go."""
    worker = tmp_path / "worker"
    supervisor = tmp_path / "supervisor"
    build_dir = _seed_closure_fixture(worker, supervisor)

    for node, body in (("A", "theorem A : True := by trivial\n"),
                       ("B", "theorem B : 1 = 1 := rfl\n")):
        (worker / "Tablet" / f"{node}.lean").write_text(body, encoding="utf-8")
        (supervisor / "Tablet" / f"{node}.lean").write_text(body, encoding="utf-8")
        (build_dir / f"{node}.olean").write_bytes(b"olean bytes")
        node_hash = tablet_source_closure_hash(supervisor, node)
        assert node_hash is not None
        write_olean_srcclosure(supervisor, node, node_hash)

    # B's source genuinely changes; its sidecar keeps the old closure hash.
    (worker / "Tablet" / "B.lean").write_text(
        "theorem B : 2 = 2 := rfl\n", encoding="utf-8"
    )

    # Restamp EVERY worker source to a fresh mtime, as a burst does: under
    # the old mtime cutoff this made every artifact "stale".
    restamp_ns = 1_800_000_000_000_000_000
    for source in (worker / "Tablet").glob("*.lean"):
        os.utime(source, ns=(restamp_ns, restamp_ns))

    result = _sync_repo_tree(worker, supervisor)

    assert (build_dir / "A.olean").exists(), (
        "a mass mtime restamp with unchanged content must not delete "
        "current artifacts"
    )
    assert (build_dir / "A.olean.srcclosure").exists()
    assert not (build_dir / "B.olean").exists(), (
        "a content-stale node's artifacts must still be invalidated"
    )
    assert not (build_dir / "B.olean.srcclosure").exists()
    assert result["stale_build_artifacts_removed"] == 2
