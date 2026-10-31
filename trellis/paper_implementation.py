"""Paper-native implementation intake and source bindings.

This module checks artifact identity and the supported execution boundary. It
never approves semantics, creates trusted assumptions, or replaces the ordinary
kernel/Paper/Corr/Sound lanes. A manifest records evidence, not approvals.
"""
from __future__ import annotations

import copy
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import time
from typing import Any

SCHEMA = "trellis-paper-implementation/v1"
MANIFEST = "paper-implementation/manifest.json"
ROUTES = {"rust_aeneas", "lean_native"}
PAPER_UPLOAD_LIMIT = 20 * 1024 * 1024
_DIGEST = re.compile(r"[0-9a-f]{64}\Z")
_IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_']*(?:\.[A-Za-z_][A-Za-z0-9_']*)*\Z")
_MANIFEST_FIELDS = {"schema", "route", "paper_path", "paper_files", "files", "targets",
                    "entry_points", "implementation_roots", "modeling_choices", "source_files",
                    "build_files", "evidence_files", "proof_project"}
PROOF_INVENTORY = "paper-implementation/proof-dependencies.json"
MATHLIB_LOCK = "paper-implementation/mathlib-lake-manifest.json"
NATIVE_LAKEFILE = "paper-implementation/native/lakefile.toml"
NATIVE_LOCK = "paper-implementation/native/lake-manifest.json"
MATHLIB_URLS = {
    "mathlib": "https://github.com/leanprover-community/mathlib4.git",
    "batteries": "https://github.com/leanprover-community/batteries",
    "Qq": "https://github.com/leanprover-community/quote4",
    "aesop": "https://github.com/leanprover-community/aesop",
    "proofwidgets": "https://github.com/leanprover-community/ProofWidgets4",
    "importGraph": "https://github.com/leanprover-community/import-graph",
    "LeanSearchClient": "https://github.com/leanprover-community/LeanSearchClient",
    "plausible": "https://github.com/leanprover-community/plausible",
    "Cli": "https://github.com/leanprover/lean4-cli",
}


def canonical_json(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def relative_path(name: str) -> str:
    if not isinstance(name, str) or not name or "\\" in name or "\x00" in name:
        raise ValueError(f"Noncanonical evidence path: {name!r}")
    path = Path(name)
    if path.is_absolute() or path.as_posix() != name or any(part in {".", ".."} for part in path.parts):
        raise ValueError(f"Noncanonical evidence path: {name!r}")
    return name


def evidence_path(repo: Path, name: str) -> Path:
    relative_path(name)
    path = repo / name
    current = path
    while current != repo:
        if current.is_symlink():
            raise ValueError(f"Evidence may not use symlinks: {name}")
        current = current.parent
    if not path.is_file():
        raise ValueError(f"Missing evidence file: {name}")
    return path


def paper_source_text(path: Path) -> str:
    """Read original UTF-8 source without Python's universal-newline conversion."""
    return path.read_bytes().decode("utf-8")


def paper_source_lines(path: Path) -> list[str]:
    """Use native source locators' LF/CRLF convention, preserving other bytes."""
    text = paper_source_text(path)
    if not text:
        return []
    lines = text.split("\n")
    lines[:-1] = [line.removesuffix("\r") for line in lines[:-1]]
    if lines[-1] == "":
        lines.pop()
    return lines


def paper_source_path(name: str) -> str:
    relative_path(name)
    if any(part.lower() in {".git", ".gitattributes"} for part in Path(name).parts):
        raise ValueError(f"Reserved Git control path in paper bundle: {name}")
    return name


def validate_example_inputs(bundle: Path, paper: dict | None = None) -> bool:
    """Admit only the complete, host-produced example reference projection."""
    import os
    import stat
    import tempfile
    from scripts import create_paper_job as intake
    from trellis.agents.engineering_exec import open_regular
    from trellis.agents.host_files import safe_directory

    bundle = Path(os.path.abspath(bundle))
    index_name = intake.REFERENCE_INDEX
    declared = bool(paper and (paper.get('reference_files') or any(
        name == 'paper/' + index_name or name == 'paper/references' or name.startswith('paper/references/')
        for name in paper.get('paper_files', []))))
    with safe_directory(bundle, create=False):
        present = any(os.path.lexists(bundle / name) for name in (index_name, 'references'))
    if not present and not declared:
        return False
    if not all(os.path.lexists(bundle / name) for name in (index_name, 'references')):
        raise ValueError('Engineering example inputs require a complete host reference index')

    def inventory():
        result, directories, total = {}, set(), 0
        def walk(path):
            nonlocal total
            info = path.lstat()
            name = path.relative_to(bundle).as_posix()
            paper_source_path(name)
            if len(result) + len(directories) >= intake.MAX_ENTRIES:
                raise ValueError('Example input inventory exceeds its entry bound')
            if stat.S_ISDIR(info.st_mode):
                directories.add(name)
                with os.scandir(path) as entries:
                    for entry in entries:
                        walk(Path(entry.path))
            elif stat.S_ISREG(info.st_mode) and info.st_nlink == 1:
                with open_regular(path) as stream:
                    raw = stream.read(PAPER_UPLOAD_LIMIT + 1)
                total += len(raw)
                if total > PAPER_UPLOAD_LIMIT:
                    raise ValueError('Example input inventory exceeds its byte bound')
                result[name] = raw
            else:
                raise ValueError('Example inputs require ordinary, unaliased files and directories')
        for name in (index_name, 'references'):
            walk(bundle / name)
        return result, directories

    files, directories = inventory()
    index = json.loads(files[index_name])
    if (not isinstance(index, dict) or index.get('reference_kind') != 'example_inputs'
            or not isinstance(index.get('files'), list) or not 1 <= len(index['files']) <= intake.MAX_ENTRIES):
        raise ValueError('Engineering intake accepts only host-indexed example inputs')
    originals = []
    for row in index['files']:
        if not isinstance(row, dict) or not isinstance(row.get('original_path'), str):
            raise ValueError('Malformed example input index')
        name = row['original_path']
        if not name.startswith('references/') or name not in files:
            raise ValueError('Example input original is missing')
        originals.append((intake.reference_name(name.removeprefix('references/')), files[name]))
    # Regeneration reuses the bounded upload/archive parser, including every
    # extracted member. Exact index bytes also reject duplicate/unknown fields.
    with tempfile.TemporaryDirectory(prefix='trellis-example-inputs-') as temporary:
        temporary = Path(temporary)
        upload = temporary / 'references.bundle'
        header = {'schema': intake.REFERENCE_SCHEMA, 'files': [
            {'name': name, 'byte_length': len(raw)} for name, raw in originals]}
        upload.write_bytes(canonical_json(header) + b'\n' + b''.join(raw for _, raw in originals))
        expected = temporary / 'paper'
        expected.mkdir()
        intake.add_references(expected, upload, index.get('reference_note', ''), reference_kind='example_inputs')
        expected_files = {path.relative_to(expected).as_posix(): path.read_bytes()
                          for path in expected.rglob('*') if path.is_file()}
        expected_dirs = {path.relative_to(expected).as_posix() for path in expected.rglob('*') if path.is_dir()}
        if files != expected_files or directories != expected_dirs or inventory() != (files, directories):
            raise ValueError('Example input inventory differs from its host index')
    if paper is not None:
        names = paper.get('paper_files', [])
        pinned = paper.get('files', {})
        namespace = {'paper/' + name for name in files}
        declared_namespace = {name for name in names if name == 'paper/' + index_name
                              or name == 'paper/references' or name.startswith('paper/references/')}
        if (len(names) != len(set(names)) or declared_namespace != namespace
                or any(pinned.get('paper/' + name) != hashlib.sha256(raw).hexdigest() for name, raw in files.items())
                or {name for name in pinned if name == 'paper/' + index_name or name == 'paper/references'
                    or name.startswith('paper/references/')} != namespace):
            raise ValueError('Example input index is not bound by the original paper inventory')
    return True


def stage_paper_inputs(repo: Path, paper: dict) -> None:
    """Stage every pinned input verbatim, including files hidden by ignore rules."""
    names = paper["paper_files"]
    for name in names:
        paper_source_path(name)
        if sha256(evidence_path(repo, name)) != paper["files"][name]:
            raise ValueError(f"Original paper input changed: {name}")
    subprocess.run(["git", "--literal-pathspecs", "-c", "core.autocrlf=false",
                    "-c", "core.attributesFile=/dev/null", "add", "--force",
                    "--pathspec-from-file=-", "--pathspec-file-nul"],
                   cwd=repo, input=b"".join(name.encode() + b"\0" for name in names),
                   capture_output=True, check=True, timeout=30)
    staged = subprocess.run(["git", "ls-files", "--stage", "-z"], cwd=repo,
                            capture_output=True, check=True, timeout=30).stdout
    entries = {}
    for entry in staged.split(b"\0"):
        if entry:
            metadata, name = entry.split(b"\t", 1)
            mode, oid, stage = metadata.split()
            entries[name.decode()] = (mode, oid, stage)
    for name in names:
        if name not in entries or entries[name][0] not in {b"100644", b"100755"} or entries[name][2] != b"0":
            raise ValueError(f"Paper input is absent from the Git snapshot: {name}")
    blobs = subprocess.run(["git", "cat-file", "--batch"], cwd=repo,
                           input=b"".join(entries[name][1] + b"\n" for name in names),
                           capture_output=True, check=True, timeout=30).stdout
    offset = 0
    for name in names:
        end = blobs.index(b"\n", offset)
        oid, kind, size = blobs[offset:end].split()
        size = int(size)
        data = blobs[end + 1:end + 1 + size]
        if (oid != entries[name][1] or kind != b"blob"
                or hashlib.sha256(data).hexdigest() != paper["files"][name]):
            raise ValueError(f"Git snapshot changed original paper bytes: {name}")
        offset = end + size + 2


def preparation_launch_config(root: Path, config: dict, paper: dict | None = None, *, lean: bool = False) -> dict:
    """Project retained launch options into a preparation without mutating them."""
    config = copy.deepcopy(config)
    if lean:
        for name in ('preparation', 'pv_tablet', 'modeling', 'paper_implementation'):
            config.pop(name, None)
    config['repo_path'] = str(root.resolve())
    if paper is None:
        config['workflow']['paper_tex_path'] = 'GOAL.md'
    else:
        config.pop('goal_file', None)
        config['workflow'].update(paper_tex_path=paper['paper_path'], main_result_targets=workflow_targets(paper))
        config['workflow'].pop('main_result_labels', None)
    return config


def _strings(value: Any, label: str, *, nonempty: bool = True) -> list[str]:
    if not isinstance(value, list) or (nonempty and not value) or any(not isinstance(x, str) or not x.strip() for x in value):
        raise ValueError(f"{label} must be {'a nonempty' if nonempty else 'a'} string list")
    if len(set(value)) != len(value):
        raise ValueError(f"{label} has duplicates")
    return value


def config_block(value: dict) -> dict:
    block = value.get("paper_implementation", value)
    if not isinstance(block, dict) or set(block) != {"version", "route", "manifest"}:
        raise ValueError("paper_implementation requires only version, route and manifest")
    if type(block["version"]) is not int or block["version"] != 1 or block["route"] not in ROUTES:
        raise ValueError("Unsupported paper implementation version or route")
    if block["route"] == "lean_native":
        _validate_lean_trust_config(value)
    relative_path(block["manifest"])
    return block


def _validate_lean_trust_config(config: dict) -> None:
    # A trust_base config block selects the existing PV-only RequiredV1 protocol.
    # Reject it independently of seed availability; never change the profile.
    if "trust_base" in config:
        raise ValueError("Lean-native v1 cannot use trust_base: the existing trellis-trust-v1 protocol supports only program-verification runs")


def paper_digest(manifest: dict) -> str:
    return hashlib.sha256(canonical_json({name: manifest["files"][name]
                                        for name in manifest["paper_files"]})).hexdigest()


def _validate_targets(repo: Path, manifest: dict, *, check_files: bool) -> list[str]:
    targets = manifest.get("targets")
    if not isinstance(targets, list) or not targets:
        raise ValueError("Original paper targets are required")
    ids = []
    for target in targets:
        if not isinstance(target, dict) or set(target) - {
            "id", "source_path", "start_line", "end_line", "selected_text", "excluded_clauses", "kind"
        }:
            raise ValueError("Paper target contains unknown fields")
        if target.get("kind", "semantic") != "semantic":
            raise ValueError("Cost/running-time targets are outside paper implementation v1")
        identity = target.get("id")
        if not isinstance(identity, str) or not identity.strip() or identity in ids:
            raise ValueError("Paper targets need distinct original target IDs")
        ids.append(identity)
        if target.get("source_path") not in manifest["paper_files"]:
            raise ValueError("Target locator is outside the original paper bundle")
        start, end = target.get("start_line"), target.get("end_line")
        if type(start) is not int or type(end) is not int or not 0 < start <= end:
            raise ValueError("Target needs an inclusive, positive original source range")
        selected = target.get("selected_text")
        if not isinstance(selected, str) or not selected.strip():
            raise ValueError("Target needs a verbatim selected paper clause")
        excluded = target.get("excluded_clauses", [])
        if not isinstance(excluded, list):
            raise ValueError("Excluded clauses must be an explicit list")
        for clause in excluded:
            if (not isinstance(clause, dict) or set(clause) != {"text", "reason"}
                    or not all(isinstance(clause[k], str) and clause[k].strip() for k in ("text", "reason"))):
                raise ValueError("Each excluded clause needs verbatim text and a disclosure reason")
            if clause["text"] == selected:
                raise ValueError("The selected clause cannot also be excluded")
        if check_files:
            lines = paper_source_lines(evidence_path(repo, target["source_path"]))
            if end > len(lines):
                raise ValueError("Paper target range exceeds the original source")
            original = "\n".join(lines[start - 1:end])
            if selected not in original or any(c["text"] not in original for c in excluded):
                raise ValueError("Selected/excluded clauses must be verbatim original-paper evidence")
    return ids


def validate_manifest(repo: Path, config_or_block: dict, *, check_files: bool = True) -> dict:
    """Validate a closed, pinned evidence bundle; no semantic approval is inferred."""
    repo = repo.resolve()
    block = config_block(config_or_block)
    path = evidence_path(repo, block["manifest"])
    raw = path.read_bytes()
    manifest = json.loads(raw)
    if not isinstance(manifest, dict) or set(manifest) - _MANIFEST_FIELDS:
        raise ValueError("Paper implementation manifest has unsupported fields")
    if manifest.get("schema") != SCHEMA or manifest.get("route") != block["route"]:
        raise ValueError("Paper implementation manifest schema/route mismatch")
    if "proof_project" in manifest and manifest["route"] != "lean_native":
        raise ValueError("The Mathlib proof project is supported only for Lean-native subjects")
    files = manifest.get("files")
    if not isinstance(files, dict) or not files:
        raise ValueError("Paper implementation evidence file inventory is empty")
    for name, digest in files.items():
        relative_path(name)
        if name == block["manifest"] or not isinstance(digest, str) or not _DIGEST.fullmatch(digest):
            raise ValueError("Invalid file digest or self-referential evidence manifest")
        if check_files and sha256(evidence_path(repo, name)) != digest:
            raise ValueError(f"Paper implementation evidence changed: {name}")
    groups = {}
    for key in ("paper_files", "source_files", "build_files", "evidence_files"):
        groups[key] = _strings(manifest.get(key, []), key, nonempty=key != "evidence_files")
    for name in groups["paper_files"]:
        paper_source_path(name)
    classified = set().union(*map(set, groups.values()))
    if classified != set(files) or sum(map(len, groups.values())) != len(classified):
        raise ValueError("Every evidence file must have exactly one paper/source/build/evidence classification")
    if manifest.get("paper_path") not in groups["paper_files"] or not manifest["paper_path"].endswith(".tex"):
        raise ValueError("The original .tex paper must be part of the pinned bundle")
    ids = _validate_targets(repo, manifest, check_files=check_files)
    entries = manifest.get("entry_points")
    if not isinstance(entries, list) or not entries:
        raise ValueError("Actual implementation entry points are required")
    nodes = set()
    for entry in entries:
        if not isinstance(entry, dict) or set(entry) != {"node_id", "declaration", "source_path"}:
            raise ValueError("Entry point must bind node_id, declaration and source_path")
        if (not all(isinstance(entry[k], str) and _IDENT.fullmatch(entry[k]) for k in ("node_id", "declaration"))
                or entry["node_id"] in nodes or entry["source_path"] not in groups["source_files"]):
            raise ValueError("Invalid or duplicate implementation entry-point binding")
        nodes.add(entry["node_id"])
        if manifest["route"] == "lean_native" and entry["source_path"] != f"Tablet/{entry['node_id']}.lean":
            raise ValueError("Lean-native entry point must be the actual ordinary Tablet definition file")
    roots = manifest.get("implementation_roots")
    if not isinstance(roots, dict) or set(roots) != set(ids):
        raise ValueError("Implementation roots must cover exactly the selected original paper targets")
    for target, roots_for_target in roots.items():
        if not set(_strings(roots_for_target, f"implementation_roots.{target}")) <= nodes:
            raise ValueError("Implementation roots must bind actual entry-point nodes, not ideal specifications")
    choices = manifest.get("modeling_choices", [])
    if not isinstance(choices, list) or (manifest["route"] == "lean_native" and choices):
        raise ValueError("Rust modeling choices must be a list and cannot be transferred to Lean-native")
    seen = set()
    for choice in choices:
        if not isinstance(choice, dict) or set(choice) - {
            "node", "source_locator", "rationale", "evidence_path", "consequential", "route_decision"
        }:
            raise ValueError("Modeling choices contain unknown fields or claimed approvals")
        for key in ("node", "source_locator", "rationale", "evidence_path"):
            if not isinstance(choice.get(key), str) or not choice[key].strip():
                raise ValueError(f"Modeling choice needs {key}")
        if choice["node"] in seen or choice["evidence_path"] not in groups["evidence_files"]:
            raise ValueError("Modeling choice is duplicate or lacks pinned evidence")
        seen.add(choice["node"])
        if type(choice.get("consequential")) is not bool:
            raise ValueError("Modeling choice must explicitly classify consequential source modeling")
        index = choice.get("route_decision")
        if index is not None and (type(index) is not int or index < 0):
            raise ValueError("route_decision must reference an existing kernel lane decision")
    if isinstance(config_or_block.get("workflow"), dict):
        wf = config_or_block["workflow"]
        if wf.get("paper_tex_path") != manifest["paper_path"]:
            raise ValueError("workflow.paper_tex_path must point at the original pinned paper")
        if wf.get("forbidden_keyword_allowlist"):
            raise ValueError("Paper implementation v1 cannot widen the ordinary Lean policy")
        if wf.get("allowed_import_prefixes", ["Mathlib"]) != ["Mathlib"]:
            raise ValueError("Paper implementation v1 preserves the ordinary import policy")
        if "main_result_targets" in wf:
            expected = {item["id"]: (item["start_line"], item["end_line"]) for item in manifest["targets"]}
            actual = wf["main_result_targets"]
            if (not isinstance(actual, list) or len(actual) != len(expected)
                    or any(not isinstance(item, dict) or workflow_target_id(item) not in expected
                           or (item.get("start_line"), item.get("end_line")) != expected[workflow_target_id(item)] for item in actual)
                    or len({workflow_target_id(item) for item in actual}) != len(expected)):
                raise ValueError("Configured paper target locators differ from pinned original evidence")
    if config_or_block.get("goal_file"):
        raise ValueError("Paper implementation has no intermediate prose goal authority")
    if config_or_block.get("pv_tablet", {}).get("verification_targets"):
        raise ValueError("Paper implementation uses paper targets, not PV prose challenge targets")
    for name in ("APPROVED_AXIOMS.json", "tcb_manifest.json"):
        if (repo / name).exists() and name not in files:
            raise ValueError(f"Existing trust basis is not pinned: {name}")
    if check_files and manifest["route"] == "lean_native":
        validate_lean_source_boundary(repo, manifest)
    return {"config": block, "manifest": manifest,
            "manifest_sha256": hashlib.sha256(raw).hexdigest(), "paper_sha256": paper_digest(manifest)}


def validate_lean_source_boundary(repo: Path, manifest: dict) -> None:
    """Conservative admission screen, in addition to mandatory kernel/Lean checks.

    Compilation/refinement is NOT inferred from lexical admission. Only exact
    ordinary Tablet definitions may become the executable implementation.
    """
    from trellis.config import FORBIDDEN_KEYWORDS_DEFAULT
    from scripts.extract_pv_model import _strip_lean_comments
    source_files = set(manifest["source_files"])
    for name in source_files:
        if not name.startswith("Tablet/") or not name.endswith(".lean"):
            raise ValueError("Lean-native project code must use ordinary Tablet definition files")
        text = evidence_path(repo, name).read_text()
        code = _strip_lean_comments(text.splitlines())
        forbidden = [token for token in (*FORBIDDEN_KEYWORDS_DEFAULT, "csimp", "compiler_extern", "export", "__dir__", "__file__", "__line__")
                     if re.search(r"(?<![\w'])" + re.escape(token) + r"(?![\w'])", code)]
        if forbidden:
            raise ValueError(f"Unsupported Lean-native code in {name}: {', '.join(forbidden)}")
        for imported in re.findall(r"(?m)^\s*import\s+([^\n]+)", code):
            for module in imported.split():
                dependency = module.replace(".", "/") + ".lean"
                # Project implementation dependencies must be in the exact body inventory.
                if dependency not in source_files:
                    raise ValueError(f"Lean-native implementation import requires a pinned source body: {module}")
    for entry in manifest["entry_points"]:
        code = _strip_lean_comments(evidence_path(repo, entry["source_path"]).read_text().splitlines())
        if not re.search(r"(?m)^\s*(?:protected\s+)?def\s+" + re.escape(entry["declaration"]) + r"(?=\s|[:({\[])" , code):
            raise ValueError(f"Entry point does not bind a safe total definition: {entry['declaration']}")
    # Implementation modules stay core-only even when the ordinary proof
    # project has an explicitly pinned Mathlib dependency.
    for name in ("lakefile.lean", "Main.lean", "Mathlib.lean"):
        if (repo / name).exists():
            raise ValueError(f"Unsupported Lean-native project build/runtime override: {name}")
    for name in ("Mathlib", "Init", "Lean", "Lake", "Aeneas"):
        if (repo / name).exists():
            raise ValueError(f"Project shadows a trusted Lean library root: {name}")
    nodes = [p["node_id"] for p in manifest["entry_points"]]
    expected = lean_lakefile(nodes)
    if "proof_project" in manifest:
        validate_mathlib_proof_project(repo, manifest)
        expected = lean_proof_lakefile(nodes, manifest["proof_project"]["mathlib_rev"])
    if evidence_path(repo, "lakefile.toml").read_text() != expected:
        raise ValueError("Lean-native executable assembly must use the exact supported Lake configuration")
    if not {"lean-toolchain", "lakefile.toml", "lake-manifest.json"} <= set(manifest["build_files"]):
        raise ValueError("Lean-native compiler/toolchain assembly is not pinned")
    if "proof_project" not in manifest and json.loads(evidence_path(repo, "lake-manifest.json").read_text()) != json.loads(lean_lake_manifest()):
        raise ValueError("Lean-native v1 requires the canonical dependency-free Lake manifest")


def lean_lakefile(nodes: list[str]) -> str:
    roots = ", ".join(json.dumps("Tablet." + node) for node in sorted(nodes))
    return ('name = "paper_implementation"\nversion = "0.1.0"\n'
            'defaultTargets = ["Tablet"]\n\n[[lean_lib]]\nname = "Tablet"\n'
            f'roots = [{roots}]\nglobs = ["Tablet.+"]\n')


def lean_lake_manifest() -> str:
    """Host-owned empty Lake package resolution; ordinary checks cannot create it."""
    return json.dumps({"version": "1.2.0", "packagesDir": ".lake/packages", "packages": [],
                       "name": "paper_implementation", "lakeDir": ".lake", "fixedToolchain": False}, indent=2) + "\n"


def lean_proof_lakefile(nodes: list[str], revision: str) -> str:
    return (lean_lakefile(nodes) + '\n[[require]]\nname = "mathlib"\n'
            f'git = "{MATHLIB_URLS["mathlib"]}"\nrev = "{revision}"\n')


def mathlib_proof_lock(mathlib_lock: dict, revision: str) -> dict:
    """Restrict dependency selection to the existing pinned Mathlib closure."""
    if (not isinstance(mathlib_lock, dict) or mathlib_lock.get("version") != "1.2.0"
            or mathlib_lock.get("packagesDir") != ".lake/packages"
            or mathlib_lock.get("name") != "mathlib" or not isinstance(revision, str)
            or set(mathlib_lock) != {"version", "packagesDir", "packages", "name", "lakeDir", "fixedToolchain"}
            or mathlib_lock.get("lakeDir") != ".lake" or mathlib_lock.get("fixedToolchain") is not False
            or not re.fullmatch(r"[0-9a-f]{40}", revision)):
        raise ValueError("Unsupported pinned Mathlib dependency manifest")
    packages = mathlib_lock.get("packages")
    if not isinstance(packages, list):
        raise ValueError("Mathlib dependency closure must be a package list")
    seen = {"mathlib"}
    for item in packages:
        if (not isinstance(item, dict) or item.get("name") in seen
                or item.get("name") not in MATHLIB_URLS
                or item.get("url") != MATHLIB_URLS[item["name"]]
                or item.get("type") != "git" or item.get("subDir") is not None
                or not isinstance(item.get("rev"), str) or not re.fullmatch(r"[0-9a-f]{40}", item["rev"])
                or item.get("configFile") not in {"lakefile.lean", "lakefile.toml"}
                or item.get("manifestFile") != "lake-manifest.json"
                or not isinstance(item.get("scope"), str) or not isinstance(item.get("inputRev"), str)
                or type(item.get("inherited")) is not bool
                or set(item) - {"url", "type", "subDir", "scope", "rev", "name", "manifestFile", "inputRev", "inherited", "configFile"}):
            raise ValueError("Unsupported dependency outside the pinned Mathlib package closure")
        seen.add(item["name"])
    top = {"url": MATHLIB_URLS["mathlib"], "type": "git", "subDir": None, "scope": "", "rev": revision,
           "name": "mathlib", "manifestFile": "lake-manifest.json", "inputRev": revision,
           "inherited": False, "configFile": "lakefile.lean"}
    result = json.loads(lean_lake_manifest())
    result["packages"] = [top, *[{**item, "inherited": True} for item in sorted(packages, key=lambda p: p["name"])]]
    return result


def _package_source_files(package: Path) -> dict[str, str]:
    """Pin loadable Lean/configuration sources; generated caches keep existing trust."""
    import os
    if package.is_symlink() or not package.is_dir():
        raise ValueError(f"Proof package is missing or aliased: {package}")
    files = {}
    for directory, dirs, names in os.walk(package):
        dirs[:] = sorted(name for name in dirs if name not in {".git", ".lake"})
        if any((Path(directory) / name).is_symlink() for name in dirs):
            raise ValueError("Proof package source directories cannot be aliases")
        for name in sorted(names):
            if not (name.endswith(".lean") or name in {"lean-toolchain", "lake-manifest.json", "lakefile.toml"}):
                continue
            path = Path(directory) / name
            relative = path.relative_to(package).as_posix()
            files[relative] = sha256(evidence_path(package, relative))
    if not files:
        raise ValueError("Proof package source inventory is empty")
    return files


def validate_mathlib_proof_project(repo: Path, manifest: dict) -> None:
    proof = manifest["proof_project"]
    if (not isinstance(proof, dict) or set(proof) != {"kind", "mathlib_rev", "dependency_inventory"}
            or proof.get("kind") != "mathlib" or proof.get("dependency_inventory") != PROOF_INVENTORY):
        raise ValueError("Unsupported Mathlib proof-project configuration")
    required = {NATIVE_LAKEFILE, NATIVE_LOCK, MATHLIB_LOCK, PROOF_INVENTORY}
    if not required <= set(manifest["build_files"]):
        raise ValueError("Mathlib proof/native build separation is not pinned")
    original = json.loads(evidence_path(repo, MATHLIB_LOCK).read_text())
    expected = mathlib_proof_lock(original, proof["mathlib_rev"])
    if json.loads(evidence_path(repo, "lake-manifest.json").read_text()) != expected:
        raise ValueError("Proof project dependency closure differs from the pinned Mathlib lock")
    nodes = [entry["node_id"] for entry in manifest["entry_points"]]
    if (evidence_path(repo, NATIVE_LAKEFILE).read_text() != lean_lakefile(nodes)
            or json.loads(evidence_path(repo, NATIVE_LOCK).read_text()) != json.loads(lean_lake_manifest())):
        raise ValueError("Executable assembly must retain its canonical core-only build")
    inventory = json.loads(evidence_path(repo, PROOF_INVENTORY).read_text())
    pinned = evidence_path(repo, "lean-toolchain").read_text().strip()
    if (not isinstance(inventory, dict) or set(inventory) != {"schema", "toolchain", "packages"}
            or inventory.get("schema") != "trellis-mathlib-proof-dependencies/v1"
            or inventory.get("toolchain") != pinned or not isinstance(inventory.get("packages"), dict)):
        raise ValueError("Proof dependency inventory/toolchain differs from the pinned implementation")
    packages = inventory["packages"]
    if set(packages) != {p["name"] for p in expected["packages"]}:
        raise ValueError("Proof dependency inventory does not match its Mathlib closure")
    packages_root = repo / ".lake/packages"
    if (repo / ".lake/package-overrides.json").exists() or (repo / ".lake/package-overrides.json").is_symlink():
        raise ValueError("Mathlib proof package overrides are unsupported")
    if packages_root.is_symlink() or (repo / ".lake").is_symlink():
        raise ValueError("Proof package roots cannot be aliases")
    if {p.name for p in packages_root.iterdir()} != set(packages):
        raise ValueError("Proof project contains an unselected dependency package")
    for package in expected["packages"]:
        name = package["name"]
        entry = packages[name]
        if (not isinstance(entry, dict) or set(entry) != {"url", "rev", "source_files"}
                or entry["url"] != package["url"] or entry["rev"] != package["rev"]
                or entry["source_files"] != _package_source_files(packages_root / name)):
            raise ValueError(f"Pinned Mathlib package source changed: {name}")
    if (evidence_path(packages_root / "mathlib", "lean-toolchain").read_text().strip() != pinned
            or evidence_path(packages_root / "mathlib", "lake-manifest.json").read_bytes()
               != evidence_path(repo, MATHLIB_LOCK).read_bytes()):
        raise ValueError("Mathlib source lock/toolchain differs from the selected proof project")


def prepare_mathlib_proof_project(repo: Path, seed: Path, nodes: list[str], toolchain: str) -> dict:
    """Use operator-selected, already trusted/prewarmed packages without downloads.

    Package revision/source checks establish identity. Authenticity of the setup
    and compiled dependency cache remains the existing trusted-setup boundary.
    """
    import tempfile
    from trellis.supervisor_workspace import _ensure_supervisor_lake_packages, dependency_cache_invocation
    seed = seed.resolve()
    if (seed / "lean-toolchain").read_text().strip() != toolchain:
        raise ValueError("Mathlib proof project and implementation toolchains must match exactly")
    root_lock = json.loads(evidence_path(seed, "lake-manifest.json").read_text())
    candidates = [p for p in root_lock.get("packages", []) if p.get("name") == "mathlib"]
    if (len(candidates) != 1 or candidates[0].get("url") not in
            {MATHLIB_URLS["mathlib"], MATHLIB_URLS["mathlib"].removesuffix(".git")}):
        raise ValueError("The trusted prewarmed project must pin the supported Mathlib repository")
    revision = candidates[0].get("rev", "")
    mathlib = seed / ".lake/packages/mathlib"
    lock_bytes = evidence_path(mathlib, "lake-manifest.json").read_bytes()
    proof_lock = mathlib_proof_lock(json.loads(lock_bytes), revision)
    # Existing cache admission validates host-prewarmed layout/executable and
    # toolchain. This does not execute the cache or fetch any dependency.
    with tempfile.TemporaryDirectory(prefix="paper-cache-admission-", dir=repo.parent) as temporary:
        dependency_cache_invocation(seed, Path(temporary))
    inventory = {"schema": "trellis-mathlib-proof-dependencies/v1", "toolchain": toolchain, "packages": {}}
    for package in proof_lock["packages"]:
        name = package["name"]
        source = seed / ".lake/packages" / name
        command = ["git", "-C", str(source), "-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"]
        head = subprocess.run([*command, "rev-parse", "HEAD"], capture_output=True, text=True, timeout=15)
        status = subprocess.run([*command, "status", "--porcelain", "--untracked-files=normal"], capture_output=True, text=True, timeout=15)
        if head.returncode or head.stdout.strip() != package["rev"] or status.returncode or status.stdout:
            raise ValueError(f"Mathlib proof dependency is not a clean checkout of its pinned revision: {name}")
        inventory["packages"][name] = {"url": package["url"], "rev": package["rev"], "source_files": _package_source_files(source)}
    # Reuse the protected mirror's package materializer, with a selected view
    # so unrelated seed-project dependencies never enter the proof project.
    with tempfile.TemporaryDirectory(prefix="paper-package-view-", dir=repo.parent) as temporary:
        view = Path(temporary)
        packages = view / ".lake/packages"
        packages.mkdir(parents=True)
        for package in proof_lock["packages"]:
            (packages / package["name"]).symlink_to(seed / ".lake/packages" / package["name"], target_is_directory=True)
        _ensure_supervisor_lake_packages(view, repo)
    for name, text in {
        NATIVE_LAKEFILE: lean_lakefile(nodes), NATIVE_LOCK: lean_lake_manifest(),
        MATHLIB_LOCK: lock_bytes.decode(), PROOF_INVENTORY: json.dumps(inventory, indent=2, sort_keys=True) + "\n",
        "lakefile.toml": lean_proof_lakefile(nodes, revision),
        "lake-manifest.json": json.dumps(proof_lock, indent=2) + "\n",
    }.items():
        path = repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.is_symlink():
            raise ValueError("Proof assembly output cannot be a symlink")
        path.write_text(text)
    _prewarm_mathlib_configuration(repo, seed)
    return {"kind": "mathlib", "mathlib_rev": revision, "dependency_inventory": PROOF_INVENTORY}


def _prewarm_mathlib_configuration(repo: Path, seed: Path) -> None:
    """Compile trusted package configuration for its new dependency indices.

    Lake's configuration cache records package indices, so a seed with another
    dependency graph cannot always be imported directly into the read-only
    checker. This isolated project contains no candidate Tablet sources.
    """
    import os
    import tempfile
    from trellis.modeling.environment import run_source_command

    def config_files(project: Path) -> dict[str, str]:
        result = {}
        for name in sorted(p.name for p in (repo / ".lake/packages").iterdir()):
            directory = project / ".lake/packages" / name / ".lake/config"
            if not directory.exists():
                continue
            for path in sorted(directory.rglob("*")):
                if path.is_symlink():
                    raise ValueError("Prewarmed package configuration cannot contain aliases")
                if path.is_file():
                    result[path.relative_to(project).as_posix()] = sha256(path)
        return result

    seed_before = config_files(seed)
    for name in config_files(repo):
        path = repo / name
        # Lake updates trace files in place. Detach every copied configuration
        # file before invoking it, including files already shared with a mirror.
        fd, temporary = tempfile.mkstemp(prefix=".config-copy-", dir=path.parent)
        os.close(fd)
        try:
            shutil.copy2(path, temporary)
            os.replace(temporary, path)
        finally:
            Path(temporary).unlink(missing_ok=True)
    output = repo / ".trellis/scratch/proof-config-prewarm"
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="proof-config-", dir=repo.parent) as temporary:
        assembly = Path(temporary)
        for name in ("lakefile.toml", "lake-manifest.json", "lean-toolchain"):
            shutil.copy2(evidence_path(repo, name), assembly / name)
        (assembly / ".lake").mkdir()
        (assembly / ".lake/packages").symlink_to((repo / ".lake/packages").resolve(), target_is_directory=True)
        tools = _installed_lean_tools(assembly)
        lake = Path(tools["lean"]).with_name("lake")
        command = [str(lake), "--no-cache", "env", "/usr/bin/true"]
        result = run_source_command(command, cwd=assembly,
                                    env={"PATH": str(lake.parent) + ":/usr/bin:/bin", "ELAN_TOOLCHAIN": tools["lean_toolchain"]},
                                    stdout=output / "config.log", timeout=120)
        seed_after = config_files(seed)
        if seed_before != seed_after:
            raise ValueError("Trusted seed package configuration changed during isolated prewarm")
        if result["exit_code"] or result["timeout"]:
            raise ValueError(f"Isolated Mathlib configuration prewarm failed; inspect {output / 'config.log'}")
        if (assembly / "lake-manifest.json").read_bytes() != (repo / "lake-manifest.json").read_bytes():
            raise ValueError("Mathlib configuration prewarm attempted to change its frozen dependency closure")
    inventory = json.loads((repo / PROOF_INVENTORY).read_text())
    for name, entry in inventory["packages"].items():
        if _package_source_files(repo / ".lake/packages" / name) != entry["source_files"]:
            raise ValueError("Mathlib configuration prewarm changed pinned package source")
    (output / "result.json").write_text(json.dumps({"argv": command, "candidate_sources_present": False,
        "seed_configuration_unchanged": True, "seed_config_files": seed_before,
        "proof_config_files": config_files(repo), "boundary": "Existing trusted package setup; no candidate definitions or proofs are elaborated."}, indent=2) + "\n")


def preparation_targets(config: dict) -> list[dict]:
    return config["paper_input"]["targets"] if "paper_input" in config else config["goals"]


def preparation_referent(root: Path) -> Path:
    config = json.loads((root / "config.json").read_text())
    return root / config["paper_input"]["paper_path"] if "paper_input" in config else root / "GOAL.md"


def preparation_paper_digest(root: Path) -> str:
    config = json.loads((root / "config.json").read_text())
    paper = config.get("paper_input")
    if paper is None:
        return sha256(root / "GOAL.md")
    for name in paper["paper_files"]:
        paper_source_path(name)
        if sha256(evidence_path(root, name)) != paper["files"][name]:
            raise ValueError(f"Original paper input changed: {name}")
    return paper_digest(paper)


def paper_input(bundle: Path, main: str, targets: list[dict], *, route: str,
                entry_points: list[dict], implementation_roots: dict, modeling_choices: list[dict] = ()) -> dict:
    """Create a source-located index, never a generated theorem specification."""
    if route not in ROUTES:
        raise ValueError("Unsupported implementation route")
    relative_path(main)
    files = {}
    for path in sorted(bundle.rglob("*")):
        paper_source_path(path.relative_to(bundle).as_posix())
        if path.is_symlink():
            raise ValueError("Original paper bundle cannot contain symlinks")
        if path.is_file():
            name = "paper/" + path.relative_to(bundle).as_posix()
            files[name] = sha256(path)
    if not files or "paper/" + main not in files:
        raise ValueError("Original main .tex file is absent from paper bundle")
    normalized = []
    for item in targets:
        item = dict(item)
        source = item.get("source_path", main)
        item["source_path"] = source if source.startswith("paper/") else "paper/" + relative_path(source)
        normalized.append(item)
    return {"route": route, "paper_path": "paper/" + main, "paper_files": sorted(files),
            "files": files, "targets": normalized, "entry_points": entry_points,
            "implementation_roots": implementation_roots, "modeling_choices": list(modeling_choices)}


def write_manifest(repo: Path, *, paper: dict, source_files: list[str], build_files: list[str],
                   evidence_files: list[str] = (), modeling_choices: list[dict] = (),
                   proof_project: dict | None = None) -> dict:
    manifest = {"schema": SCHEMA, **{key: paper[key] for key in (
        "route", "paper_path", "paper_files", "targets", "entry_points", "implementation_roots")},
        "source_files": sorted(source_files), "build_files": sorted(build_files),
        "evidence_files": sorted(evidence_files), "modeling_choices": list(modeling_choices)}
    if proof_project is not None:
        manifest["proof_project"] = proof_project
    manifest["files"] = {name: sha256(evidence_path(repo, name)) for name in
                         [*manifest["paper_files"], *source_files, *build_files, *evidence_files]}
    block = {"version": 1, "route": paper["route"], "manifest": MANIFEST}
    path = repo / MANIFEST
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(json.dumps(manifest, indent=2, ensure_ascii=False).encode() + b"\n")
    validate_manifest(repo, block)
    return block


def workflow_target_id(target: dict) -> str:
    """Use math mode's original label or canonical source-range identity."""
    label = target.get("tex_label")
    return label.strip() if isinstance(label, str) and label.strip() else f"lines:{target.get('start_line')}-{target.get('end_line')}"


def workflow_targets(paper: dict) -> list[dict]:
    result = []
    for target in paper["targets"]:
        locator = {"start_line": target["start_line"], "end_line": target["end_line"]}
        if target["id"] != workflow_target_id(locator):
            locator["tex_label"] = target["id"]
        result.append(locator)
    return result


def export_paper_inputs(preparation: Path, repo: Path, native_config: dict, *, source_files: list[str],
                        build_files: list[str], evidence_files: list[str] = (), modeling_choices: list[dict] | None = None) -> dict:
    inputs = json.loads((preparation / "config.json").read_text())
    paper = json.loads(json.dumps(inputs["paper_input"]))
    preparation_paper_digest(preparation)
    if paper["route"] == "rust_aeneas":
        from trellis.runtime.kernel_cli import KernelCliError, run_kernel_cli
        models = native_config.get("pv_tablet", {}).get("extraction_models", [])
        for entry in paper["entry_points"]:
            matches = [model for model in models if model["node"] == entry["node_id"]]
            if not matches:
                raise ValueError(f"Selected Rust implementation root is not an extracted node: {entry['node_id']}")
            if len(matches) != 1:
                raise ValueError(f"Selected Rust implementation root has multiple extracted models: {entry['node_id']}")
            model = matches[0]
            # Resolve the materialized node with the same unique-principal and
            # namespace rules used by strict native admission.
            path = evidence_path(repo, f"Tablet/{entry['node_id']}.lean")
            try:
                result = run_kernel_cli({"action": "resolve_node_principal", "lean_path": str(path),
                                         "node_name": entry["node_id"]})
            except KernelCliError as error:
                raise ValueError(f"Extracted implementation principal: {error}") from error
            if result.get("status") != "resolve_node_principal_ok" or not isinstance(result.get("principal_name"), str) or not result["principal_name"]:
                raise ValueError("Kernel did not resolve an extracted implementation principal")
            entry["declaration"] = result["principal_name"]
            entry["source_path"] = "crate/" + relative_path(model["provenance"]["source_file"])
    if any(target["source_path"] != paper["paper_path"] for target in paper["targets"]):
        raise ValueError("V1's existing target resolver requires targets in the configured main .tex file")
    for name in paper["paper_files"]:
        destination = repo / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(evidence_path(preparation, name), destination)
    for name in ("APPROVED_AXIOMS.json", "tcb_manifest.json"):
        if (repo / name).is_file() and name not in build_files:
            build_files = [*build_files, name]
    block = write_manifest(repo, paper=paper, source_files=source_files, build_files=build_files,
                           evidence_files=evidence_files, modeling_choices=(paper.get("modeling_choices", [])
                                                                         if modeling_choices is None else modeling_choices))
    native_config["paper_implementation"] = block
    native_config.pop("goal_file", None)
    native_config["workflow"].update(paper_tex_path=paper["paper_path"], main_result_targets=workflow_targets(paper))
    native_config["workflow"].pop("main_result_labels", None)
    if "pv_tablet" in native_config:
        native_config["pv_tablet"]["verification_targets"] = []
    return block


def initialize_rust_preparation(root: Path, *, paper_bundle: Path, paper: dict, tools: dict,
                                source: Path | None = None, model: str, effort: str,
                                config_template: Path | None = None,
                                engineering_contract: Path | None = None,
                                implementation_notes: Path | None = None,
                                engineering_protocol: str | None = None) -> None:
    """Start existing Rust preparation, with the original paper as its referent."""
    from trellis.modeling.driver import initialize, REPO
    from trellis.modeling.observations import read_json, write_json
    import tempfile
    if paper["route"] != "rust_aeneas":
        raise ValueError("Rust preparation requires rust_aeneas route")
    if engineering_protocol not in (None, 'native-v1') or engineering_protocol is not None and engineering_contract is None:
        raise ValueError('Native engineering requires a new contract intake')
    if engineering_protocol is not None and config_template is not None:
        from scripts.paper_launch_snapshot import selected_submission_policy
        if selected_submission_policy(read_json(config_template)) is None:
            raise ValueError('Native engineering requires a concrete worker_submission_policy')
    if implementation_notes is not None and engineering_contract is None:
        raise ValueError("Implementation notes require engineering intake with a functional contract")
    if engineering_contract is not None:
        from trellis.modeling.engineering import read_input
        if source is not None or paper.get("modeling_choices"):
            raise ValueError("Engineering intake accepts an original paper and contract with a fresh source scaffold")
        validate_example_inputs(paper_bundle, paper)
        read_input(engineering_contract)
        if implementation_notes is not None:
            read_input(implementation_notes, notes=True)
    if source is not None:
        initialize(root, source, None, tools, model=model, effort=effort,
                   config_template=config_template, paper_input=paper, paper_bundle=paper_bundle)
        return
    root.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="paper-rust-source-", dir=root.parent) as temporary:
        scaffold = Path(temporary)
        (scaffold / "src").mkdir()
        (scaffold / "Cargo.toml").write_text('[package]\nname = "paper_algorithm"\nversion = "0.1.0"\nedition = "2021"\n')
        (scaffold / "src/lib.rs").write_text('// Implement the algorithm from the original paper during preparation.\n')
        initialize(root, scaffold, None, tools, model=model, effort=effort,
                   config_template=config_template, paper_input=paper, paper_bundle=paper_bundle)
    if engineering_contract is not None:
        from trellis.modeling.engineering import initialize as initialize_engineering
        initialize_engineering(root, engineering_contract, implementation_notes)
        if engineering_protocol is not None:
            from trellis.modeling import engineering
            from scripts.paper_launch_snapshot import selected_submission_policy
            inputs = read_json(root / 'config.json')
            roles = read_json(root / 'trellis.config.json')
            if config_template is None:
                # Retain the standalone model bindings and use the shared paper default.
                roles['worker_submission_policy'] = read_json(REPO / 'examples/trellis.paper-implementation.config.json')['worker_submission_policy']
                write_json(root / 'trellis.config.json', roles)
                inputs['launch_config'] = roles
            policy = selected_submission_policy(roles)
            if policy is None:
                raise ValueError('Native engineering requires a concrete worker_submission_policy')
            inputs['engineering_protocol'] = engineering_protocol
            write_json(root / 'config.json', inputs)
            current = engineering.native_initialize(root, policy)
            state_path = root / '.trellis/modeling/state.json'
            write_json(state_path, engineering.native_project(root, read_json(state_path), current))


def initialize_lean_preparation(root: Path, *, paper_bundle: Path, paper: dict, tools: dict,
                                source: Path | None = None, model: str, effort: str,
                                config_template: Path | None = None) -> None:
    """Create an agent-authorable Lean candidate; no source approval is fabricated."""
    from trellis.modeling.driver import REPO
    from trellis.modeling.environment import prepare_environment
    from trellis.modeling.history import initialize_history
    from trellis.modeling.observations import write_json
    from trellis.checking import write_scripts
    if root.exists() or paper["route"] != "lean_native":
        raise ValueError("Lean preparation requires a new directory and lean_native route")
    template = json.loads((config_template or REPO / "examples/trellis.config.json").read_text())
    _validate_lean_trust_config(template)
    if not isinstance(tools.get("lean_toolchain"), str) or not tools["lean_toolchain"].strip():
        raise ValueError("Select an installed Lean toolchain explicitly")
    if not isinstance(tools.get("lean"), str) or not Path(tools["lean"]).is_file():
        raise ValueError("Select the installed Lean executable explicitly")
    root.mkdir(parents=True)
    initialize_history(root)
    shutil.copytree(paper_bundle, root / "paper")
    write_json(root / "paper-targets.json", paper)
    upstream = root / "upstream"
    upstream.mkdir()
    (root / "work/lean/Tablet").mkdir(parents=True)
    if source:
        # Candidate input consists of ordinary source pairs, not executable
        # checker scripts, build caches or arbitrary project configuration.
        if not (source / "Tablet").is_dir() or (source / "Tablet").is_symlink():
            raise ValueError("Initial Lean source must contain an ordinary Tablet directory")
        for path in sorted((source / "Tablet").rglob("*")):
            if path.is_symlink():
                raise ValueError("Initial Lean source cannot contain symlinks")
            if path.is_file():
                name = path.relative_to(source).as_posix()
                if path.suffix not in {".lean", ".tex"}:
                    raise ValueError(f"Unsupported initial Lean project input: {name}")
                evidence_path(source, name)
                for base in (upstream, root / "work/lean"):
                    (base / name).parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(path, base / name)
    candidate = root / "work/lean"
    (candidate / "Tablet").mkdir(exist_ok=True)
    for name in ("Preamble.lean", "Preamble.tex"):
        if not (candidate / "Tablet" / name).exists():
            (candidate / "Tablet" / name).write_text("")
    (candidate / "lean-toolchain").write_text(tools["lean_toolchain"].strip() + "\n")
    (candidate / "lakefile.toml").write_text(lean_lakefile([entry["node_id"] for entry in paper["entry_points"]]))
    (candidate / "lake-manifest.json").write_text(lean_lake_manifest())
    proof_project = None
    if tools.get("mathlib_project"):
        proof_project = prepare_mathlib_proof_project(
            candidate, Path(tools["mathlib_project"]),
            [entry["node_id"] for entry in paper["entry_points"]], tools["lean_toolchain"].strip())
    (root / "work/SETUP.md").write_text("The original paper supplies the intended algorithm. Implementation arguments are reviewed in native theorem stating.\n")
    write_json(root / "work/extraction.json", {})  # Shared tool lookup; no extraction is claimed.
    inputs = {"model": model, "effort": effort, "tools": tools, "paper_input": paper}
    if proof_project is not None:
        inputs["proof_project"] = proof_project
    if config_template:
        inputs["launch_config"] = template
    write_json(root / "config.json", inputs)
    from trellis.modeling.native import launch_template
    template = preparation_launch_config(root, launch_template(inputs), paper, lean=True)
    write_json(root / "trellis.config.json", template)
    write_json(root / ".trellis/modeling/state.json", {
        "verification_protocol": "paper-lean-preparation/v1", "stage": "lean_implementation",
        "status": "active", "sequence": 0, "next": {"role": "worker", "instruction": "Implement the paper algorithm using safe total ordinary Lean definitions and paired NL descriptions."},
        "audits": {}, "history": [], "observation": {}, "feedback": ""})
    preparation_paper_digest(root)
    prepare_environment(root, tools)
    write_scripts(root, root / ".trellis")


def lean_preparation_prompt(root: Path, state: dict, task: dict, artifacts: Any) -> str:
    from trellis.modeling.driver import REPO
    inputs = json.loads((root / "config.json").read_text())
    paper = inputs["paper_input"]
    assignment = dict(task)
    if task["role"] == "audit" and "instruction" in assignment:
        assignment["reviewer_proposal"] = assignment.pop("instruction")
    contracts = {
        "worker": 'Response: {"summary": "implementation changes, fidelity to the paper, and remaining blockers"}. Report unsupported requirements in the summary.',
        "reviewer": 'Response: {"action": "work|audit|fail", "instruction": "specific repair, audit justification, or unsupported requirement"}. Choose audit only after ordinary checking and compilation pass; fail ends this preparation.',
        "audit": 'Response: {"decision": "accept|revise", "reason": "assessment of algorithm fidelity and the checked implementation"}. Accept requires current ordinary checking and compilation to pass. Otherwise revise with concrete reasons; missing semantics cannot be waived.',
    }
    if task["role"] not in contracts:
        raise ValueError("Unsupported Lean preparation prompt role")
    references = (REPO / "trellis/prompt_fragments/paper_implementation/references.md").read_text()
    return ("Prepare an implementation of the original paper's algorithm.\n"
            f"Read {root / paper['paper_path']} and the target locators in {root / 'paper-targets.json'}.\n"
            + references + "\n"
            + f"Source pairs belong under {root / 'work/lean/Tablet'}; follow {REPO / 'FILESPEC.md'}.\n"
            "Use safe, total core Lean definitions and pinned Tablet dependencies under the existing trust policy. V1 excludes cost proofs.\n"
            "Reviewers and auditors assess algorithm fidelity, complete definitions, entry-point binding and ordinary checker/build evidence. Report unsupported requirements.\n"
            "Performance choices must leave the paper's natural-language proof suitable as a template for formalizing its selected targets about the implementation; justify the correspondence and any necessary refinement.\n"
            "After algorithm audit, the performance agent measures the implementation and may request improvements through renewed implementation review.\n"
            "Native lanes develop the mathematical arguments. The single human review follows theorem stating; formal completion requires checked proofs. Compiler/runtime correctness remains trusted.\n"
            f"Entry points and roots: {json.dumps({'entry_points': paper['entry_points'], 'implementation_roots': paper['implementation_roots']})}\n"
            f"Read prior decisions in {root / '.trellis/modeling/state.json'}.\n"
            f"Task/evidence: {json.dumps({'task': assignment, 'observation': state.get('observation', {}), 'feedback': state.get('feedback', ''), 'latest_decision': state.get('history', [])[-1:]})}\n"
            + "The supervisor refreshes ordinary checking and compilation evidence before reviewer and auditor dispatch. Workers report source changes; reviewers use that evidence to assign repairs.\n"
            + contracts[task["role"]] + "\n"
            + f"Write response JSON to {artifacts.raw}, validate with python3 {root / '.trellis/scripts/check.py'} preparation-{'review' if task['role'] == 'reviewer' else task['role']}-result {artifacts.raw}, then create {artifacts.done}.\n")


def _lean_candidate_manifest(root: Path) -> tuple[Path, dict]:
    inputs = json.loads((root / "config.json").read_text())
    candidate = root / "work/lean"
    paper = inputs["paper_input"]
    for name in paper["paper_files"]:
        destination = candidate / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        if not destination.exists():
            shutil.copy2(evidence_path(root, name), destination)
        elif sha256(destination) != paper["files"][name]:
            raise ValueError("The candidate changed original paper evidence")
    # Body inventory covers every project implementation module, not only entry points.
    sources = sorted(p.relative_to(candidate).as_posix() for p in (candidate / "Tablet").rglob("*.lean"))
    if not sources:
        raise ValueError("No Lean implementation definitions have been authored")
    lock = candidate / "lake-manifest.json"
    if lock.is_symlink():
        raise ValueError("Lake manifest may not be a symlink")
    if not lock.exists():
        with lock.open("x") as stream:
            stream.write(lean_lake_manifest())
    build = ["lean-toolchain", "lakefile.toml"]
    for name in ("lake-manifest.json", "APPROVED_AXIOMS.json", "tcb_manifest.json"):
        if (candidate / name).exists():
            build.append(name)
    proof_project = inputs.get("proof_project")
    if proof_project is not None:
        build.extend([NATIVE_LAKEFILE, NATIVE_LOCK, MATHLIB_LOCK, PROOF_INVENTORY])
    block = write_manifest(candidate, paper=paper, source_files=sources, build_files=build,
                           proof_project=proof_project)
    return candidate, block


def _installed_lean_tools(repo: Path) -> dict:
    """Resolve an already installed toolchain; never trigger elan installation."""
    import os
    pinned = evidence_path(repo, "lean-toolchain").read_text().strip()
    installed = subprocess.run(["elan", "toolchain", "list"], text=True, capture_output=True, timeout=15)
    if installed.returncode or pinned not in {line.split(" ", 1)[0] for line in installed.stdout.splitlines()}:
        raise ValueError(f"Lean toolchain is not already installed: {pinned}")
    selected = subprocess.run(["elan", "which", "lean"], cwd=repo,
                              env={**os.environ, "ELAN_TOOLCHAIN": pinned},
                              text=True, capture_output=True, timeout=15)
    if selected.returncode or not Path(selected.stdout.strip()).is_file():
        raise ValueError("Cannot resolve the pinned installed Lean toolchain")
    return {"lean": str(Path(selected.stdout.strip()).resolve()), "lean_toolchain": pinned}


def build_native_subject(repo: Path, *, tools: dict | None = None) -> dict:
    """Compile a frozen Lean-native subject without claiming its source proof.

    The runtime caller MUST first perform ordinary checking/closure. This build
    function checks identity and executable linkage only; it cannot waive that
    prerequisite. Report fields contain no timestamps or elapsed durations.
    """
    import fcntl
    import tempfile
    from trellis.modeling.environment import environment_signature, run_source_command
    from trellis.pv_execution import execution_timeout
    repo = repo.resolve()
    config_file = repo / "trellis.config.json"
    config = json.loads(config_file.read_text()) if config_file.exists() else {
        "version": 1, "route": "lean_native", "manifest": MANIFEST}
    subject = validate_manifest(repo, config)
    if subject["manifest"]["route"] != "lean_native":
        raise ValueError("Native Lean build requires a Lean-native paper subject")
    tools = dict(tools) if tools is not None else _installed_lean_tools(repo)
    pinned = evidence_path(repo, "lean-toolchain").read_text().strip()
    if tools.get("lean_toolchain") != pinned:
        raise ValueError("Compiler selection differs from the pinned Lean toolchain")
    lean = Path(tools["lean"]).resolve()
    lake = lean.with_name("lake")
    if not lean.is_file() or not lake.is_file():
        raise ValueError("The selected installed Lean/Lake executables are unavailable")
    core = lean.parent.parent / "lib/lean"
    resource_identity = environment_signature({"lean": str(lean), "lean_paths": [str(core)]})
    binaries = {name: {"path": str(path), "sha256": sha256(path)} for name in
                ("lean", "lake", "leanc", "clang", "llvm-ar")
                if (path := lean.with_name(name)).is_file()}
    output_rel = ".trellis/scratch/paper-native-build/" + subject["manifest_sha256"]
    output = repo / output_rel
    current = repo
    for part in Path(output_rel).parts:
        current = current / part
        if current.is_symlink():
            raise ValueError("Native build output must not traverse a symlink")
        current.mkdir(exist_ok=True)
    for name in ("build.lock", "build.log", "result.json", "implementation.a"):
        item = output / name
        if item.is_symlink() or (item.exists() and (not item.is_file() or item.stat().st_nlink != 1)):
            raise ValueError(f"Native build output must be an unlinked ordinary file: {name}")
    lock_path = output / "build.lock"
    with lock_path.open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        with tempfile.TemporaryDirectory(prefix="assembly-", dir=output) as temporary:
            assembly = Path(temporary)
            descriptors = ({"lakefile.toml": NATIVE_LAKEFILE, "lake-manifest.json": NATIVE_LOCK}
                           if "proof_project" in subject["manifest"] else
                           {"lakefile.toml": "lakefile.toml", "lake-manifest.json": "lake-manifest.json"})
            assembly_inputs = {name: name for name in [*subject["manifest"]["source_files"], "lean-toolchain"]}
            assembly_inputs.update(descriptors)
            for name, source_name in assembly_inputs.items():
                destination = assembly / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(evidence_path(repo, source_name), destination)
                if sha256(destination) != subject["manifest"]["files"][source_name]:
                    raise ValueError("Lean source changed while assembling compilation inputs")
            env = {"PATH": str(lake.parent) + ":/usr/bin:/bin", "ELAN_TOOLCHAIN": pinned}
            argv = [str(lake), "build", "Tablet:static"]
            completed = run_source_command(argv, cwd=assembly, env=env, stdout=output / "build.log",
                                           timeout=execution_timeout(tools, "lean", default=120))
            if completed["exit_code"] != 0 or completed["timeout"]:
                raise ValueError(f"Compilation of pinned Lean source failed; inspect {output / 'build.log'}")
            libraries = sorted((assembly / ".lake/build/lib").glob("*Tablet.a"))
            if len(libraries) != 1 or libraries[0].stat().st_size == 0:
                raise ValueError("Native build did not produce an implementation static library")
            current = validate_manifest(repo, config)
            if current["manifest_sha256"] != subject["manifest_sha256"]:
                raise ValueError("Lean subject changed during compilation")
            if (any(sha256(Path(item["path"])) != item["sha256"] for item in binaries.values())
                    or environment_signature({"lean": str(lean), "lean_paths": [str(core)]}) != resource_identity):
                raise ValueError("Selected Lean compiler/runtime changed during compilation")
            artifact = output / "implementation.a"
            if artifact.is_symlink():
                raise ValueError("Native artifact destination must not be a symlink")
            shutil.copy2(libraries[0], artifact)
    report = {"schema": "trellis-paper-native-build/v1", "subject_sha256": subject["manifest_sha256"],
              "paper_sha256": subject["paper_sha256"], "route": "lean_native",
              "source_files": {name: subject["manifest"]["files"][name] for name in subject["manifest"]["source_files"]},
              "build_files": {name: subject["manifest"]["files"][name] for name in subject["manifest"]["build_files"]},
              "toolchain": pinned, "compiler_binaries": binaries, "resource_identity": resource_identity,
              "argv": argv, "compiled_library": artifact.relative_to(repo).as_posix(),
              "compiled_library_sha256": sha256(artifact),
              "boundary": "Compiled library identity; source correctness requires ordinary kernel closure; compiler/runtime correctness remains trusted."}
    if "proof_project" in subject["manifest"]:
        report["proof_support"] = {"kind": "mathlib", "mathlib_rev": subject["manifest"]["proof_project"]["mathlib_rev"],
                                   "dependency_inventory_sha256": subject["manifest"]["files"][PROOF_INVENTORY],
                                   "lake_manifest_sha256": subject["manifest"]["files"]["lake-manifest.json"]}
        report["native_build_inputs"] = {
            name: {"source_path": source, "sha256": subject["manifest"]["files"][source]}
            for name, source in {**descriptors, "lean-toolchain": "lean-toolchain"}.items()}
        report["proof_dependency_boundary"] = "Package/cache authenticity inherits existing trusted setup; ordinary proof closure and axiom checks remain required."
    (output / "result.json").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    return report


def _ordinary_lean_check(candidate: Path, output: Path) -> dict:
    """Run the ordinary kernel through a scoped authoritative checker server.

    Preparation has no supervisor yet. Establish the same protected mirror and
    checker service used by native runs; never fall back to host Lake checks.
    The explicit child environment avoids redirecting another caller's checker.
    """
    import os
    import tempfile
    import threading
    from trellis.checker.server import CheckerServer
    from trellis.runtime.kernel_cli import kernel_cli_command
    from trellis.supervisor_workspace import sync_supervisor_workspace

    sync_supervisor_workspace(candidate)
    # AF_UNIX has a short path limit; preparation paths may be deeply nested.
    with tempfile.TemporaryDirectory(prefix="paper-check-", dir="/tmp") as temporary:
        runtime = Path(temporary)
        (runtime / "runtime_metadata.json").write_text(json.dumps({"repo_path": str(candidate.resolve())}))
        server = CheckerServer(runtime, parallelism=1, socket_group_gid=None)
        server.set_expected_peer_uid(os.geteuid())
        server.start()
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            env = os.environ.copy()
            env["TRELLIS_CHECKER_SOCKET"] = str(server.socket_path)
            # This private server binds its own peer uid, not a token belonging
            # to an unrelated parent runtime or agent workspace.
            env.pop("TRELLIS_CHECKER_TOKEN", None)
            payload = {"action": "check_tablet", "repo_path": str(candidate), "node_role": {}}
            result = subprocess.run(kernel_cli_command(), input=json.dumps(payload),
                                    capture_output=True, text=True, env=env, timeout=180)
            (output / "ordinary-check.stderr").write_text(result.stderr)
            if result.returncode:
                raise ValueError(f"Ordinary kernel check failed: {result.stderr or result.stdout}")
            checked = json.loads(result.stdout)
            if not isinstance(checked, dict):
                raise ValueError("Ordinary kernel check returned a non-object response")
            return checked
        finally:
            server.shutdown()
            thread.join(timeout=5)
            if server.log_path.is_file():
                shutil.copyfile(server.log_path, output / "checker-server.log")


def check_lean_preparation(root: Path) -> dict:
    """Run ordinary kernel validation and a bounded native build; never run agents."""
    from trellis.modeling.observations import write_json
    from trellis.checking import write_scripts
    from trellis.modeling.driver import REPO
    inputs = json.loads((root / "config.json").read_text())
    output = root / "observations" / f"lean-check-{time.time_ns()}"
    output.mkdir(parents=True)
    result = {"passed": False, "report": str(output / "check.json"), "error": ""}
    try:
        preparation_paper_digest(root)
        candidate, block = _lean_candidate_manifest(root)
        template_path = root / "trellis.config.json"
        config = json.loads((template_path if template_path.exists() else REPO / "examples/trellis.config.json").read_text())
        config.update(repo_path=str(candidate.resolve()), paper_implementation=block)
        config.pop("goal_file", None)
        config.pop("pv_tablet", None)
        config.pop("modeling", None)
        paper = inputs["paper_input"]
        config["workflow"].update(paper_tex_path=paper["paper_path"], main_result_targets=workflow_targets(paper))
        config["workflow"].pop("main_result_labels", None)
        write_json(candidate / "trellis.config.json", config)
        write_scripts(candidate, candidate / ".trellis")
        subject = validate_manifest(candidate, config)
        # Empty role map is explicit: the handwritten Lean source is ordinary.
        checked = _ordinary_lean_check(candidate, output)
        result["ordinary_check"] = checked
        if checked.get("status") != "check_tablet_ok" or not checked.get("output", {}).get("ok"):
            raise ValueError("Ordinary Lean tablet check did not pass")
        result["build"] = build_native_subject(candidate, tools=inputs["tools"])
        result["subject_sha256"] = subject["manifest_sha256"]
        result["resource_identity"] = result["build"]["resource_identity"]
        result["compiled_library"] = str(candidate / result["build"]["compiled_library"])
        result["compiled_library_sha256"] = result["build"]["compiled_library_sha256"]
        result["passed"] = True
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
        result["error"] = str(error)
    write_json(output / "check.json", result)
    return result


class LeanPreparationDriver:
    """One bounded author/reviewer/auditor step using the existing executor lanes."""
    def __init__(self, root: Path, *, agents=None, checker=None):
        from trellis.modeling.driver import Agents
        self.root = root.resolve()
        self.path = self.root / ".trellis/modeling/state.json"
        self.state = json.loads(self.path.read_text())
        if self.state.get("verification_protocol") != "paper-lean-preparation/v1":
            raise ValueError("Not a Lean-native paper preparation")
        self.agents = agents or Agents()
        self.checker = checker or check_lean_preparation

    def step(self) -> dict:
        from trellis.modeling.observations import write_json
        from trellis.modeling.driver import check_halt
        check_halt(self.root)
        if self.state["status"] != "active":
            raise ValueError("Lean preparation is not active")
        task = self.state["next"]
        if task["role"] == "performance":
            from trellis.modeling.performance_loop import step
            return step(self.root, self.state, self.agents)
        if task["role"] != "worker":
            self.state["observation"] = self.checker(self.root)
        raw = self.agents(self.root, self.state, task)
        role = task["role"]
        if role == "worker":
            if not isinstance(raw.get("summary"), str):
                raise ValueError("Worker must report its preparation changes")
            if raw.get("modeling_review_request") or raw.get("modeling_choice_proposals"):
                raise ValueError("Lean-native v1 cannot introduce manual semantics; use safe source or report unsupported")
            self.state["next"] = {"role": "reviewer"}
        elif role == "reviewer":
            action = raw.get("action")
            if action == "work":
                self.state["next"] = {"role": "worker", "instruction": raw.get("instruction", "")}
            elif action == "audit":
                if not self.state["observation"].get("passed"):
                    raise ValueError("Lean source must pass ordinary checking and build before audit")
                self.state["next"] = {"role": "audit", "instruction": raw.get("instruction", "")}
            elif action == "fail":
                self.state["status"] = "failed"
            else:
                raise ValueError("Unsupported Lean preparation reviewer action")
        elif role == "audit":
            if raw.get("decision") == "accept":
                if not self.state["observation"].get("passed") or raw.get("support_obligations"):
                    raise ValueError("Lean native export cannot waive failed checking or missing semantics")
                self.state["audits"]["lean_implementation"] = {
                    "decision": "accept", "basis": self.state["observation"]["subject_sha256"],
                    "resource_identity": self.state["observation"]["resource_identity"], "reason": raw.get("reason", "")}
                from trellis.modeling.performance_loop import begin
                begin(self.root, self.state)
            elif raw.get("decision") == "revise":
                self.state["feedback"] = raw.get("reason", "")
                self.state["next"] = {"role": "reviewer"}
            else:
                raise ValueError("Unsupported Lean preparation audit disposition")
        else:
            raise ValueError("Unsupported preparation agent role")
        self.state["history"].append({"sequence": self.state["sequence"], "task": task, "response": raw})
        self.state["sequence"] += 1
        write_json(self.path, self.state)
        return self.state


def export_lean_preparation(root: Path, destination: Path, runtime: Path, *, kernel_command: str) -> Path:
    """Freeze checked ordinary definitions, then enter the existing native runtime."""
    from trellis.modeling.observations import write_json
    from trellis.modeling.native import initialize_native_runtime
    from trellis.modeling.driver import REPO
    state = json.loads((root / ".trellis/modeling/state.json").read_text())
    if destination.exists() or runtime.exists() or state.get("status") != "prepared":
        raise ValueError("Export needs an agent-prepared candidate and new repository/runtime paths")
    from trellis.modeling.performance_loop import validate_acceptance, export_evidence
    validate_acceptance(root, state)
    checked = check_lean_preparation(root)
    approved = state["audits"].get("lean_implementation", {})
    if (not checked["passed"] or approved.get("decision") != "accept"
            or approved.get("basis") != checked.get("subject_sha256")
            or approved.get("resource_identity") != checked.get("resource_identity")):
        raise ValueError("Lean preparation approval/check is stale or incomplete")
    candidate = root / "work/lean"
    shutil.copytree(candidate, destination, ignore=shutil.ignore_patterns(".lake", ".git", ".trellis"))
    if "proof_project" in json.loads((candidate / MANIFEST).read_text()):
        from trellis.supervisor_workspace import _ensure_supervisor_lake_packages
        _ensure_supervisor_lake_packages(candidate, destination)
    config = json.loads((root / "trellis.config.json").read_text())
    config.update(repo_path=str(destination.resolve()), paper_implementation={"version": 1, "route": "lean_native", "manifest": MANIFEST})
    config.pop("goal_file", None)
    config.pop("pv_tablet", None)
    config.pop("modeling", None)
    config["workflow"]["start_phase"] = "theorem_stating"
    evidence = destination / "paper-implementation/preparation.json"
    write_json(evidence, {"preparation_root": str(root.resolve()), "audit": approved, "check": checked})
    subject = validate_manifest(destination, config)["manifest"]
    library = destination / "paper-implementation/artifacts/implementation.a"
    library.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(checked["compiled_library"], library)
    if sha256(library) != checked["compiled_library_sha256"]:
        raise ValueError("Compiled implementation library changed during export")
    subject["build_files"].append("paper-implementation/artifacts/implementation.a")
    subject["files"]["paper-implementation/artifacts/implementation.a"] = sha256(library)
    subject["evidence_files"].append("paper-implementation/preparation.json")
    subject["files"]["paper-implementation/preparation.json"] = sha256(evidence)
    for name in export_evidence(root, state, destination):
        subject["evidence_files"].append(name)
        subject["files"][name] = sha256(destination / name)
    (destination / MANIFEST).write_text(json.dumps(subject, indent=2, ensure_ascii=False) + "\n")
    validate_manifest(destination, config)
    write_json(destination / "trellis.config.json", config)
    for name in ("FAITHFULNESS.md", "SOUNDNESS.md", "SUBSTANTIVENESS.md"):
        shutil.copy2(REPO / "trellis/prompt_fragments/canonical" / name, destination / name)
    # Native checkpointing expects a real local repository. This initializes
    # only the newly exported project, never the Trellis source checkout.
    from trellis.git_ops import init_repo, _git
    init_repo(destination)
    _git(destination, "add", "-A")
    stage_paper_inputs(destination, subject)
    _git(destination, "commit", "--quiet", "-m", "Initialize paper-native Lean implementation")
    result = initialize_native_runtime(destination, runtime, kernel_command)
    if result.get("status") != "ok":
        raise ValueError(f"Native Lean initialization failed: {result}")
    return destination / "trellis.config.json"
