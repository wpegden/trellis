#!/usr/bin/env python3
"""Validate and safely extract viewer-uploaded PV source archives."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sys
import tarfile
import unicodedata
from pathlib import Path, PurePosixPath

try:
    from scripts.pv_source_tree import (
        SOURCE_TREE_MANIFEST_SCHEMA,
        source_tree_manifest,
        source_tree_root_from_entries,
    )
except ModuleNotFoundError:  # direct ``python scripts/pv_archive.py`` execution
    from pv_source_tree import (  # type: ignore[no-redef]
        SOURCE_TREE_MANIFEST_SCHEMA,
        source_tree_manifest,
        source_tree_root_from_entries,
    )


MAX_MEMBERS = 10_000


class ArchiveError(ValueError):
    pass


def _open_archive(name: str) -> tarfile.TarFile:
    if name == "-":
        raise ArchiveError(
            "stdin archives are unsupported; provide a bounded .tar.gz file path"
        )
    # Stream mode makes the member and expanded-size checks below fire while
    # gzip data is being decoded.  In particular, do not use getmembers(): it
    # parses the complete decompressed stream before returning control.
    return tarfile.open(name, mode="r|gz")


def _sha256_file(name: str) -> str:
    digest = hashlib.sha256()
    with open(name, "rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def inspect_archive(name: str, limit: int) -> dict[str, object]:
    if limit <= 0:
        raise ArchiveError("archive size limit must be positive")
    try:
        archive = _open_archive(name)
    except (OSError, tarfile.TarError) as exc:
        raise ArchiveError(f"not an extractable .tar.gz archive: {exc}") from exc
    with archive:
        seen: set[str] = set()
        members: list[tuple[PurePosixPath, bool]] = []
        files: list[tuple[PurePosixPath, int, str]] = []
        total = 0
        entries = 0
        try:
            for member in archive:
                entries += 1
                if entries > MAX_MEMBERS:
                    raise ArchiveError(
                        f"source archive has more than {MAX_MEMBERS} entries"
                    )
                original = member.name
                raw = original
                while raw.startswith("./"):
                    raw = raw[2:]
                if raw in {"", "."} and member.isdir():
                    continue
                normalized_raw = raw.rstrip("/") if member.isdir() else raw
                try:
                    normalized_raw.encode("utf-8")
                except UnicodeEncodeError as exc:
                    raise ArchiveError(f"unsafe archive path: {original!r}") from exc
                raw_parts = normalized_raw.split("/")
                path = PurePosixPath(normalized_raw)
                if (
                    not normalized_raw
                    or normalized_raw.startswith("/")
                    or "\\" in normalized_raw
                    or any(
                        part in {"", ".", ".."}
                        or any(unicodedata.category(character) == "Cc" for character in part)
                        for part in raw_parts
                    )
                    or (path.parts and ":" in path.parts[0])
                ):
                    raise ArchiveError(f"unsafe archive path: {original!r}")
                normalized = path.as_posix()
                if normalized in seen:
                    raise ArchiveError(f"duplicate archive path: {normalized}")
                seen.add(normalized)
                if not (member.isdir() or member.isfile()):
                    raise ArchiveError(
                        f"unsupported archive entry {normalized!r}: only directories and regular files are allowed"
                    )
                members.append((path, member.isdir()))
                if member.isfile():
                    total += member.size
                    if total > limit:
                        raise ArchiveError(
                            f"expanded source exceeds the {limit}-byte upload limit"
                        )
                    source = archive.extractfile(member)
                    if source is None:
                        raise ArchiveError(f"could not read archive member: {member.name}")
                    digest = hashlib.sha256()
                    read = 0
                    with source:
                        while chunk := source.read(1024 * 1024):
                            read += len(chunk)
                            digest.update(chunk)
                    if read != member.size:
                        raise ArchiveError(f"archive member is truncated: {normalized}")
                    files.append((path, member.size, digest.hexdigest()))
        except (OSError, tarfile.TarError) as exc:
            raise ArchiveError(f"not an extractable .tar.gz archive: {exc}") from exc

        if not files:
            raise ArchiveError("source archive contains no regular files")

        top_levels = {path.parts[0] for path, _is_dir in members}
        source_root = ""
        if len(top_levels) == 1:
            common = next(iter(top_levels))
            # A sole top-level regular file is a source member, not a directory
            # that can serve as the materialized root.
            if not any(
                len(path.parts) == 1 and not is_dir for path, is_dir in members
            ):
                source_root = common

        manifest_files = []
        for path, length, digest in files:
            relative_parts = path.parts[1:] if source_root else path.parts
            if not relative_parts:
                raise ArchiveError("source root resolves to a regular file")
            manifest_files.append(
                {
                    "relative_path": PurePosixPath(*relative_parts).as_posix(),
                    "byte_length": length,
                    "sha256_of_raw_bytes": digest,
                }
            )
        manifest_files.sort(key=lambda item: item["relative_path"].encode("utf-8"))
        manifest = {
            "schema": SOURCE_TREE_MANIFEST_SCHEMA,
            "files": manifest_files,
            "source_tree_sha256": source_tree_root_from_entries(manifest_files),
        }
        return {
            "ok": True,
            "entries": entries,
            "expanded_bytes": total,
            "archive_sha256": _sha256_file(name),
            "source_root": source_root,
            "source_manifest": manifest,
            "source_tree_sha256": manifest["source_tree_sha256"],
        }


def extract_archive(name: str, destination: Path, limit: int) -> dict[str, object]:
    if destination.exists():
        raise ArchiveError(f"extraction destination already exists: {destination}")
    # Inspect and extract from separate opens so extraction independently
    # traverses the member stream rather than trusting intake alone.
    metadata = inspect_archive(name, limit)
    try:
        archive = _open_archive(name)
    except (OSError, tarfile.TarError) as exc:
        raise ArchiveError(f"not an extractable .tar.gz archive: {exc}") from exc
    destination.mkdir(parents=False, mode=0o700)
    written = 0
    entries = 0
    seen: set[str] = set()
    try:
        with archive:
            for member in archive:
                entries += 1
                if entries > MAX_MEMBERS:
                    raise ArchiveError(
                        "archive changed after validation or has more than "
                        f"{MAX_MEMBERS} entries"
                    )
                raw = member.name
                while raw.startswith("./"):
                    raw = raw[2:]
                if raw in {"", "."} and member.isdir():
                    continue
                normalized_raw = raw.rstrip("/") if member.isdir() else raw
                try:
                    normalized_raw.encode("utf-8")
                except UnicodeEncodeError as exc:
                    raise ArchiveError(f"unsafe archive path: {member.name!r}") from exc
                raw_parts = normalized_raw.split("/")
                rel = PurePosixPath(normalized_raw)
                if (
                    not normalized_raw
                    or normalized_raw.startswith("/")
                    or "\\" in normalized_raw
                    or any(
                        part in {"", ".", ".."}
                        or any(unicodedata.category(character) == "Cc" for character in part)
                        for part in raw_parts
                    )
                    or (rel.parts and ":" in rel.parts[0])
                    or not (member.isdir() or member.isfile())
                ):
                    raise ArchiveError(
                        f"archive changed after validation or contains an unsafe member: {member.name!r}"
                    )
                normalized = rel.as_posix()
                if normalized in seen:
                    raise ArchiveError(
                        "archive changed after validation or contains a duplicate path: "
                        f"{normalized}"
                    )
                seen.add(normalized)
                target = destination.joinpath(*rel.parts)
                if member.isdir():
                    target.mkdir(parents=True, exist_ok=True, mode=0o700)
                    continue
                target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
                source = archive.extractfile(member)
                if source is None:
                    raise ArchiveError(f"could not read archive member: {member.name}")
                with source, target.open("xb") as output:
                    while chunk := source.read(1024 * 1024):
                        written += len(chunk)
                        if written > limit:
                            raise ArchiveError(
                                f"expanded source exceeds the {limit}-byte upload limit"
                            )
                        output.write(chunk)
                os.chmod(target, 0o600)
        if entries != metadata["entries"] or written != metadata["expanded_bytes"]:
            raise ArchiveError("archive members changed while extracting")
        if _sha256_file(name) != metadata["archive_sha256"]:
            raise ArchiveError("archive bytes changed while extracting")
        source_root = str(metadata["source_root"])
        materialized = destination / source_root if source_root else destination
        actual_manifest = source_tree_manifest(materialized)
        if actual_manifest != metadata["source_manifest"]:
            raise ArchiveError("materialized source tree differs from the inspected archive")
    except Exception:
        shutil.rmtree(destination, ignore_errors=True)
        raise
    return metadata


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    for command in ("inspect", "extract"):
        child = sub.add_parser(command)
        child.add_argument("archive")
        child.add_argument("--limit", type=int, required=True)
        if command == "extract":
            child.add_argument("destination", type=Path)
    args = parser.parse_args(argv)
    try:
        if args.command == "inspect":
            metadata = inspect_archive(args.archive, args.limit)
        else:
            metadata = extract_archive(args.archive, args.destination, args.limit)
    except (ArchiveError, OSError, tarfile.TarError) as exc:
        print(str(exc), file=sys.stderr)
        return 1
    print(json.dumps(metadata, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
