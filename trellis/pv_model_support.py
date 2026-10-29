"""Compose machine extraction with ordinary paired model-definition nodes.

This module checks identities and materializes imports/typed aliases. It never
grants semantic approval or changes the raw extraction or generated functions.
"""
from __future__ import annotations

from collections.abc import Collection, Mapping, Sequence
import hashlib
import json
from pathlib import Path
import re
import stat

from trellis.modeling.modules import imports
from trellis.pv_lean_resources import prepare_generated_lean


SCHEMA = "trellis-model-support/v1"
MANIFEST_PATH = "Tablet/model-support.json"
TYPE_LAYOUT = "extracted-types/v1"
_NODE = re.compile(r"[A-Za-z_][A-Za-z0-9_']*\Z")
_IDENTIFIER = re.compile(r"[A-Za-z_][A-Za-z0-9_']*(?:\.[A-Za-z_][A-Za-z0-9_']*)*\Z")
_BINDING_FIELDS = {"name", "extracted_signature", "rust_symbol", "raw_declaration_sha256", "node", "declaration"}


def empty_model_support() -> dict:
    return {"schema": SCHEMA, "imports": [], "bindings": []}


def _sha(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _read_regular(path: Path) -> str:
    if not stat.S_ISREG(path.lstat().st_mode):
        raise ValueError(f"Model support requires a regular file: {path}")
    return path.read_text(encoding="utf-8")


def _node_name(value: object) -> str:
    if not isinstance(value, str) or not _NODE.fullmatch(value) or value in {"Preamble", "Assumptions", "Axioms"}:
        raise ValueError(f"Expected an ordinary definition-node ID, got {value!r}")
    return value


def _node_pair(repo: Path, node: str, *, require_definition: bool = True) -> tuple[str, str, str]:
    """Read a candidate pair and its principal declared name.

    This is structural discovery before initialization, not FILESPEC acceptance.
    The shared kernel still validates node shape, types, and correspondence.
    """
    from scripts.extract_pv_model import _strip_lean_comments
    from trellis.lean_declarations import declaration_heads

    _node_name(node)
    lean = _read_regular(repo / "Tablet" / f"{node}.lean")
    tex = _read_regular(repo / "Tablet" / f"{node}.tex")
    marker = re.compile(r"(?m)^\s*--\s*\[TABLET NODE:\s*" + re.escape(node) + r"\s*\]\s*$")
    markers = list(marker.finditer(lean))
    if len(markers) != 1:
        raise ValueError(f"Model support node {node} needs its FILESPEC marker")
    nl = re.sub(r"(?m)(?<!\\)%[^\n]*", "", tex)
    environments = re.findall(r"\\begin\{(definition|theorem|lemma|proposition|axiom)\}", nl)
    if (not environments or f"\\end{{{environments[0]}}}" not in nl
            or (require_definition and environments[0] != "definition")):
        raise ValueError(f"Model support node {node} needs a paired NL {'definition' if require_definition else 'statement'}")
    header = _strip_lean_comments(lean[:markers[0].start()].splitlines())
    declaration = _strip_lean_comments(lean[markers[0].end():].splitlines())
    principal = next(iter(declaration_heads(declaration)), None)
    allowed = {"def", "abbrev", "structure", "inductive", "class"}
    if not require_definition:
        allowed |= {"theorem", "lemma"}
    if principal is None or principal.kind not in allowed:
        raise ValueError(f"Model support node {node} must declare a {'definition' if require_definition else 'definition or theorem'}")
    contexts: list[str | None] = []
    for line in header.splitlines():
        if match := re.fullmatch(r"\s*namespace\s+(\S+)\s*", line):
            contexts.append(match[1])
        elif re.match(r"\s*(?:noncomputable\s+)?section(?:\s|$)", line):
            contexts.append(None)
        elif re.match(r"\s*end(?:\s|$)", line) and contexts:
            contexts.pop()
    name = principal.name
    if name.startswith("_root_."):
        name = name[len("_root_."):]
    else:
        name = ".".join([*(item for item in contexts if item), name])
    if not _IDENTIFIER.fullmatch(name):
        raise ValueError(f"Unsupported model-support declaration name: {name!r}")
    return lean, tex, name


def discover_definition_nodes(repo: Path) -> set[str]:
    """Discover candidate ordinary paired definitions without kernel state."""
    nodes = set()
    for path in sorted((repo / "Tablet").glob("*.lean")):
        try:
            _node_pair(repo, path.stem)
        except (OSError, ValueError):
            continue
        nodes.add(path.stem)
    return nodes


def boundary_inventory(raw_text: str) -> list[dict]:
    """Return exact identities that an authored external binding must match."""
    from scripts.extract_pv_model import split_generated_lean

    return [{key: value for key, value in item.items() if key not in {"prefix_utf8", "type_signature"}}
            for item in split_generated_lean(raw_text).raw_boundary_declarations]


def _validate_manifest(manifest: Mapping) -> tuple[list[str], list[dict]]:
    if not isinstance(manifest, Mapping) or set(manifest) != {"schema", "imports", "bindings"} or manifest["schema"] != SCHEMA:
        raise ValueError(f"Expected {SCHEMA} with imports and bindings")
    roots, bindings = manifest["imports"], manifest["bindings"]
    if not isinstance(roots, list) or not isinstance(bindings, list):
        raise ValueError("Model support imports and bindings must be lists")
    roots = [_node_name(node) for node in roots]
    if len(roots) != len(set(roots)):
        raise ValueError("Duplicate model-support import")
    names = set()
    for binding in bindings:
        if not isinstance(binding, dict) or set(binding) != _BINDING_FIELDS:
            raise ValueError("A model-support binding needs exactly the external identity, node, and declaration")
        if any(not isinstance(value, str) or not value for value in binding.values()):
            raise ValueError("Model-support binding fields must be nonempty strings")
        _node_name(binding["node"])
        if not _IDENTIFIER.fullmatch(binding["declaration"]) or binding["declaration"].startswith("_root_."):
            raise ValueError("Binding declaration must be a fully qualified Lean name without _root_")
        if not re.fullmatch(r"[0-9a-f]{64}", binding["raw_declaration_sha256"]):
            raise ValueError("Binding needs the raw external declaration SHA-256")
        if binding["name"] in names:
            raise ValueError(f"Duplicate model-support binding: {binding['name']}")
        names.add(binding["name"])
    return sorted(set(roots) | {item["node"] for item in bindings}), sorted(bindings, key=lambda item: item["name"])


def _pi_type(signature: str) -> str:
    """Turn an extracted binder telescope and result type into its full Pi type."""
    stack = []
    pairs = {")": "(", "}": "{", "]": "["}
    for index, char in enumerate(signature):
        if char in "({[":
            stack.append(char)
        elif char in ")}]":
            if not stack or stack.pop() != pairs[char]:
                raise ValueError("Unbalanced extracted boundary signature")
        elif char == ":" and not stack:
            telescope, result = signature[:index].strip(), signature[index + 1:].strip()
            if not result or (telescope and telescope[0] not in "({["):
                break
            return f"∀ {telescope}, {result}" if telescope else result
    raise ValueError("External declaration has no supported complete type signature")


def _prerequisite_evidence(repo: Path, roots: list[str], definitions: Collection[str],
                           extracted_types: Mapping[str, str]) -> list[dict]:
    visiting: set[str] = set()
    complete: dict[str, dict] = {}

    def visit(node: str) -> None:
        if node == "Preamble" or node in visiting:
            raise ValueError(f"Model support creates a Preamble import cycle through {node}")
        if node in complete:
            return
        _node_name(node)
        if node in roots and node not in definitions:
            raise ValueError(f"Model-support prerequisite {node} is not an ordinary paired definition")
        if node in extracted_types:
            lean = extracted_types[node]
            record = {"node": node, "classification": "extraction_model", "lean_sha256": _sha(lean)}
        else:
            lean, tex, declaration = _node_pair(repo, node, require_definition=node in definitions)
            record = {"node": node, "declaration": declaration,
                      "lean_sha256": _sha(lean), "nl_sha256": _sha(tex)}
        visiting.add(node)
        dependencies = sorted({module[len("Tablet."):] for module in imports(lean) if module.startswith("Tablet.")})
        for dependency in dependencies:
            visit(dependency)
        visiting.remove(node)
        complete[node] = {**record, "imports": dependencies}

    for node in [*roots, *extracted_types]:
        visit(node)
    return [complete[node] for node in sorted(complete)]


def compose_model_support(
    raw_text: str, manifest: Mapping, *, repo: Path,
    definition_nodes: Collection[str] | None = None,
    lean_imports: Sequence[str] = (),
    expose_types: bool = False,
    _paper_legacy_layout: bool = False,
) -> dict:
    """Derive Preamble and provenance; ordinary checks approve authored meanings."""
    from scripts.extract_pv_model import match_boundary_declaration, split_generated_lean, _node_deps

    roots, bindings = _validate_manifest(manifest)
    definitions = discover_definition_nodes(repo) if definition_nodes is None else set(definition_nodes)
    if isinstance(lean_imports, str) or any(not isinstance(module, str) or not _IDENTIFIER.fullmatch(module)
                                          or module.startswith("Tablet.") for module in lean_imports):
        raise ValueError("Configured Lean imports must be library module names")
    selected_imports = list(dict.fromkeys(lean_imports))
    split = split_generated_lean(raw_text, expose_types=expose_types)
    if _paper_legacy_layout and split.validity_definitions:
        raise ValueError("Paper legacy packaging cannot omit extracted validity definitions")
    # Foundation imports may supply names missing from the extraction, such as
    # unsupported scalar carriers. Add only the ones each extracted type uses;
    # importing every binding here would recreate the cycle this layout avoids.
    owners = {}
    for node in manifest["imports"]:
        if node not in definitions:
            raise ValueError(f"Model-support prerequisite {node} is not an ordinary paired definition")
        _, _, declaration = _node_pair(repo, node)
        owners[declaration] = node
        if declaration.startswith(split.namespace + "."):
            owners[declaration[len(split.namespace) + 1:]] = node
    extracted_types = {
        node.name: prepare_generated_lean(node.text, imports=[
            *selected_imports,
            *(f"Tablet.{dep}" for dep in _node_deps(node.region.splitlines(), node.name, owners))])
        for node in split.nodes if node.name in split.type_nodes
    }
    # Native generated modules have one deterministic elaboration context.
    # Checking only the region after TABLET NODE would allow local notation or
    # macros to reinterpret byte-identical declarations. Authored meanings enter
    # through the support manifest and its ordinary paired nodes instead.
    extracted_modules = {
        node.name: (extracted_types[node.name] if node.name in extracted_types
                    else ("" if _paper_legacy_layout else "import Tablet.Assumptions\n") + node.text)
        for node in split.nodes
    } if expose_types else {}
    prerequisites = _prerequisite_evidence(repo, roots, definitions, extracted_types)
    by_node = {item["node"]: item for item in prerequisites}
    preamble = split.preamble_text
    by_name: dict[str, list[dict]] = {}
    for identity in split.raw_boundary_declarations:
        by_name.setdefault(identity["name"], []).append(identity)
    evidence = []
    for binding in bindings:
        matches = [identity for identity in by_name.get(binding["name"], [])
                   if match_boundary_declaration(identity, **{key: binding[key] for key in
                       ("name", "extracted_signature", "rust_symbol", "raw_declaration_sha256")})]
        if len(matches) != 1:
            raise ValueError(f"Binding does not match one exact raw external declaration: {binding['name']}")
        identity = matches[0]
        if by_node[binding["node"]]["declaration"] != binding["declaration"]:
            raise ValueError(f"Binding implementation is not the principal definition of {binding['node']}")
        if preamble.count(identity["source_utf8"]) != 1:
            raise ValueError(f"External declaration is not unique in generated Preamble: {binding['name']}")
        alias = (identity["prefix_utf8"] + f"def {identity['name']} : "
                 + _pi_type(identity["extracted_signature"]) + f" := _root_.{binding['declaration']}\n")
        preamble = preamble.replace(identity["source_utf8"], alias, 1)
        evidence.append({**binding, "classification": "authored-model-binding",
                         "alias_sha256": _sha(alias), "implementation": by_node[binding["node"]]})
    extra_imports = [*(f"import {module}\n" for module in selected_imports),
                     *(f"import Tablet.{node}\n" for node in roots)]
    preamble = prepare_generated_lean("".join(extra_imports) + preamble)
    receipt = {"schema": SCHEMA, "raw_extraction_sha256": _sha(raw_text),
               "preamble_sha256": _sha(preamble), "lean_imports": selected_imports,
               "imports": roots, "bindings": evidence, "definitions": prerequisites,
               "manifest_sha256": _sha(json.dumps(manifest, sort_keys=True, separators=(",", ":")))}
    if expose_types:
        receipt["extracted_modules"] = {name: _sha(text) for name, text in extracted_modules.items()}
    return {"preamble": preamble, "extracted_types": extracted_types,
            "extracted_modules": extracted_modules, "evidence": receipt}


def sync_model_support(
    repo: Path, *, raw_text: str | None = None, manifest: Mapping | None = None,
    definition_nodes: Collection[str] | None = None, lean_imports: Sequence[str] | None = None,
    write: bool = False,
    _paper_legacy_layout: bool = False,
) -> dict:
    """Verify or regenerate deterministic packaging without editing raw declarations."""
    if raw_text is None:
        raw_text = _read_regular(repo / "generated.lean")
    if manifest is None:
        path = repo / MANIFEST_PATH
        manifest = json.loads(_read_regular(path)) if path.exists() else empty_model_support()
    inputs = repo / "modeling-input/manifest.json"
    native = json.loads(_read_regular(inputs)) if inputs.exists() else {}
    config_path = repo / "trellis.config.json"
    config = json.loads(_read_regular(config_path)) if config_path.exists() else {}
    # The private flag is only for deterministic preparation before the paper
    # manifest is frozen. Later checks derive layout from the admitted config;
    # changing it therefore changes the existing configuration fingerprint.
    paper = config.get("paper_implementation")
    admitted_paper_route = (isinstance(paper, dict) and paper.get("version") == 1
                            and paper.get("route") == "rust_aeneas")
    paper_legacy = (admitted_paper_route or _paper_legacy_layout) and config.get("trust_base") is None
    if paper_legacy and any((repo / "Tablet" / name).exists() or (repo / "Tablet" / name).is_symlink()
                            for name in ("Assumptions.lean", "Assumptions.tex")):
        raise ValueError("Paper legacy packaging has unexpected assumptions support; prepare a fresh generation")
    if lean_imports is None:
        lean_imports = native.get("lean_imports", [])
    result = compose_model_support(raw_text, manifest, repo=repo,
                                   definition_nodes=definition_nodes, lean_imports=lean_imports,
                                   expose_types=native.get("model_support_layout") == TYPE_LAYOUT,
                                   _paper_legacy_layout=paper_legacy)
    path = repo / "Tablet/Preamble.lean"
    actual = _read_regular(path) if path.exists() else None
    expected = result.pop("preamble")
    matched = actual == expected
    changed_files = []
    generated = {**result.pop("extracted_types"), **result.pop("extracted_modules")}
    for node, text in generated.items():
        target = repo / "Tablet" / f"{node}.lean"
        if not target.exists() or _read_regular(target) != text:
            changed_files.append(str(target.relative_to(repo)))
            if write:
                target.write_text(text, encoding="utf-8")
    if not matched:
        changed_files.append("Tablet/Preamble.lean")
    if write and not matched:
        path.write_text(expected, encoding="utf-8")
    return {**result, "matches": not changed_files or write, "changed": bool(write and changed_files),
            "mismatched_files": [] if write else changed_files,
            "actual_sha256": _sha(expected) if write else (_sha(actual) if actual is not None else None),
            "expected_sha256": _sha(expected)}
