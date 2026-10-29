#!/usr/bin/env python3
"""Canonical, no-follow identity for a regular-file PV source tree.

The Rust implementation lives in ``kernel/src/trust_base/source_tree.rs``.
The root is deliberately the promoted bootstrap identity: the
``manifest-node`` tagged hash of the canonical JSON file-entry array.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import stat
import subprocess
import sys
import unicodedata
from pathlib import Path
from typing import Any

# Direct script callers start with scripts/ on sys.path, just like the
# extraction CLI. Resolve our host package from this trusted source tree.
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))


SOURCE_TREE_MANIFEST_SCHEMA = "trellis-source-tree-manifest/v1"
SOURCE_PARTITION_MANIFEST_SCHEMA = "trellis-phase0-source-partition-manifest/v1"
_HASH_PREFIX = b"trellis-trust-v1\0"


class SourceTreeError(ValueError):
    pass


def canonical_json(value: Any) -> bytes:
    return json.dumps(
        value, ensure_ascii=False, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")


def tagged_hash(tag: str, payload: bytes) -> str:
    tag_bytes = tag.encode("utf-8")
    framed = (
        _HASH_PREFIX
        + len(tag_bytes).to_bytes(4, "big")
        + tag_bytes
        + len(payload).to_bytes(8, "big")
        + payload
    )
    return hashlib.sha256(framed).hexdigest()


def _relative_path(parts: tuple[str, ...]) -> str:
    if not parts or any(
        part in {"", ".", ".."}
        or "\\" in part
        or "\x00" in part
        or any(unicodedata.category(character) == "Cc" for character in part)
        for part in parts
    ):
        raise SourceTreeError("source tree contains an invalid relative path")
    relative = "/".join(parts)
    try:
        relative.encode("utf-8")
    except UnicodeEncodeError as exc:
        raise SourceTreeError("source tree path is not valid UTF-8") from exc
    return relative


def source_tree_entries(root: Path, *, excluded_names=(), allow_empty=False) -> list[dict[str, Any]]:
    from trellis.checker.admission import cooperative
    return cooperative(_source_tree_entries)(root, excluded_names=excluded_names, allow_empty=allow_empty)


def _stream_digest(stream):
    from trellis.checker.admission import cooperative_poll
    size, digest = 0, hashlib.sha256()
    for chunk in iter(lambda: stream.read(1 << 20), b''):
        cooperative_poll()
        size += len(chunk)
        digest.update(chunk)
    return size, digest.hexdigest()


def _source_tree_entries(root: Path, *, excluded_names=(), allow_empty=False) -> list[dict[str, Any]]:
    """Read every regular file without following links, in UTF-8 byte order."""

    root = Path(root)
    try:
        root_stat = os.lstat(root)
    except OSError as exc:
        raise SourceTreeError(f"cannot inspect source tree {root}: {exc}") from exc
    if stat.S_ISLNK(root_stat.st_mode) or not stat.S_ISDIR(root_stat.st_mode):
        raise SourceTreeError(f"source tree root is not a non-symlink directory: {root}")

    files: list[dict[str, Any]] = []
    seen: set[str] = set()

    def walk(directory: Path, parts: tuple[str, ...]) -> None:
        try:
            children = list(os.scandir(directory))
        except OSError as exc:
            raise SourceTreeError(f"cannot enumerate source tree {directory}: {exc}") from exc
        children.sort(key=lambda child: os.fsencode(child.name))
        for child in children:
            if child.name in excluded_names:
                continue
            child_parts = (*parts, child.name)
            relative = _relative_path(child_parts)
            try:
                metadata = child.stat(follow_symlinks=False)
            except OSError as exc:
                raise SourceTreeError(f"cannot inspect source tree path {relative}: {exc}") from exc
            mode = metadata.st_mode
            if stat.S_ISLNK(mode):
                raise SourceTreeError(f"source tree contains a symlink: {relative}")
            if stat.S_ISDIR(mode):
                walk(Path(child.path), child_parts)
                continue
            if not stat.S_ISREG(mode):
                raise SourceTreeError(f"source tree contains a special file: {relative}")

            flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
            try:
                descriptor = os.open(child.path, flags)
            except OSError as exc:
                raise SourceTreeError(f"cannot open source file {relative}: {exc}") from exc
            try:
                opened = os.fstat(descriptor)
                if (
                    not stat.S_ISREG(opened.st_mode)
                    or opened.st_dev != metadata.st_dev
                    or opened.st_ino != metadata.st_ino
                ):
                    raise SourceTreeError(
                        f"source file changed type or identity while hashing: {relative}"
                    )
                with os.fdopen(descriptor, "rb", closefd=False) as stream:
                    length, content_digest = _stream_digest(stream)
            finally:
                os.close(descriptor)
            if length != opened.st_size:
                raise SourceTreeError(f"source file changed length while hashing: {relative}")
            if relative in seen:
                raise SourceTreeError(f"source tree repeats normalized path: {relative}")
            seen.add(relative)
            files.append(
                {
                    "relative_path": relative,
                    "byte_length": length,
                    "sha256_of_raw_bytes": content_digest,
                }
            )

    walk(root, ())
    files.sort(key=lambda entry: entry["relative_path"].encode("utf-8"))
    if not files and not allow_empty:
        raise SourceTreeError("source tree contains no regular files")
    return files


def source_tree_root_from_entries(entries: list[dict[str, Any]]) -> str:
    if not entries:
        raise SourceTreeError("source tree contains no regular files")
    for entry in entries:
        if not isinstance(entry, dict) or set(entry) != {
            "relative_path",
            "byte_length",
            "sha256_of_raw_bytes",
        }:
            raise SourceTreeError("source tree manifest contains a malformed entry")
        relative = entry["relative_path"]
        length = entry["byte_length"]
        digest = entry["sha256_of_raw_bytes"]
        if not isinstance(relative, str):
            raise SourceTreeError("source tree manifest path is not a string")
        _relative_path(tuple(relative.split("/")))
        if not isinstance(length, int) or isinstance(length, bool) or length < 0:
            raise SourceTreeError("source tree manifest byte length is invalid")
        if (
            not isinstance(digest, str)
            or re.fullmatch(r"[0-9a-f]{64}", digest) is None
            or digest == "0" * 64
        ):
            raise SourceTreeError("source tree manifest digest is invalid")
    ordered = sorted(entries, key=lambda entry: entry["relative_path"].encode("utf-8"))
    if ordered != entries:
        raise SourceTreeError("source tree manifest entries are not in canonical order")
    return tagged_hash("manifest-node", canonical_json(entries))


def source_partition_digest(entries: list[dict[str, Any]]) -> str:
    """Hash one partition with source-tree semantics, admitting an empty support set."""
    if entries:
        return source_tree_root_from_entries(entries)
    return tagged_hash("manifest-node", canonical_json([]))


def build_source_partition_manifest(
    union_manifest: dict[str, Any], target_paths: set[str]
) -> dict[str, Any]:
    files = union_manifest.get("files")
    if not isinstance(files, list) or not files:
        raise SourceTreeError("source partition needs a nonempty union manifest")
    if (
        union_manifest.get("schema") != SOURCE_TREE_MANIFEST_SCHEMA
        or union_manifest.get("source_tree_sha256") != source_tree_root_from_entries(files)
    ):
        raise SourceTreeError("source partition has an invalid union manifest")
    all_paths = {entry["relative_path"] for entry in files}
    if not target_paths or not target_paths <= all_paths:
        raise SourceTreeError("target partition is empty or escapes the source union")
    rows = [
        {
            "relative_path": entry["relative_path"],
            "origin": "target" if entry["relative_path"] in target_paths else "supporting",
        }
        for entry in files
    ]
    target_entries = [entry for entry in files if entry["relative_path"] in target_paths]
    supporting_entries = [entry for entry in files if entry["relative_path"] not in target_paths]
    manifest: dict[str, Any] = {
        "schema": SOURCE_PARTITION_MANIFEST_SCHEMA,
        "files": rows,
        "target_source_tree_sha256": source_partition_digest(target_entries),
        "supporting_source_tree_sha256": source_partition_digest(supporting_entries),
        "union_source_tree_sha256": union_manifest["source_tree_sha256"],
    }
    manifest["partition_manifest_sha256"] = tagged_hash(
        "raw-artifact", canonical_json(manifest)
    )
    return manifest


def validate_source_partition_manifest(
    manifest: dict[str, Any], union_manifest: dict[str, Any]
) -> None:
    if set(manifest) != {
        "schema", "files", "target_source_tree_sha256",
        "supporting_source_tree_sha256", "union_source_tree_sha256",
        "partition_manifest_sha256",
    } or manifest.get("schema") != SOURCE_PARTITION_MANIFEST_SCHEMA:
        raise SourceTreeError("source partition manifest has missing/unknown fields or wrong schema")
    files = manifest.get("files")
    union_files = union_manifest.get("files")
    if not isinstance(files, list) or not isinstance(union_files, list) or len(files) != len(union_files):
        raise SourceTreeError("source partition manifest does not cover the union")
    target_paths: set[str] = set()
    seen: set[str] = set()
    for row, source in zip(files, union_files):
        if not isinstance(row, dict) or set(row) != {"relative_path", "origin"}:
            raise SourceTreeError("source partition manifest contains a malformed row")
        if row["relative_path"] != source.get("relative_path") or row["relative_path"] in seen:
            raise SourceTreeError("source partition rows are not canonical or do not match the union")
        seen.add(row["relative_path"])
        if row["origin"] not in {"target", "supporting"}:
            raise SourceTreeError("source partition row has an invalid origin")
        if row["origin"] == "target":
            target_paths.add(row["relative_path"])
    expected = build_source_partition_manifest(union_manifest, target_paths)
    if manifest != expected:
        raise SourceTreeError("source partition manifest digests do not match the union")


def source_tree_manifest(root: Path, *, excluded_names=(), allow_empty=False) -> dict[str, Any]:
    files = source_tree_entries(root, excluded_names=excluded_names, allow_empty=allow_empty)
    return {
        "schema": SOURCE_TREE_MANIFEST_SCHEMA,
        "files": files,
        "source_tree_sha256": source_partition_digest(files) if allow_empty else source_tree_root_from_entries(files),
    }


def source_tree_digest(root: Path) -> str:
    return source_tree_manifest(root)["source_tree_sha256"]


def git_checkout_digest(root: Path, expected_revision: str | None = None) -> str:
    from trellis.checker.admission import cooperative
    return cooperative(_git_checkout_digest)(root, expected_revision)


def _git_checkout_digest(root: Path, expected_revision: str | None = None) -> str:
    """SHA-256 identity of a clean Git checkout, including tracked symlinks.

    PV source trees intentionally reject links. Tool source checkouts cannot:
    upstream Charon and Aeneas contain tracked symlinks. Their separate
    closure therefore hashes the indexed mode, path, and live raw blob bytes,
    and binds those rows to the exact HEAD revision.
    """

    root = Path(root).resolve(strict=True)

    def git(*args: str) -> bytes:
        completed = subprocess.run(
            ["git", "-C", str(root), *args], capture_output=True, check=False
        )
        if completed.returncode != 0:
            raise SourceTreeError(
                f"cannot inspect Git checkout {root}: "
                f"{completed.stderr.decode('utf-8', errors='replace').strip()}"
            )
        return completed.stdout

    top = Path(git("rev-parse", "--show-toplevel").decode("utf-8").strip()).resolve()
    if top != root:
        raise SourceTreeError(f"Git checkout root is not exact: {root}")
    revision = git("rev-parse", "HEAD").decode("ascii").strip()
    if re.fullmatch(r"[0-9a-f]{40}", revision) is None:
        raise SourceTreeError("Git checkout HEAD is not a full revision")
    if expected_revision is not None and revision != expected_revision:
        raise SourceTreeError("Git checkout revision drifted")
    if git("status", "--porcelain", "--untracked-files=all"):
        raise SourceTreeError(f"Git checkout is not clean: {root}")

    rows: list[dict[str, Any]] = []
    for record in git("ls-files", "-z", "--stage").split(b"\0"):
        if not record:
            continue
        try:
            prefix, raw_relative = record.split(b"\t", 1)
            mode, _object_id, stage = prefix.split(b" ")
            relative = raw_relative.decode("utf-8")
        except (ValueError, UnicodeDecodeError) as exc:
            raise SourceTreeError("Git checkout index row is malformed") from exc
        _relative_path(tuple(relative.split("/")))
        if stage != b"0" or mode not in {b"100644", b"100755", b"120000"}:
            raise SourceTreeError("Git checkout has an unsupported index mode or stage")
        path_bytes = os.fsencode(root) + b"/" + raw_relative
        if mode == b"120000":
            raw = os.readlink(path_bytes)
            if not isinstance(raw, bytes):  # pragma: no cover - bytes input is bytes output
                raw = os.fsencode(raw)
            length, content_digest = len(raw), hashlib.sha256(raw).hexdigest()
        else:
            metadata = os.lstat(path_bytes)
            if not stat.S_ISREG(metadata.st_mode):
                raise SourceTreeError("Git checkout file type differs from its index mode")
            descriptor = os.open(path_bytes, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
            try:
                with os.fdopen(descriptor, "rb", closefd=False) as stream:
                    length, content_digest = _stream_digest(stream)
            finally:
                os.close(descriptor)
        rows.append(
            {
                "byte_length": length,
                "git_mode": mode.decode("ascii"),
                "relative_path": relative,
                "sha256_of_raw_bytes": content_digest,
            }
        )
    rows.sort(key=lambda row: row["relative_path"].encode("utf-8"))
    if not rows:
        raise SourceTreeError("Git checkout contains no tracked files")
    return tagged_hash(
        "git-checkout-root",
        canonical_json({"files": rows, "revision": revision}),
    )


def readonly_closure_digest(root: Path) -> str:
    from trellis.checker.admission import cooperative
    return cooperative(_readonly_closure_digest)(root)


def _readonly_closure_digest(root: Path) -> str:
    """Hash every file and link byte in a read-only dependency/cache root."""

    root = Path(root)
    metadata = os.lstat(root)
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        raise SourceTreeError("read-only closure root is not a non-symlink directory")
    rows: list[dict[str, Any]] = []

    def walk(directory: Path, parts: tuple[str, ...]) -> None:
        children = list(os.scandir(directory))
        children.sort(key=lambda child: os.fsencode(child.name))
        for child in children:
            child_parts = (*parts, child.name)
            relative = _relative_path(child_parts)
            item = child.stat(follow_symlinks=False)
            if stat.S_ISDIR(item.st_mode):
                walk(Path(child.path), child_parts)
                continue
            if stat.S_ISLNK(item.st_mode):
                raw = os.readlink(os.fsencode(child.path))
                if not isinstance(raw, bytes):  # pragma: no cover
                    raw = os.fsencode(raw)
                length, content_digest = len(raw), hashlib.sha256(raw).hexdigest()
                entry_type = "symlink"
            elif stat.S_ISREG(item.st_mode):
                descriptor = os.open(
                    child.path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
                )
                try:
                    opened = os.fstat(descriptor)
                    if (
                        not stat.S_ISREG(opened.st_mode)
                        or opened.st_dev != item.st_dev
                        or opened.st_ino != item.st_ino
                    ):
                        raise SourceTreeError(
                            f"read-only closure member changed while hashing: {relative}"
                        )
                    with os.fdopen(descriptor, "rb", closefd=False) as stream:
                        length, content_digest = _stream_digest(stream)
                finally:
                    os.close(descriptor)
                if length != opened.st_size:
                    raise SourceTreeError(
                        f"read-only closure member changed length: {relative}"
                    )
                entry_type = "file"
            else:
                raise SourceTreeError(
                    f"read-only closure contains a special member: {relative}"
                )
            rows.append(
                {
                    "byte_length": length,
                    "entry_type": entry_type,
                    "relative_path": relative,
                    "sha256_of_raw_bytes": content_digest,
                }
            )

    walk(root, ())
    rows.sort(key=lambda row: row["relative_path"].encode("utf-8"))
    if not rows:
        raise SourceTreeError("read-only closure contains no files or links")
    return tagged_hash("readonly-closure-root", canonical_json(rows))
