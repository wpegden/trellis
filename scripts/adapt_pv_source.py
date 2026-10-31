#!/usr/bin/env python3
"""Materialize the target/supporting upload union for PV source adaptation."""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import stat
import sys
from pathlib import Path
from typing import Any, Dict, List

try:
    from scripts.pv_source_tree import (
        SourceTreeError,
        build_source_partition_manifest,
        canonical_json,
        source_tree_manifest,
    )
except ModuleNotFoundError:  # direct script execution
    from pv_source_tree import (  # type: ignore[no-redef]
        SourceTreeError,
        build_source_partition_manifest,
        canonical_json,
        source_tree_manifest,
    )


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 16), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _copy_manifest(
    source: Path,
    out: Path,
    expected: dict[str, Any],
    written: Dict[str, str],
) -> None:
    for entry in expected["files"]:
        relative = Path(*entry["relative_path"].split("/"))
        path = source / relative
        destination = out / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
        try:
            descriptor = os.open(path, flags)
        except OSError as exc:
            raise SystemExit(f"ERROR: cannot copy source file {relative}: {exc}") from exc
        try:
            opened = os.fstat(descriptor)
            if not stat.S_ISREG(opened.st_mode):
                raise SystemExit(f"ERROR: source member is not regular: {relative}")
            with os.fdopen(descriptor, "rb", closefd=False) as stream:
                with destination.open("xb") as output:
                    shutil.copyfileobj(stream, output, length=1024 * 1024)
        finally:
            os.close(descriptor)
        os.chmod(destination, 0o600)
        written[relative.as_posix()] = sha256_file(destination)
    try:
        if source_tree_manifest(source) != expected:
            raise SystemExit("ERROR: source tree changed while it was materialized")
    except SourceTreeError as exc:
        raise SystemExit(f"ERROR: materialized source tree is invalid: {exc}") from exc


def materialize_source_tree(source: Path, out: Path) -> Dict[str, str]:
    """Copy one uploaded regular-file tree without changing any file bytes."""
    try:
        expected = source_tree_manifest(source)
    except SourceTreeError as exc:
        raise SystemExit(f"ERROR: invalid --source-tree: {exc}") from exc
    written: Dict[str, str] = {}
    _copy_manifest(source, out, expected, written)
    try:
        if source_tree_manifest(out) != expected:
            raise SystemExit("ERROR: materialized tree differs from the source-tree identity")
    except SourceTreeError as exc:
        raise SystemExit(f"ERROR: materialized source tree is invalid: {exc}") from exc
    return written


def materialize_partitioned_source_tree(
    target: Path, supporting: Path | None, out: Path
) -> tuple[Dict[str, str], dict[str, Any], dict[str, Any]]:
    try:
        target_manifest = source_tree_manifest(target)
        supporting_manifest = source_tree_manifest(supporting) if supporting else None
    except SourceTreeError as exc:
        raise SystemExit(f"ERROR: invalid uploaded source partition: {exc}") from exc
    target_paths = {entry["relative_path"] for entry in target_manifest["files"]}
    supporting_paths = {
        entry["relative_path"] for entry in (supporting_manifest or {"files": []})["files"]
    }
    overlap = sorted(target_paths & supporting_paths, key=lambda value: value.encode("utf-8"))
    if overlap:
        raise SystemExit(f"ERROR: target and supporting uploads overlap at paths: {overlap!r}")
    written: Dict[str, str] = {}
    _copy_manifest(target, out, target_manifest, written)
    if supporting and supporting_manifest:
        _copy_manifest(supporting, out, supporting_manifest, written)
    try:
        union_manifest = source_tree_manifest(out)
    except SourceTreeError as exc:
        raise SystemExit(f"ERROR: materialized upload union is invalid: {exc}") from exc
    expected_paths = target_paths | supporting_paths
    if {entry["relative_path"] for entry in union_manifest["files"]} != expected_paths:
        raise SystemExit("ERROR: materialized upload union changed its path set")
    return written, union_manifest, build_source_partition_manifest(union_manifest, target_paths)


def main(argv: List[str]) -> int:
    parser = argparse.ArgumentParser(description="Materialize PV target/supporting source uploads.")
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--target-tree", type=Path, help="required upstream target source tree")
    source.add_argument(
        "--source-tree", type=Path,
        help="compatibility form for copying one unpartitioned source tree",
    )
    parser.add_argument("--supporting-tree", type=Path, help="optional upstream supporting tree")
    parser.add_argument(
        "--out", type=Path, required=True, help="output source directory to materialize"
    )
    parser.add_argument("--force", action="store_true", help="replace an existing --out")
    parser.add_argument("--tree-manifest-out", type=Path)
    parser.add_argument("--partition-manifest-out", type=Path)
    args = parser.parse_args(argv)

    output = args.out.resolve()
    if output.exists():
        if not args.force:
            raise SystemExit(f"ERROR: --out already exists: {output} (pass --force to replace)")
        shutil.rmtree(output)
    output.mkdir(parents=True)

    try:
        if args.source_tree:
            if args.supporting_tree or args.tree_manifest_out or args.partition_manifest_out:
                parser.error("--source-tree cannot be combined with partition inputs or outputs")
            written = materialize_source_tree(args.source_tree.absolute(), output)
        else:
            if not args.tree_manifest_out or not args.partition_manifest_out:
                parser.error("--target-tree requires both manifest output paths")
            written, tree_manifest, partition_manifest = materialize_partitioned_source_tree(
                args.target_tree.absolute(),
                args.supporting_tree.absolute() if args.supporting_tree else None,
                output,
            )
            args.tree_manifest_out.write_bytes(canonical_json(tree_manifest))
            args.partition_manifest_out.write_bytes(canonical_json(partition_manifest))
    except BaseException:
        shutil.rmtree(output, ignore_errors=True)
        raise
    print(f"Materialized partitioned source tree at: {output}")
    for relative, digest in written.items():
        print(f"  {digest}  {relative}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
