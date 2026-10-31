"""Regression tests for the bounded operator-side PV archive validator."""

from __future__ import annotations

import importlib.util
import hashlib
import io
import sys
import tarfile
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parent.parent
MODULE_PATH = ROOT / "scripts" / "pv_archive.py"
SPEC = importlib.util.spec_from_file_location("pv_archive", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
pv_archive = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(pv_archive)


def _write_archive(
    path: Path, entries: list[tuple[str, bytes, bytes | None]]
) -> Path:
    """Write (name, content, optional type) entries without path deduplication."""

    with tarfile.open(path, "w:gz", format=tarfile.USTAR_FORMAT) as archive:
        for name, content, entry_type in entries:
            member = tarfile.TarInfo(name)
            member.type = entry_type or tarfile.REGTYPE
            if member.isfile():
                member.size = len(content)
                archive.addfile(member, io.BytesIO(content))
            else:
                member.size = 0
                member.linkname = "Cargo.toml" if member.islnk() else ""
                archive.addfile(member)
    return path


def test_inspect_and_extract_safe_crate(tmp_path: Path) -> None:
    archive = _write_archive(
        tmp_path / "crate.tar.gz",
        [
            ("crate/Cargo.toml", b"[package]\n", None),
            ("crate/src/lib.rs", b"pub fn alpha() {}\n", None),
        ],
    )

    metadata = pv_archive.inspect_archive(str(archive), 1024)
    assert metadata["ok"] is True
    assert metadata["entries"] == 2
    assert metadata["expanded_bytes"] == 28
    assert metadata["archive_sha256"] == hashlib.sha256(archive.read_bytes()).hexdigest()
    assert metadata["source_root"] == "crate"
    assert metadata["source_tree_sha256"] == metadata["source_manifest"]["source_tree_sha256"]
    assert [entry["relative_path"] for entry in metadata["source_manifest"]["files"]] == [
        "Cargo.toml", "src/lib.rs"
    ]

    destination = tmp_path / "unpacked"
    assert pv_archive.extract_archive(str(archive), destination, 1024) == metadata
    assert (destination / "crate/src/lib.rs").read_bytes() == b"pub fn alpha() {}\n"


@pytest.mark.parametrize(
    ("entries", "expected_root", "expected_files"),
    [
        (
            [("module/lib.rs", b"pub fn alpha() {}\n", None)],
            "module",
            ["lib.rs"],
        ),
        (
            [("traits.rs", b"pub trait T {}\n", None)],
            "",
            ["traits.rs"],
        ),
        (
            [
                ("upstream/mod.rs", b"mod inner;\n", None),
                ("upstream/inner.rs", b"pub fn f() {}\n", None),
            ],
            "upstream",
            ["inner.rs", "mod.rs"],
        ),
        (
            [
                ("engine/mod.rs", b"pub fn f() {}\n", None),
                ("traits.rs", b"pub trait T {}\n", None),
            ],
            "",
            ["engine/mod.rs", "traits.rs"],
        ),
    ],
)
def test_generic_source_root_discovery_without_manifest(
    tmp_path: Path,
    entries: list[tuple[str, bytes, bytes | None]],
    expected_root: str,
    expected_files: list[str],
) -> None:
    archive = _write_archive(tmp_path / "source.tar.gz", entries)
    metadata = pv_archive.inspect_archive(str(archive), 4096)
    assert metadata["source_root"] == expected_root
    assert [entry["relative_path"] for entry in metadata["source_manifest"]["files"]] == expected_files

    destination = tmp_path / "materialized"
    pv_archive.extract_archive(str(archive), destination, 4096)
    source_root = destination / expected_root if expected_root else destination
    for name, content, entry_type in entries:
        if entry_type is None:
            relative = Path(name)
            if expected_root:
                relative = Path(*relative.parts[1:])
            assert (source_root / relative).read_bytes() == content


def test_checked_in_raw_example_is_a_verbatim_manifestless_upload(tmp_path: Path) -> None:
    source = next((ROOT / "examples").glob("pv_*/source/PROVENANCE.md")).parent
    raw_files = sorted(source.rglob("*.rs"))
    assert len(raw_files) == 10
    archive = _write_archive(
        tmp_path / "raw-source.tar.gz",
        [
            (path.relative_to(source).as_posix(), path.read_bytes(), None)
            for path in raw_files
        ],
    )
    metadata = pv_archive.inspect_archive(str(archive), 1024 * 1024)
    assert metadata["source_root"] == ""
    assert all(
        path not in {"Cargo.toml", "Cargo.lock", "PROVENANCE.md"}
        for path in (entry["relative_path"] for entry in metadata["source_manifest"]["files"])
    )
    destination = tmp_path / "materialized"
    pv_archive.extract_archive(str(archive), destination, 1024 * 1024)
    for path in raw_files:
        relative = path.relative_to(source)
        assert (destination / relative).read_bytes() == path.read_bytes()


@pytest.mark.parametrize(
    ("label", "entries", "limit", "message"),
    [
        (
            "path traversal",
            [("../Cargo.toml", b"[package]\n", None)],
            1024,
            "unsafe archive path",
        ),
        (
            "absolute path",
            [("/Cargo.toml", b"[package]\n", None)],
            1024,
            "unsafe archive path",
        ),
        (
            "ambiguous repeated separator",
            [("source//mod.rs", b"pub fn f() {}\n", None)],
            1024,
            "unsafe archive path",
        ),
        (
            "control character in path",
            [("source/bad\x01.rs", b"pub fn f() {}\n", None)],
            1024,
            "unsafe archive path",
        ),
        (
            "duplicate paths",
            [
                ("Cargo.toml", b"[package]\n", None),
                ("Cargo.toml", b"[package]\n", None),
            ],
            1024,
            "duplicate archive path",
        ),
        (
            "hardlinks",
            [
                ("Cargo.toml", b"[package]\n", None),
                ("cargo-hardlink", b"", tarfile.LNKTYPE),
            ],
            1024,
            "only directories and regular files",
        ),
        (
            "expanded-size cap",
            [("Cargo.toml", b"[package]\n", None), ("large", b"x" * 65, None)],
            64,
            "expanded source exceeds",
        ),
        ("empty regular-file tree", [("only-dir", b"", tarfile.DIRTYPE)], 1024, "no regular files"),
        ("special file", [("input.fifo", b"", tarfile.FIFOTYPE)], 1024, "only directories and regular files"),
    ],
)
def test_archive_negative_controls(
    tmp_path: Path,
    label: str,
    entries: list[tuple[str, bytes, bytes | None]],
    limit: int,
    message: str,
) -> None:
    archive = _write_archive(tmp_path / "negative.tar.gz", entries)
    with pytest.raises(pv_archive.ArchiveError, match=message):
        pv_archive.inspect_archive(str(archive), limit)


class _IncrementalArchive:
    def __init__(self, members: int, first_size: int = 0) -> None:
        self.members = members
        self.first_size = first_size
        self.yielded = 0

    def __enter__(self) -> "_IncrementalArchive":
        return self

    def __exit__(self, *_args: object) -> None:
        return None

    def __iter__(self):
        for index in range(self.members):
            self.yielded += 1
            member = tarfile.TarInfo("Cargo.toml" if index == 0 else f"entry-{index}")
            member.size = self.first_size if index == 0 else 0
            yield member

    def getmembers(self):
        raise AssertionError("inspect_archive must never materialize all headers")

    def extractfile(self, member: tarfile.TarInfo):
        return io.BytesIO(b"x" * member.size)


def test_invalid_utf8_archive_name_is_a_structured_path_rejection(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    class InvalidNameArchive(_IncrementalArchive):
        def __iter__(self):
            member = tarfile.TarInfo("bad\udcff.rs")
            member.size = 1
            yield member

    monkeypatch.setattr(pv_archive, "_open_archive", lambda _name: InvalidNameArchive(1))
    with pytest.raises(pv_archive.ArchiveError, match="unsafe archive path"):
        pv_archive.inspect_archive("fixture.tar.gz", 1024)


def test_member_cap_stops_header_iteration_immediately(monkeypatch: pytest.MonkeyPatch) -> None:
    archive = _IncrementalArchive(pv_archive.MAX_MEMBERS + 100)
    monkeypatch.setattr(pv_archive, "_open_archive", lambda _name: archive)

    with pytest.raises(pv_archive.ArchiveError, match="more than 10000 entries"):
        pv_archive.inspect_archive("fixture.tar.gz", 1024)
    assert archive.yielded == pv_archive.MAX_MEMBERS + 1


def test_size_cap_stops_header_iteration_immediately(monkeypatch: pytest.MonkeyPatch) -> None:
    archive = _IncrementalArchive(100, first_size=65)
    monkeypatch.setattr(pv_archive, "_open_archive", lambda _name: archive)

    with pytest.raises(pv_archive.ArchiveError, match="expanded source exceeds"):
        pv_archive.inspect_archive("fixture.tar.gz", 64)
    assert archive.yielded == 1


def test_real_tarfile_inspection_never_calls_getmembers(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    archive = _write_archive(
        tmp_path / "crate.tar.gz", [("Cargo.toml", b"[package]\n", None)]
    )

    def forbidden(_self: tarfile.TarFile):
        raise AssertionError("getmembers materializes the complete archive")

    monkeypatch.setattr(tarfile.TarFile, "getmembers", forbidden)
    assert pv_archive.inspect_archive(str(archive), 1024)["entries"] == 1


def test_stdin_is_rejected_without_reading_it(monkeypatch: pytest.MonkeyPatch) -> None:
    class ExplodingBuffer:
        def read(self, *_args: object) -> bytes:
            raise AssertionError("stdin was read without a bound")

    class ExplodingStdin:
        buffer = ExplodingBuffer()

    monkeypatch.setattr(sys, "stdin", ExplodingStdin())
    with pytest.raises(pv_archive.ArchiveError, match="stdin archives are unsupported"):
        pv_archive.inspect_archive("-", 1024)


def test_failed_extraction_removes_partial_destination(tmp_path: Path) -> None:
    archive = _write_archive(
        tmp_path / "traversal.tar.gz",
        [("Cargo.toml", b"[package]\n", None), ("../escape", b"bad", None)],
    )
    destination = tmp_path / "must-not-remain"

    with pytest.raises(pv_archive.ArchiveError, match="unsafe archive path"):
        pv_archive.extract_archive(str(archive), destination, 1024)
    assert not destination.exists()
