"""Preserve extracted modules and compile their import graph."""
from __future__ import annotations

from pathlib import Path
import json
import re
import shutil

from trellis.lean_declarations import declaration_heads, mask_comments


def module_name(path: Path, root: Path) -> str:
    return ".".join(path.relative_to(root).with_suffix("").parts)


def imports(text: str) -> list[str]:
    return [name for line in re.findall(r"(?m)^\s*import\s+([^\n]+)", text)
            for name in line.split("--", 1)[0].split()]


def module_files(root: Path) -> dict[str, Path]:
    return {module_name(p, root): p for p in sorted(root.rglob("*.lean"))
            if not p.stem.endswith("_Template")}


class ModuleDependencyCycle(ValueError):
    def __init__(self, cycle: list[str]):
        self.cycle = cycle
        super().__init__("Cyclic Lean module dependency: " + " -> ".join(cycle))


def order_module_imports(modules: dict[str, list[str]]) -> list[str]:
    """Order actual module imports, retaining a concrete cycle on failure."""
    ordered, complete, visiting = [], set(), []

    def visit(name: str) -> None:
        if name in complete:
            return
        if name in visiting:
            raise ModuleDependencyCycle([*visiting[visiting.index(name):], name])
        visiting.append(name)
        for dependency in modules[name]:
            if dependency in modules:
                visit(dependency)
        visiting.pop()
        complete.add(name)
        ordered.append(name)

    for name in sorted(modules):
        visit(name)
    return ordered


def order_modules(modules: dict[str, Path]) -> list[str]:
    return order_module_imports({name: imports(path.read_text()) for name, path in modules.items()})


def assemble(raw: Path, destination: Path, *, lean_imports: list[str] = ()) -> dict[str, Path]:
    """Prepare extracted modules for checking, retaining raw output separately."""
    from trellis.pv_lean_resources import prepare_generated_lean
    shutil.copytree(raw, destination, ignore=shutil.ignore_patterns("*.llbc", "*.olean", "*.ilean"))
    modules = module_files(destination)
    if not modules:
        raise ValueError("Aeneas produced no Lean modules")
    if "Combined" in modules:
        raise ValueError("Combined is reserved for the model entry module")
    for path in modules.values():
        path.write_text(prepare_generated_lean(path.read_text(), imports=lean_imports))
    (destination / "Combined.lean").write_text("\n".join("import " + name for name in sorted(modules)) + "\n")
    return module_files(destination)


def declaration_index(root: Path) -> dict:
    """Navigation hints from extractor provenance and declaration references.

    Textual references are candidates for inspection, not a semantic dependency
    certificate; acceptance continues to inspect the complete modeling basis.
    """
    declarations = []
    for path in sorted(root.rglob("*.lean")):
        namespace, source, symbol = [], None, None
        text = path.read_text()
        lines = text.splitlines()
        code_lines = mask_comments(text, strings=True).splitlines()
        heads = {head.line: head for head in declaration_heads(text)}
        for number, line in enumerate(lines, 1):
            match = re.search(r"/-- \[([^\]]+)\]", line)
            if match:
                symbol, source = match.group(1), None
            match = re.search(r"Source: '([^']+)', lines ([0-9:]+)-([0-9:]+)", line)
            if match:
                source = {"path": match.group(1), "start": match.group(2), "end": match.group(3)}
            code_line = code_lines[number - 1]
            match = re.match(r"\s*namespace\s+(\S+)", code_line)
            if match:
                namespace.append(match.group(1))
            elif re.match(r"\s*(?:(?:noncomputable\s+)?section|mutual)(?:\s|$)", code_line):
                namespace.append(None)
            elif re.match(r"^end(?:\s|$)", code_line) and namespace:
                namespace.pop()
            head = heads.get(number)
            if head:
                name = (head.name.removeprefix("_root_.") if head.name.startswith("_root_.")
                        else ".".join([*(name for name in namespace if name), head.name]))
                declarations.append({"name": name, "kind": head.kind,
                                     "module": module_name(path, root), "file": str(path), "line": number,
                                     "source_symbol": symbol, "source": source, "template": path.stem.endswith("_Template")})
                source, symbol = None, None
    by_name = {item["name"]: item for item in declarations if not item["template"]}
    for item in declarations:
        if item["template"] and item["name"] in by_name and not by_name[item["name"]]["source"]:
            by_name[item["name"]].update(source=item["source"], source_symbol=item["source_symbol"])
    manifests = []
    for manifest in root.rglob("translation.json"):
        metadata = json.loads(manifest.read_text())
        manifests.append(str(manifest))
        for category in ("functions", "types", "globals", "trait_decls", "trait_impls"):
            for entry in metadata.get(category, []):
                item = by_name.get(entry["lean_name"])
                if item is None:
                    continue
                source = entry.get("source", {})
                item.update(source_symbol=entry["rust_name"], provenance="aeneas",
                            source={"path": source.get("file", ""), "start": source.get("begin_line"), "end": source.get("end_line")})
                item.update({key: entry[key] for key in ("is_local", "is_opaque", "can_fail", "can_diverge", "parent_lean_name") if key in entry})
    by_file: dict[str, list[dict]] = {}
    for declaration in declarations:
        by_file.setdefault(declaration["file"], []).append(declaration)
    names = {item["name"] for item in declarations}
    short_names: dict[str, list[str]] = {}
    for name in names:
        short_names.setdefault(name.rsplit(".", 1)[-1], []).append(name)
    for filename, items in by_file.items():
        lines = Path(filename).read_text().splitlines()
        for index, item in enumerate(items):
            end = items[index + 1]["line"] - 1 if index + 1 < len(items) else len(lines)
            body = "\n".join(lines[item["line"] - 1:end])
            tokens = set(re.findall(r"[A-Za-z_][A-Za-z0-9_'.]*", body))
            references = tokens & names
            namespaces = item["name"].split(".")[:-1]
            for token in tokens - names:
                matches = [".".join([*namespaces[:depth], token]) for depth in range(len(namespaces), 0, -1)]
                resolved = next((name for name in matches if name in names), None)
                if resolved:
                    references.add(resolved)
                elif len(short_names.get(token, [])) == 1:
                    references.add(short_names[token][0])
            item["reference_candidates"] = sorted(references - {item["name"]})
    return {"declarations": declarations, "translation_manifests": manifests,
            "imports": {name: imports(path.read_text()) for name, path in module_files(root).items()}}


def attach_diagnostics(diagnostics: list[dict], declarations: list[dict]) -> None:
    for diagnostic in diagnostics:
        text = diagnostic["text"]
        location = re.search(r"(.+?\.lean):(\d+):(\d+):", text)
        if location:
            filename, line = location.group(1), int(location.group(2))
            candidates = [item for item in declarations if item["file"].endswith(filename)
                          and item["line"] <= line and not item.get("template")]
            if candidates:
                diagnostic["declarations"] = [max(candidates, key=lambda item: item["line"])["name"]]
        details = text + "\n" + diagnostic.get("context", "")
        location = re.search(r"(?m)^\s*Source: '([^']+)', lines (\d+):\d+-(\d+):", details)
        if location:
            filename, first, last = location.group(1), int(location.group(2)), int(location.group(3))
            diagnostic["source"] = {"path": filename, "start": first, "end": last}
        else:
            location = re.search(r"(?m)^\s*-->\s+(.+):(\d+):(\d+)\s*$", details)
            if location:
                filename, line, column = location.group(1), int(location.group(2)), int(location.group(3))
                diagnostic["source"] = {"path": filename, "start": line, "end": line, "column": column}
        location = diagnostic.get("source")
        if location:
            filename, first, last = location["path"], location["start"], location["end"]
            names = []
            for item in declarations:
                source = item.get("source") or {}
                if source.get("path") == filename and source.get("start") is not None:
                    begin = int(str(source["start"]).split(":")[0])
                    end = int(str(source["end"]).split(":")[0])
                    if begin <= first <= end or first <= begin <= last:
                        names.append(item["name"])
            diagnostic["declarations"] = sorted(set(names))
