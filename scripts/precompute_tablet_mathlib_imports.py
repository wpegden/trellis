#!/usr/bin/env python3
"""Precompute Mathlib imports for a public tablet viewer.

For each `Tablet/<Node>.lean`, this records the `Mathlib.*` modules that
appear as direct import lines anywhere in that node's recursive `Tablet.*`
import closure. This intentionally does not expand Mathlib's own transitive
imports; it is the Mathlib surface explicitly imported by the tablet files.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import re
from pathlib import Path
from typing import Iterable


IMPORT_RE = re.compile(r"^\s*import\s+([A-Za-z0-9_'.]+)\s*$", re.MULTILINE)


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _node_names(repo: Path) -> list[str]:
    tablet = repo / "Tablet"
    if not tablet.is_dir():
        raise SystemExit(f"Tablet directory not found: {tablet}")
    return sorted(p.stem for p in tablet.glob("*.lean"))


def _module_imports_from_lean(lean: str) -> list[str]:
    return sorted(set(IMPORT_RE.findall(lean)))


def _recursive_mathlib_imports(
    seed: str,
    module_imports: dict[str, list[str]],
    node_set: set[str],
) -> list[str]:
    seen_nodes: set[str] = set()
    mathlib: set[str] = set()

    def walk(node: str) -> None:
        if node in seen_nodes or node not in node_set:
            return
        seen_nodes.add(node)
        for module in module_imports.get(node, []):
            if module == "Mathlib" or module.startswith("Mathlib."):
                mathlib.add(module)
                continue
            if module.startswith("Tablet."):
                dep = module.split(".", 1)[1]
                if dep in node_set:
                    walk(dep)

    walk(seed)
    return sorted(mathlib)


def build_payload(repo: Path) -> dict[str, object]:
    node_names = _node_names(repo)
    node_set = set(node_names)
    module_imports = {
        name: _module_imports_from_lean(_read(repo / "Tablet" / f"{name}.lean"))
        for name in node_names
    }
    nodes = {
        name: _recursive_mathlib_imports(name, module_imports, node_set)
        for name in node_names
    }
    return {
        "schema_version": 1,
        "kind": "tablet_recursive_mathlib_imports",
        "source_repo": str(repo),
        "generated_at": _dt.datetime.now(_dt.timezone.utc).isoformat(),
        "description": (
            "For each node, direct Mathlib.* imports appearing in the node's "
            "recursive Tablet.* import closure. Mathlib's own transitive "
            "imports are not expanded."
        ),
        "nodes": nodes,
    }


def main(argv: Iterable[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", help="completed tablet repository")
    parser.add_argument("out", help="output JSON path")
    args = parser.parse_args(list(argv) if argv is not None else None)

    repo = Path(args.repo).expanduser().resolve()
    out = Path(args.out).expanduser().resolve()
    payload = build_payload(repo)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True), encoding="utf-8")
    node_count = len(payload["nodes"]) if isinstance(payload.get("nodes"), dict) else 0
    import_count = len(
        {
            module
            for modules in payload.get("nodes", {}).values()  # type: ignore[union-attr]
            for module in modules
        }
    )
    print(f"wrote {out}")
    print(f"nodes={node_count} mathlib_imports={import_count}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
