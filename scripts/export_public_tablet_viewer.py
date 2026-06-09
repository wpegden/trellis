#!/usr/bin/env python3
"""Export a static public viewer for a completed Trellis tablet.

The output is plain static HTTP: an index page, one JSON payload, and small
CSS/JS assets. Semantic closures are computed at export time by running the
same Lean script used by Trellis' checker, so the deployed viewer does not
need Node, PHP logic, or a live .trellis runtime.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import hashlib
import json
import re
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Iterable


ROOT_DIR = Path(__file__).resolve().parents[1]
SEMANTIC_SCRIPT = ROOT_DIR / "scripts" / "lean_semantic_fingerprint.lean"
IMPORT_RE = re.compile(r"^\s*import\s+([A-Za-z0-9_'.]+)\s*$", re.MULTILINE)
TABLET_IMPORT_RE = re.compile(r"^\s*import\s+Tablet\.([A-Za-z0-9_']+)\s*$", re.MULTILINE)
TEX_ENV_RE = re.compile(r"\\begin\{([A-Za-z*]+)\}")
README_TARGET_RE = re.compile(
    r"^\|\s*`(?P<label>[^`]+)`\s*\|\s*`Tablet\.(?P<node>[A-Za-z0-9_']+)`\s*\|\s*(?P<statement>.*?)\s*\|\s*$"
)
# Audit M-2: this list MUST stay in sync with
# `kernel/src/model.rs::CANONICAL_APPROVED_AXIOMS`. Generating it from
# the Rust source would require linking a build script into the Python
# release pipeline — disproportionate for a four-element list. The Rust
# regression test
# `kernel/src/runtime_cli_observations.rs::tests::default_approved_axioms_matches_canonical_constant`
# pins engine and runtime-CLI sides; the Python regression test
# `tests/test_public_release_axioms_consistency.py` pins this copy by
# parsing the Rust source directly.
# TODO(M-2): if a fifth axiom ever lands, update both this list AND
# the Rust constant in the same commit.
DEFAULT_APPROVED_AXIOMS = ["propext", "funext", "Classical.choice", "Quot.sound"]


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def _sha256_file(path: Path) -> str:
    if not path.is_file():
        return ""
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _run_text(cmd: list[str], cwd: Path, timeout_secs: float = 30.0) -> str:
    try:
        proc = subprocess.run(
            cmd,
            cwd=str(cwd),
            text=True,
            capture_output=True,
            timeout=timeout_secs,
            check=False,
        )
    except Exception:
        return ""
    return proc.stdout.strip()


def _git_info(repo: Path) -> dict[str, Any]:
    return {
        "commit": _run_text(["git", "rev-parse", "HEAD"], repo),
        "branch": _run_text(["git", "branch", "--show-current"], repo),
        "remote": _run_text(["git", "remote", "get-url", "origin"], repo),
        "dirty": bool(_run_text(["git", "status", "--porcelain"], repo)),
    }


def _node_names(repo: Path) -> list[str]:
    tablet = repo / "Tablet"
    if not tablet.is_dir():
        raise SystemExit(f"Tablet directory not found: {tablet}")
    return sorted(p.stem for p in tablet.glob("*.lean"))


def _imports_from_lean(lean: str) -> list[str]:
    return sorted(set(TABLET_IMPORT_RE.findall(lean)))


def _module_imports_from_lean(lean: str) -> list[str]:
    return sorted(set(IMPORT_RE.findall(lean)))


def _root_imports(repo: Path, all_nodes: set[str]) -> list[str]:
    tablet_root = repo / "Tablet.lean"
    if not tablet_root.is_file():
        return []
    imports = _imports_from_lean(_read(tablet_root))
    return [name for name in imports if name in all_nodes and name != "Preamble"]


def _split_lean(lean: str) -> tuple[str, str]:
    marker = "\n-- BODY\n"
    if marker in lean:
        before, after = lean.split(marker, 1)
    else:
        before, after = lean, ""
    statement_lines: list[str] = []
    for line in before.splitlines():
        stripped = line.strip()
        if stripped.startswith("import "):
            continue
        if stripped.startswith("-- [TABLET NODE:"):
            continue
        if stripped.startswith("set_option "):
            continue
        if not stripped and not statement_lines:
            continue
        statement_lines.append(line)
    return ("\n".join(statement_lines).strip(), after.strip())


def _split_tex(tex: str) -> tuple[str, str, str]:
    env_match = TEX_ENV_RE.search(tex)
    tex_env = env_match.group(1) if env_match else ""
    proof_match = re.search(r"\\begin\{proof\}", tex)
    if not proof_match:
        return tex.strip(), "", tex_env
    return tex[: proof_match.start()].strip(), tex[proof_match.start() :].strip(), tex_env


def _kind_from_env(tex_env: str, lean_statement: str) -> str:
    env = tex_env.lower()
    if env in {"definition", "notation"}:
        return "definition"
    if re.search(r"\b(def|abbrev|structure|inductive|class)\s+", lean_statement):
        return "definition"
    if env:
        return env
    return "node"


def _title_from_tex_statement(node: str, tex_statement: str) -> str:
    # The public tablets usually do not carry separate titles; keep the stable
    # node name as the title rather than inventing prose.
    return node


def _parse_readme_targets(repo: Path) -> list[dict[str, str]]:
    readme = repo / "README.md"
    if not readme.is_file():
        return []
    targets: list[dict[str, str]] = []
    for line in _read(readme).splitlines():
        match = README_TARGET_RE.match(line)
        if not match:
            continue
        targets.append(
            {
                "label": match.group("label"),
                "node": match.group("node"),
                "statement": match.group("statement").strip(),
            }
        )
    return targets


def _title_from_readme(repo: Path) -> str:
    readme = repo / "README.md"
    if readme.is_file():
        for line in _read(readme).splitlines():
            if line.startswith("# "):
                return line[2:].strip()
    return repo.name


def _parse_payload_closure(seed: str, payload: str, all_nodes: set[str]) -> list[str]:
    closure: set[str] = set()
    for chunk in payload.split("||"):
        chunk = chunk.strip()
        if not chunk.startswith("const|"):
            continue
        after = chunk[len("const|") :]
        name = after.split("|", 1)[0].strip()
        if not name or name == seed:
            continue
        top = name.split(".", 1)[0]
        if top and top in all_nodes and top != seed:
            closure.add(top)
    return sorted(closure)


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


def _load_mathlib_imports_json(path: Path, node_set: set[str]) -> dict[str, list[str]]:
    if not path.is_file():
        raise SystemExit(f"mathlib imports json not found: {path}")
    raw = json.loads(_read(path))
    if isinstance(raw, dict) and isinstance(raw.get("nodes"), dict):
        raw_nodes = raw["nodes"]
    elif isinstance(raw, dict):
        raw_nodes = raw
    else:
        raise SystemExit(f"mathlib imports json has unexpected shape: {path}")

    out: dict[str, list[str]] = {}
    for name, modules in raw_nodes.items():
        if str(name) not in node_set or not isinstance(modules, list):
            continue
        out[str(name)] = sorted(
            {
                str(module)
                for module in modules
                if str(module) == "Mathlib" or str(module).startswith("Mathlib.")
            }
        )
    return out


def _run_semantic_closures(
    repo: Path,
    nodes: list[str],
    timeout_secs: float,
    *,
    batch_size: int,
    sleep_secs: float,
) -> tuple[dict[str, list[str]], dict[str, str]]:
    if not SEMANTIC_SCRIPT.is_file():
        raise SystemExit(f"semantic fingerprint script not found: {SEMANTIC_SCRIPT}")
    if not nodes:
        return {}, {}
    batch_size = max(1, int(batch_size))
    all_nodes = set(nodes)
    closures: dict[str, list[str]] = {name: [] for name in nodes}
    errors: dict[str, str] = {}
    seen: set[str] = set()

    batches = [nodes[i : i + batch_size] for i in range(0, len(nodes), batch_size)]
    for batch_index, batch in enumerate(batches, start=1):
        sys.stderr.write(
            f"semantic closure batch {batch_index}/{len(batches)}: "
            f"{', '.join(batch)}\n"
        )
        sys.stderr.flush()
        cmd = [
            "lake",
            "env",
            "lean",
            "--run",
            str(SEMANTIC_SCRIPT),
            *batch,
        ]
        proc = subprocess.run(
            cmd,
            cwd=str(repo),
            text=True,
            capture_output=True,
            timeout=timeout_secs,
            check=False,
        )
        if proc.returncode != 0:
            sys.stderr.write(proc.stderr)
            raise SystemExit(
                f"semantic closure command failed with exit code {proc.returncode} "
                f"on batch {batch_index}/{len(batches)}"
            )
        for line in proc.stdout.splitlines():
            if line.startswith("FP\t"):
                _, node, payload = line.split("\t", 2)
                seen.add(node)
                closures[node] = _parse_payload_closure(node, payload, all_nodes)
            elif line.startswith("ERR\t"):
                parts = line.split("\t", 2)
                node = parts[1] if len(parts) > 1 else "<unknown>"
                errors[node] = parts[2] if len(parts) > 2 else "unknown semantic closure error"
        if sleep_secs > 0 and batch_index < len(batches):
            time.sleep(sleep_secs)
    for node in nodes:
        if node not in seen and node not in errors:
            errors[node] = "semantic closure was not returned by Lean"
    return closures, errors


def _load_semantic_closures_from_state(
    repo: Path,
    nodes: list[str],
    state_json: Path | None,
) -> tuple[dict[str, list[str]], dict[str, str]] | None:
    state_path = state_json
    if state_path is None:
        candidate = repo / ".trellis-history" / "supervisor_state.json"
        if not candidate.is_file():
            return None
        state_path = candidate
    if not state_path.is_file():
        raise SystemExit(f"semantic state json not found: {state_path}")

    raw = json.loads(_read(state_path))
    state = raw.get("state") if isinstance(raw, dict) else None
    if not isinstance(state, dict):
        raise SystemExit(f"semantic state json has no state object: {state_path}")
    live = state.get("live")
    if not isinstance(live, dict):
        raise SystemExit(f"semantic state json has no state.live object: {state_path}")
    fingerprints = live.get("target_fingerprints")
    if not isinstance(fingerprints, dict):
        raise SystemExit(f"semantic state json has no state.live.target_fingerprints object: {state_path}")

    node_set = set(nodes)
    closures: dict[str, list[str]] = {name: [] for name in nodes}
    errors: dict[str, str] = {}
    for name in nodes:
        value = fingerprints.get(name)
        if not isinstance(value, str) or not value.strip():
            continue
        try:
            payload = json.loads(value)
        except Exception as exc:
            errors[name] = f"could not parse semantic fingerprint from {state_path.name}: {exc}"
            continue
        if not isinstance(payload, dict):
            continue
        deps: set[str] = set()
        raw_deps = payload.get("lean_relevant_dependencies")
        if isinstance(raw_deps, list):
            deps.update(str(dep) for dep in raw_deps)
        raw_desc = payload.get("lean_relevant_definition_descendants")
        if isinstance(raw_desc, dict):
            deps.update(str(dep) for dep in raw_desc)
        closures[name] = sorted(dep for dep in deps if dep in node_set and dep != name)
    return closures, errors


def _github_url(base: str, path: str) -> str:
    if not base:
        return ""
    return f"{base.rstrip('/')}/{path}"


def _lake_packages(repo: Path) -> list[dict[str, str]]:
    manifest = repo / "lake-manifest.json"
    if not manifest.is_file():
        return []
    try:
        raw = json.loads(_read(manifest))
    except Exception:
        return []
    packages = raw.get("packages") if isinstance(raw, dict) else None
    if not isinstance(packages, list):
        return []
    out: list[dict[str, str]] = []
    for package in packages:
        if not isinstance(package, dict):
            continue
        out.append(
            {
                "name": str(package.get("name", "")),
                "scope": str(package.get("scope", "")),
                "url": str(package.get("url", "")),
                "rev": str(package.get("rev", "")),
                "input_rev": str(package.get("inputRev", "")),
                "manifest_file": str(package.get("manifestFile", "")),
                "config_file": str(package.get("configFile", "")),
                "inherited": bool(package.get("inherited", False)),
            }
        )
    return out


def _project_approved_axioms(repo: Path) -> dict[str, Any]:
    approved_path = repo / "APPROVED_AXIOMS.json"
    info: dict[str, Any] = {
        "path": "APPROVED_AXIOMS.json",
        "present": approved_path.is_file(),
        "global": [],
        "nodes": {},
        "empty": True,
        "parse_error": "",
    }
    if not approved_path.is_file():
        return info
    try:
        raw = json.loads(_read(approved_path))
    except Exception as exc:
        info["parse_error"] = str(exc)
        return info
    if isinstance(raw, list):
        info["global"] = sorted({str(item).strip() for item in raw if str(item).strip()})
    elif isinstance(raw, dict):
        global_items = raw.get("global")
        if isinstance(global_items, list):
            info["global"] = sorted({str(item).strip() for item in global_items if str(item).strip()})
        node_items = raw.get("nodes")
        if isinstance(node_items, dict):
            parsed_nodes: dict[str, list[str]] = {}
            for node, items in node_items.items():
                if isinstance(items, list):
                    parsed_nodes[str(node)] = sorted({str(item).strip() for item in items if str(item).strip()})
            info["nodes"] = parsed_nodes
    else:
        info["parse_error"] = "approved axioms file is neither a list nor an object"
    info["empty"] = not info.get("global") and not info.get("nodes")
    return info


def _print_axioms(repo: Path, targets: list[str], timeout_secs: float) -> list[dict[str, Any]]:
    if not targets:
        return []
    probe = "import Tablet\n" + "\n".join(f"#print axioms {target}" for target in targets) + "\n"
    probe_path = repo / ".trellis-viewer-print-axioms.lean"
    probe_path.write_text(probe, encoding="utf-8")
    try:
        proc = subprocess.run(
            ["lake", "env", "lean", str(probe_path)],
            cwd=str(repo),
            text=True,
            capture_output=True,
            timeout=timeout_secs,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        stdout = exc.stdout if isinstance(exc.stdout, str) else ""
        stderr = exc.stderr if isinstance(exc.stderr, str) else ""
        return [
            {
                "declaration": target,
                "stdout": stdout,
                "stderr": stderr,
                "returncode": -1,
                "timed_out": True,
            }
            for target in targets
        ]
    finally:
        try:
            probe_path.unlink()
        except FileNotFoundError:
            pass
    stdout_by_decl: dict[str, str] = {target: "" for target in targets}
    for line in proc.stdout.splitlines():
        match = re.match(r"^'([^']+)' (depends on axioms: \[[^\]]*\]|does not depend on any axioms)$", line.strip())
        if match and match.group(1) in stdout_by_decl:
            stdout_by_decl[match.group(1)] = line.strip()
    return [
        {
            "declaration": target,
            "stdout": stdout_by_decl.get(target, ""),
            "stderr": proc.stderr.strip(),
            "returncode": proc.returncode,
            "timed_out": False,
        }
        for target in targets
    ]


def _toml_string(value: Any) -> str:
    return json.dumps(str(value), ensure_ascii=False)


def _toml_value(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, list):
        return "[" + ", ".join(_toml_value(item) for item in value) + "]"
    return _toml_string(value)


def _build_info_toml(info: dict[str, Any]) -> str:
    source = info.get("source", {})
    trellis = info.get("trellis", {})
    lean = info.get("lean", {})
    axioms = info.get("axioms", {})
    approved = axioms.get("approved_axioms", {})
    lines = [
        "# Trellis public tablet viewer build information",
        f"generated_at = {_toml_value(info.get('generated_at', ''))}",
        "",
        "[source]",
        f"repo_path = {_toml_value(source.get('repo_path', ''))}",
        f"github_base = {_toml_value(source.get('github_base', ''))}",
        f"git_remote = {_toml_value(source.get('git_remote', ''))}",
        f"git_branch = {_toml_value(source.get('git_branch', ''))}",
        f"git_commit = {_toml_value(source.get('git_commit', ''))}",
        f"git_dirty = {_toml_value(source.get('git_dirty', False))}",
        "",
        "[trellis]",
        f"repo_path = {_toml_value(trellis.get('repo_path', ''))}",
        f"git_commit = {_toml_value(trellis.get('git_commit', ''))}",
        f"git_dirty = {_toml_value(trellis.get('git_dirty', False))}",
        f"exporter_sha256 = {_toml_value(trellis.get('exporter_sha256', ''))}",
        f"semantic_fingerprint_script_sha256 = {_toml_value(trellis.get('semantic_fingerprint_script_sha256', ''))}",
        "",
        "[lean]",
        f"toolchain = {_toml_value(lean.get('toolchain', ''))}",
        f"lean_version = {_toml_value(lean.get('lean_version', ''))}",
        f"lake_version = {_toml_value(lean.get('lake_version', ''))}",
        f"lake_manifest_sha256 = {_toml_value(lean.get('lake_manifest_sha256', ''))}",
        f"lakefile_sha256 = {_toml_value(lean.get('lakefile_sha256', ''))}",
        "",
        "[axioms]",
        f"probe_description = {_toml_value(axioms.get('probe_description', ''))}",
        f"default_allowed_axioms = {_toml_value(axioms.get('default_allowed_axioms', []))}",
        f"approved_axioms_path = {_toml_value(approved.get('path', ''))}",
        f"approved_axioms_file_present = {_toml_value(approved.get('present', False))}",
        f"approved_axioms_empty = {_toml_value(approved.get('empty', True))}",
        f"project_global_approved_axioms = {_toml_value(approved.get('global', []))}",
        f"allowed_axioms = {_toml_value(axioms.get('allowed_axioms', []))}",
        "",
    ]
    for package in lean.get("packages", []):
        lines.extend(
            [
                "[[lean.packages]]",
                f"name = {_toml_value(package.get('name', ''))}",
                f"scope = {_toml_value(package.get('scope', ''))}",
                f"url = {_toml_value(package.get('url', ''))}",
                f"rev = {_toml_value(package.get('rev', ''))}",
                f"input_rev = {_toml_value(package.get('input_rev', ''))}",
                f"manifest_file = {_toml_value(package.get('manifest_file', ''))}",
                f"config_file = {_toml_value(package.get('config_file', ''))}",
                f"inherited = {_toml_value(package.get('inherited', False))}",
                "",
            ]
        )
    for probe in axioms.get("print_axioms", []):
        lines.extend(
            [
                "[[axioms.print_axioms]]",
                f"declaration = {_toml_value(probe.get('declaration', ''))}",
                f"returncode = {_toml_value(probe.get('returncode', ''))}",
                f"timed_out = {_toml_value(probe.get('timed_out', False))}",
                f"stdout = {_toml_value(probe.get('stdout', ''))}",
                f"stderr = {_toml_value(probe.get('stderr', ''))}",
                "",
            ]
        )
    return "\n".join(lines).rstrip() + "\n"


def _build_info(repo: Path, github_base: str, targets: list[dict[str, str]], timeout_secs: float) -> dict[str, Any]:
    git = _git_info(repo)
    trellis_git = _git_info(ROOT_DIR)
    approved = _project_approved_axioms(repo)
    allowed_axioms = sorted(set(DEFAULT_APPROVED_AXIOMS) | set(approved.get("global", [])))
    target_names = [target["node"] for target in targets]
    info: dict[str, Any] = {
        "generated_at": _dt.datetime.now(_dt.timezone.utc).isoformat(),
        "source": {
            "repo_path": str(repo),
            "github_base": github_base,
            "git_remote": git.get("remote", ""),
            "git_branch": git.get("branch", ""),
            "git_commit": git.get("commit", ""),
            "git_dirty": git.get("dirty", False),
        },
        "trellis": {
            "repo_path": str(ROOT_DIR),
            "git_commit": trellis_git.get("commit", ""),
            "git_dirty": trellis_git.get("dirty", False),
            "exporter_sha256": _sha256_file(Path(__file__).resolve()),
            "semantic_fingerprint_script_sha256": _sha256_file(SEMANTIC_SCRIPT),
        },
        "lean": {
            "toolchain": _read(repo / "lean-toolchain").strip() if (repo / "lean-toolchain").is_file() else "",
            "lean_version": _run_text(["lake", "env", "lean", "--version"], repo),
            "lake_version": _run_text(["lake", "--version"], repo),
            "lake_manifest_sha256": _sha256_file(repo / "lake-manifest.json"),
            "lakefile_sha256": _sha256_file(repo / "lakefile.lean") or _sha256_file(repo / "lakefile.toml"),
            "packages": _lake_packages(repo),
        },
        "axioms": {
            "probe_description": "The public tablet root module `Tablet` was imported, then `#print axioms` was run for each paper target declaration exported by that root tablet.",
            "default_allowed_axioms": DEFAULT_APPROVED_AXIOMS,
            "approved_axioms": approved,
            "allowed_axioms": allowed_axioms,
            "print_axioms": _print_axioms(repo, target_names, timeout_secs),
        },
    }
    info["toml"] = _build_info_toml(info)
    return info


def _build_payload(
    *,
    repo: Path,
    title: str,
    github_base: str,
    skip_semantic_closure: bool,
    semantic_state_json: Path | None,
    force_lean_semantic: bool,
    timeout_secs: float,
    semantic_batch_size: int,
    semantic_sleep_secs: float,
    mathlib_imports_json: Path | None,
) -> dict[str, Any]:
    node_names = _node_names(repo)
    node_set = set(node_names)
    nodes: dict[str, dict[str, Any]] = {}
    module_imports: dict[str, list[str]] = {}
    precomputed_mathlib_imports = (
        _load_mathlib_imports_json(mathlib_imports_json, node_set)
        if mathlib_imports_json
        else None
    )

    for name in node_names:
        lean_path = repo / "Tablet" / f"{name}.lean"
        tex_path = repo / "Tablet" / f"{name}.tex"
        lean = _read(lean_path)
        tex = _read(tex_path) if tex_path.is_file() else ""
        lean_statement, lean_proof = _split_lean(lean)
        tex_statement, tex_proof, tex_env = _split_tex(tex)
        if precomputed_mathlib_imports is None:
            module_imports[name] = _module_imports_from_lean(lean)
        imports = [imp for imp in _imports_from_lean(lean) if imp in node_set]
        nodes[name] = {
            "name": name,
            "title": _title_from_tex_statement(name, tex_statement),
            "kind": _kind_from_env(tex_env, lean_statement),
            "tex_env": tex_env,
            "imports": imports,
            "imported_by": [],
            "lean_path": f"Tablet/{name}.lean",
            "tex_path": f"Tablet/{name}.tex" if tex_path.is_file() else "",
            "github_lean_url": _github_url(github_base, f"Tablet/{name}.lean"),
            "github_tex_url": _github_url(github_base, f"Tablet/{name}.tex") if tex_path.is_file() else "",
            "lean_statement": lean_statement,
            "lean_proof": lean_proof,
            "tex_statement": tex_statement,
            "tex_proof": tex_proof,
            "semantic_closure": [],
            "semantic_closure_error": "",
            "mathlib_imports": [],
        }

    for name, node in nodes.items():
        for dep in node["imports"]:
            if dep in nodes:
                nodes[dep]["imported_by"].append(name)
    for node in nodes.values():
        node["imported_by"].sort()

    for name in node_names:
        if precomputed_mathlib_imports is not None:
            nodes[name]["mathlib_imports"] = precomputed_mathlib_imports.get(name, [])
        else:
            nodes[name]["mathlib_imports"] = _recursive_mathlib_imports(
                name,
                module_imports,
                node_set,
            )

    if not skip_semantic_closure:
        closure_result = None
        if not force_lean_semantic:
            closure_result = _load_semantic_closures_from_state(repo, node_names, semantic_state_json)
        if closure_result is None:
            closures, closure_errors = _run_semantic_closures(
                repo,
                node_names,
                timeout_secs,
                batch_size=semantic_batch_size,
                sleep_secs=semantic_sleep_secs,
            )
        else:
            closures, closure_errors = closure_result
        for name in node_names:
            nodes[name]["semantic_closure"] = closures.get(name, [])
            nodes[name]["semantic_closure_error"] = closure_errors.get(name, "")

    targets = _parse_readme_targets(repo)
    if targets:
        targets = [target for target in targets if target["node"] in nodes]
    if not targets:
        targets = [{"label": "", "node": name, "statement": ""} for name in _root_imports(repo, node_set)]
    targets.sort(key=lambda target: (target["node"] != "MainTheorem", target["node"]))

    target_nodes = {target["node"] for target in targets}
    for name, node in nodes.items():
        node["is_target"] = name in target_nodes
    build_info = _build_info(repo, github_base, targets, timeout_secs)

    return {
        "schema_version": 1,
        "title": title,
        "generated_at": _dt.datetime.now(_dt.timezone.utc).isoformat(),
        "source_repo": str(repo),
        "github_base": github_base,
        "targets": targets,
        "nodes": nodes,
        "build_info": build_info,
    }


INDEX_HTML = """<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Trellis Tablet Viewer</title>
  <link rel="stylesheet" href="assets/viewer.css">
  <script>
    window.MathJax = {
      loader: { load: ['[tex]/ams', '[tex]/html'] },
      tex: {
        packages: {'[+]': ['ams', 'html']},
        inlineMath: [['\\\\(', '\\\\)'], ['$', '$']],
        displayMath: [['\\\\[', '\\\\]'], ['$$', '$$']],
        processEnvironments: false,
        processEscapes: true,
        macros: { noderef: ['\\\\href{#1}{\\\\operatorname{#1}}', 1] }
      },
      options: { skipHtmlTags: ['script', 'noscript', 'style', 'textarea', 'pre', 'code'] }
    };
  </script>
  <script defer src="https://cdn.jsdelivr.net/npm/mathjax@3/es5/tex-chtml.js"></script>
  <script defer src="assets/viewer.js"></script>
</head>
<body>
  <div class="app-shell">
    <aside class="sidebar">
      <div class="brand">
        <div class="eyebrow">Trellis Tablet</div>
        <h1 id="siteTitle">Loading...</h1>
      </div>
      <section class="nav-section">
        <h2>Build</h2>
        <a id="buildInfoLink" class="target-link" href="#build-info"><span class="node-name">Build Info</span><span class="badge">TOML</span></a>
        <a class="target-link" href="data/build-info.toml" target="_blank" rel="noopener"><span class="node-name">Raw TOML</span><span class="badge">file</span></a>
      </section>
      <section class="nav-section">
        <h2>Paper Targets</h2>
        <div id="targetList" class="target-list"></div>
      </section>
      <section class="nav-section">
        <h2>Dependency Outline</h2>
        <div id="dependencyOutline" class="dependency-outline"></div>
      </section>
      <section class="nav-section all-node-section">
        <h2>All Nodes</h2>
        <label class="search-label" for="nodeSearch">Search nodes</label>
        <input id="nodeSearch" class="search-input" type="search" autocomplete="off" placeholder="Node name">
        <div id="allNodeList" class="all-node-list"></div>
      </section>
    </aside>
    <main class="content" id="mainContent" tabindex="-1">
      <div class="loading">Loading tablet...</div>
    </main>
  </div>
</body>
</html>
"""


CSS = r"""
:root {
  color-scheme: light;
  --bg: #f7f7f4;
  --panel: #ffffff;
  --panel-soft: #fbfbf8;
  --ink: #202124;
  --muted: #626a73;
  --line: #d9ddd5;
  --accent: #0f6b63;
  --accent-weak: #e3f1ee;
  --code-bg: #f2f4f4;
  --warn: #8a4b00;
}

* { box-sizing: border-box; }

html, body {
  margin: 0;
  min-height: 100%;
  background: var(--bg);
  color: var(--ink);
  font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
  font-size: 15px;
  line-height: 1.45;
}

a {
  color: var(--accent);
  text-decoration: none;
}

a:hover { text-decoration: underline; }

.app-shell {
  display: grid;
  grid-template-columns: minmax(280px, 360px) minmax(0, 1fr);
  min-height: 100vh;
}

.sidebar {
  position: sticky;
  top: 0;
  height: 100vh;
  overflow: auto;
  border-right: 1px solid var(--line);
  background: #ecefe9;
  padding: 18px 14px 24px;
}

.brand { margin-bottom: 16px; }

.eyebrow {
  font-size: 0.72rem;
  color: var(--muted);
  text-transform: uppercase;
  letter-spacing: 0.08em;
}

.brand h1 {
  margin: 4px 0 0;
  font-size: 1.05rem;
  line-height: 1.25;
  font-weight: 700;
}

.search-label {
  display: block;
  font-size: 0.78rem;
  color: var(--muted);
  margin-bottom: 5px;
}

.search-input {
  width: 100%;
  border: 1px solid var(--line);
  border-radius: 6px;
  padding: 8px 10px;
  font: inherit;
  background: var(--panel);
}

.nav-section {
  margin-top: 18px;
}

.nav-section h2 {
  margin: 0 0 7px;
  font-size: 0.78rem;
  color: var(--muted);
  text-transform: uppercase;
  letter-spacing: 0.08em;
}

.target-list,
.all-node-list {
  display: grid;
  gap: 3px;
}

.node-link,
.target-link {
  display: grid;
  grid-template-columns: minmax(0, 1fr) auto;
  align-items: baseline;
  gap: 8px;
  border-radius: 5px;
  padding: 5px 7px;
  color: var(--ink);
}

.node-link:hover,
.target-link:hover {
  background: rgba(15, 107, 99, 0.08);
  text-decoration: none;
}

.node-link.active,
.target-link.active {
  background: var(--accent-weak);
  color: #063d39;
  font-weight: 650;
}

.node-name {
  overflow-wrap: anywhere;
}

.badge {
  justify-self: end;
  white-space: nowrap;
  border: 1px solid var(--line);
  border-radius: 999px;
  padding: 1px 6px;
  color: var(--muted);
  font-size: 0.7rem;
  font-weight: 500;
}

.dependency-outline details {
  border-left: 1px solid var(--line);
  margin-left: 4px;
  padding-left: 8px;
}

.dependency-outline details + details { margin-top: 6px; }

.dependency-outline summary {
  cursor: pointer;
  color: var(--ink);
  font-weight: 650;
  padding: 4px 0;
}

.tree {
  list-style: none;
  margin: 0;
  padding: 0 0 0 9px;
}

.tree ul {
  list-style: none;
  margin: 0;
  padding: 0 0 0 14px;
  border-left: 1px solid var(--line);
}

.tree li {
  margin: 1px 0;
}

.tree a {
  display: inline-block;
  max-width: 100%;
  border-radius: 4px;
  padding: 2px 4px;
  color: var(--ink);
  overflow-wrap: anywhere;
}

.tree a:hover {
  background: rgba(15, 107, 99, 0.08);
  text-decoration: none;
}

.tree .repeated > a {
  color: var(--muted);
}

.content {
  min-width: 0;
  padding: 30px min(5vw, 56px) 64px;
}

.node-header {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 18px;
  margin-bottom: 18px;
}

.node-header h2 {
  margin: 0;
  font-size: 1.8rem;
  line-height: 1.15;
  overflow-wrap: anywhere;
}

.meta-line {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  margin-top: 8px;
  color: var(--muted);
  font-size: 0.86rem;
}

.source-links {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  justify-content: flex-end;
}

.source-links a,
.pill {
  border: 1px solid var(--line);
  border-radius: 999px;
  padding: 3px 8px;
  background: var(--panel);
  color: var(--accent);
  font-size: 0.82rem;
}

.section-title {
  margin: 30px 0 10px;
  font-size: 1rem;
  color: var(--muted);
  text-transform: uppercase;
  letter-spacing: 0.08em;
}

.node-panel {
  border: 1px solid var(--line);
  border-radius: 8px;
  background: var(--panel);
  margin: 14px 0;
  overflow: hidden;
}

.node-panel-header {
  display: flex;
  justify-content: space-between;
  gap: 12px;
  align-items: baseline;
  padding: 11px 14px;
  background: var(--panel-soft);
  border-bottom: 1px solid var(--line);
}

.node-panel-header h3 {
  margin: 0;
  font-size: 1rem;
  overflow-wrap: anywhere;
}

.subgrid {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
  gap: 0;
}

.block {
  min-width: 0;
  padding: 14px;
  border-bottom: 1px solid var(--line);
}

.block:nth-child(odd) {
  border-right: 1px solid var(--line);
}

.block h4 {
  margin: 0 0 9px;
  font-size: 0.78rem;
  color: var(--muted);
  text-transform: uppercase;
  letter-spacing: 0.08em;
}

.tex-render {
  overflow-wrap: anywhere;
}

.tex-env-title {
  font-weight: 700;
  margin-bottom: 8px;
}

.tex-body p {
  margin: 0.6em 0;
}

.latex-list {
  margin: 0.65em 0 0.65em 1.35em;
  padding: 0;
}

.latex-list li {
  margin: 0.45em 0;
  padding-left: 0.25em;
}

.latex-list p {
  margin: 0.35em 0;
}

.math-block {
  margin: 0.75em 0;
  overflow-x: auto;
  overflow-y: hidden;
}

.info-table {
  width: 100%;
  border-collapse: collapse;
  margin: 0.4em 0 1em;
  font-size: 0.92rem;
}

.info-table th,
.info-table td {
  vertical-align: top;
  text-align: left;
  border-bottom: 1px solid var(--line);
  padding: 7px 8px;
}

.info-table th {
  width: 13rem;
  color: var(--muted);
  font-weight: 650;
}

.build-info-list {
  margin: 0.5em 0 1em 1.25em;
  padding: 0;
}

.build-info-list li {
  margin: 0.25em 0;
}

pre {
  margin: 0;
  overflow: auto;
  background: var(--code-bg);
  border-radius: 6px;
  padding: 12px;
  font-size: 0.82rem;
  line-height: 1.45;
}

code {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace;
}

details.proof {
  padding: 0;
}

details.proof summary {
  cursor: pointer;
  padding: 12px 14px;
  color: var(--ink);
  background: var(--panel-soft);
  border-bottom: 1px solid var(--line);
  font-weight: 650;
}

details.proof[open] summary {
  border-bottom: 1px solid var(--line);
}

details.proof .proof-body {
  padding: 14px;
}

.links-row {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin: 8px 0 0;
}

.closure-note {
  color: var(--muted);
  font-size: 0.9rem;
  line-height: 1.45;
  margin: 0 0 12px;
  max-width: 920px;
}

.closure-note p {
  margin: 0.35em 0;
}

.warning {
  color: var(--warn);
  background: #fff8e8;
  border: 1px solid #ead7a1;
  border-radius: 6px;
  padding: 10px 12px;
}

.empty {
  color: var(--muted);
  font-style: italic;
}

.loading {
  color: var(--muted);
}

@media (max-width: 900px) {
  .app-shell {
    grid-template-columns: 1fr;
  }
  .sidebar {
    position: relative;
    height: auto;
    max-height: 55vh;
    border-right: 0;
    border-bottom: 1px solid var(--line);
  }
  .content {
    padding: 22px 16px 48px;
  }
  .node-header {
    display: block;
  }
  .source-links {
    justify-content: flex-start;
    margin-top: 12px;
  }
  .subgrid {
    grid-template-columns: 1fr;
  }
  .block:nth-child(odd) {
    border-right: 0;
  }
}
"""


JS = r"""
let tablet = null;
let nodeNames = [];
let activeNode = "";
const BUILD_INFO_ROUTE = "build-info";

const $ = (id) => document.getElementById(id);

function escapeHtml(value) {
  return String(value || "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

function nodeExists(name) {
  return tablet && tablet.nodes && Object.prototype.hasOwnProperty.call(tablet.nodes, name);
}

function nodeHref(name) {
  return `#${encodeURIComponent(name)}`;
}

function kindBadge(kind) {
  return `<span class="badge">${escapeHtml(kind || "node")}</span>`;
}

function nodeLink(name, extraClass = "") {
  const node = tablet.nodes[name];
  const active = name === activeNode ? " active" : "";
  return `<a class="node-link${active} ${extraClass}" href="${nodeHref(name)}"><span class="node-name">${escapeHtml(name)}</span>${kindBadge(node ? node.kind : "")}</a>`;
}

function compactNodeAnchor(name) {
  const active = name === activeNode ? " active" : "";
  return `<a class="${active}" href="${nodeHref(name)}">${escapeHtml(name)}</a>`;
}

function renderNodeReferences(escaped) {
  return escaped.replace(/\\noderef\{([A-Za-z0-9_']+)\}/g, (_m, name) => {
    if (!nodeExists(name)) return `<span class="missing-ref">${escapeHtml(name)}</span>`;
    return `<a class="node-ref" href="${nodeHref(name)}">${escapeHtml(name)}</a>`;
  });
}

function stripOuterEnv(raw) {
  const text = String(raw || "").trim();
  const match = text.match(/^\\begin\{([A-Za-z*]+)\}(?:\[[^\]]*\])?([\s\S]*)\\end\{\1\}$/);
  if (!match) return { env: "", body: text };
  return { env: match[1], body: match[2].trim() };
}

function stripLatexLabels(text) {
  return String(text || "").replace(/\\label\{[^{}]*\}/g, "");
}

function renderTextChunk(text) {
  const tokens = [];
  const protect = (html) => {
    const key = `%%TRELLIS_HTML_TOKEN_${tokens.length}%%`;
    tokens.push(html);
    return key;
  };
  let source = stripLatexLabels(text);
  source = source
    .replace(/\\noderef\{([A-Za-z0-9_']+)\}/g, (_m, name) => {
      if (!nodeExists(name)) return protect(`<span class="missing-ref">${escapeHtml(name)}</span>`);
      return protect(`<a class="node-ref" href="${nodeHref(name)}">${escapeHtml(name)}</a>`);
    })
    .replace(/\\(?:emph|textit)\{([^{}]*)\}/g, (_m, inner) => protect(`<em>${escapeHtml(inner)}</em>`))
    .replace(/\\textbf\{([^{}]*)\}/g, (_m, inner) => protect(`<strong>${escapeHtml(inner)}</strong>`))
    .replace(/\\'([A-Za-z])/g, (_m, letter) => {
      const accents = {
        a: "&aacute;", e: "&eacute;", i: "&iacute;", o: "&oacute;", u: "&uacute;", y: "&yacute;",
        A: "&Aacute;", E: "&Eacute;", I: "&Iacute;", O: "&Oacute;", U: "&Uacute;", Y: "&Yacute;"
      };
      return protect(accents[letter] || escapeHtml(letter));
    })
    .replace(/``/g, '"')
    .replace(/''/g, '"')
    .replace(/\\([%&_#$])/g, "$1")
    .replace(/~/g, " ");
  let escaped = escapeHtml(source);
  tokens.forEach((html, index) => {
    escaped = escaped.replaceAll(`%%TRELLIS_HTML_TOKEN_${index}%%`, html);
  });
  return escaped;
}

function renderInlineTexContent(text) {
  const parts = String(text || "").split(/(\\\([\s\S]*?\\\))/g);
  return parts.map((part) => {
    if (!part) return "";
    if (part.startsWith("\\(") && part.endsWith("\\)")) {
      return escapeHtml(part);
    }
    const collapsed = part.replace(/\s*\n\s*/g, " ");
    return renderTextChunk(collapsed);
  }).join("");
}

function normalizeDisplayMath(text) {
  return stripLatexLabels(text).replace(
    /(\\begin\{aligned\})([\s\S]*?)(\\tag\{[^}]+\})(\s*\\end\{aligned\})/g,
    (_match, begin, body, tag, end) => `${begin}${body}${end}${tag}`
  );
}

function renderTexParagraphs(body) {
  const parts = String(body || "").split(/(\\\[[\s\S]*?\\\])/g);
  return parts.map((part) => {
    if (!part || !part.trim()) return "";
    if (part.startsWith("\\[") && part.endsWith("\\]")) {
      return `<div class="math-block">${escapeHtml(normalizeDisplayMath(part))}</div>`;
    }
    return part
      .split(/\n\s*\n/g)
      .map((paragraph) => paragraph.trim())
      .filter(Boolean)
      .map((paragraph) => `<p>${renderInlineTexContent(paragraph)}</p>`)
      .join("");
  }).join("");
}

function splitLatexItems(body) {
  const matches = Array.from(String(body || "").matchAll(/\\item(?:\[[^\]]*\])?/g));
  if (!matches.length) return [];
  return matches.map((match, index) => {
    const start = match.index + match[0].length;
    const end = index + 1 < matches.length ? matches[index + 1].index : body.length;
    return body.slice(start, end).trim();
  }).filter(Boolean);
}

function renderLatexList(env, body) {
  const tag = env === "enumerate" ? "ol" : "ul";
  const items = splitLatexItems(body);
  if (!items.length) return renderTexParagraphs(body);
  return `<${tag} class="latex-list">${items.map((item) => `<li>${renderTexBody(item)}</li>`).join("")}</${tag}>`;
}

function renderTexBody(body) {
  const text = stripLatexLabels(body);
  const pattern = /\\begin\{(enumerate|itemize)\}(?:\[[^\]]*\])?([\s\S]*?)\\end\{\1\}/g;
  let html = "";
  let lastIndex = 0;
  let match = null;
  while ((match = pattern.exec(text)) !== null) {
    html += renderTexParagraphs(text.slice(lastIndex, match.index));
    html += renderLatexList(match[1], match[2]);
    lastIndex = match.index + match[0].length;
  }
  html += renderTexParagraphs(text.slice(lastIndex));
  return html;
}

function renderTex(raw) {
  if (!raw || !raw.trim()) return `<div class="empty">No TeX content.</div>`;
  const { env, body } = stripOuterEnv(raw);
  const rendered = renderTexBody(body);
  const title = env ? `<div class="tex-env-title">${escapeHtml(env[0].toUpperCase() + env.slice(1))}</div>` : "";
  return `<div class="tex-render">${title}<div class="tex-body">${rendered || renderInlineTexContent(body)}</div></div>`;
}

function renderLean(raw) {
  if (!raw || !raw.trim()) return `<div class="empty">No Lean content.</div>`;
  return `<pre><code>${escapeHtml(raw)}</code></pre>`;
}

function renderLinks(label, names) {
  const filtered = (names || []).filter(nodeExists);
  if (!filtered.length) return "";
  return `<div class="links-row"><span class="pill">${escapeHtml(label)}</span>${filtered.map((name) => `<a class="pill" href="${nodeHref(name)}">${escapeHtml(name)}</a>`).join("")}</div>`;
}

function renderNodePills(names) {
  const filtered = (names || []).filter(nodeExists);
  if (!filtered.length) return "";
  return `<div class="links-row">${filtered.map((name) => `<a class="pill" href="${nodeHref(name)}">${escapeHtml(name)}</a>`).join("")}</div>`;
}

function renderTextPills(names) {
  const filtered = (names || []).filter(Boolean);
  if (!filtered.length) return "";
  return `<div class="links-row">${filtered.map((name) => `<span class="pill">${escapeHtml(name)}</span>`).join("")}</div>`;
}

function mathlibDocsUrl(moduleName) {
  return `https://leanprover-community.github.io/mathlib4_docs/${encodeURIComponent(moduleName).replaceAll(".", "/")}.html`;
}

function renderMathlibPills(names) {
  const filtered = (names || []).filter(Boolean);
  if (!filtered.length) return "";
  return `<div class="links-row">${filtered.map((name) => `<a class="pill" href="${mathlibDocsUrl(name)}" target="_blank" rel="noopener">${escapeHtml(name)}</a>`).join("")}</div>`;
}

function renderInfoTable(rows) {
  const body = rows
    .filter((row) => row && row.length === 2)
    .map(([label, value]) => `<tr><th>${escapeHtml(label)}</th><td>${value}</td></tr>`)
    .join("");
  return `<table class="info-table"><tbody>${body}</tbody></table>`;
}

function renderInlineCode(value) {
  return `<code>${escapeHtml(value || "")}</code>`;
}

function renderStringList(items) {
  const filtered = (items || []).filter(Boolean);
  if (!filtered.length) return `<span class="empty">None</span>`;
  return `<ul class="build-info-list">${filtered.map((item) => `<li>${renderInlineCode(item)}</li>`).join("")}</ul>`;
}

function extractNodeReferences(text) {
  const refs = [];
  const pattern = /\\noderef\{([A-Za-z0-9_']+)\}/g;
  let match = null;
  while ((match = pattern.exec(String(text || ""))) !== null) {
    refs.push(match[1]);
  }
  return refs;
}

function directDependencies(node) {
  const seen = new Set();
  const add = (name) => {
    if (name && name !== node.name && nodeExists(name)) seen.add(name);
  };
  (node.imports || []).forEach(add);
  extractNodeReferences(node.tex_statement).forEach(add);
  extractNodeReferences(node.tex_proof).forEach(add);
  return Array.from(seen).sort((a, b) => a.localeCompare(b));
}

function renderSemanticClosureNote() {
  return `
    <div class="closure-note">
      <p>These are the additional nodes whose statements must be read to verify that the Lean statement of this node genuinely corresponds to its claimed natural-language mathematical meaning. This list is generated by Trellis's Lean semantic-payload walk: starting at the node's Lean declaration, theorem/axiom/etc. types are walked, definition types and values are walked, inductive types and constructors are walked, theorem proof bodies are not walked, external Mathlib constants stop at the boundary, and generated constants are collapsed to their top-level tablet node.</p>
    </div>
  `;
}

function renderSourceLinks(node) {
  const links = [];
  if (node.github_tex_url) links.push(`<a href="${escapeHtml(node.github_tex_url)}">TeX source</a>`);
  if (node.github_lean_url) links.push(`<a href="${escapeHtml(node.github_lean_url)}">Lean source</a>`);
  return links.join("");
}

function renderNodePanel(name, opts = {}) {
  const node = tablet.nodes[name];
  if (!node) return "";
  const header = opts.root
    ? ""
    : `<div class="node-panel-header"><h3><a href="${nodeHref(name)}">${escapeHtml(name)}</a></h3>${kindBadge(node.kind)}</div>`;
  const isDefinition = node.kind === "definition";
  const leanMain = isDefinition && node.lean_proof
    ? `${node.lean_statement}\n-- BODY\n${node.lean_proof}`
    : node.lean_statement;
  const texHeading = isDefinition ? "TeX Definition" : "TeX Statement";
  const leanHeading = isDefinition ? "Lean Definition" : "Lean Statement";
  const proofBlocks = isDefinition ? "" : `
      <details class="proof"${opts.openProofs ? " open" : ""}>
        <summary>TeX proof</summary>
        <div class="proof-body">${renderTex(node.tex_proof)}</div>
      </details>
      <details class="proof"${opts.openProofs ? " open" : ""}>
        <summary>Lean proof</summary>
        <div class="proof-body">${renderLean(node.lean_proof)}</div>
      </details>
  `;
  return `
    <article class="node-panel" id="panel-${escapeHtml(name)}">
      ${header}
      <div class="subgrid">
        <section class="block">
          <h4>${texHeading}</h4>
          ${renderTex(node.tex_statement)}
        </section>
        <section class="block">
          <h4>${leanHeading}</h4>
          ${renderLean(leanMain)}
        </section>
      </div>
      ${proofBlocks}
    </article>
  `;
}

function renderBuildInfo() {
  activeNode = BUILD_INFO_ROUTE;
  const info = tablet.build_info || {};
  const source = info.source || {};
  const trellis = info.trellis || {};
  const lean = info.lean || {};
  const axioms = info.axioms || {};
  const approved = axioms.approved_axioms || {};
  const packages = lean.packages || [];
  const directPackages = packages.filter((pkg) => !pkg.inherited);
  const inheritedPackages = packages.filter((pkg) => pkg.inherited);
  const probes = axioms.print_axioms || [];
  document.title = `Build Info - ${tablet.title}`;
  const renderPackageRows = (items) => items.length
    ? `<table class="info-table"><thead><tr><th>Package</th><th>Revision</th></tr></thead><tbody>${items.map((pkg) => {
        const name = pkg.scope ? `${pkg.scope}/${pkg.name}` : pkg.name;
        const url = pkg.url ? `<a href="${escapeHtml(pkg.url)}" target="_blank" rel="noopener">${escapeHtml(name)}</a>` : escapeHtml(name);
        return `<tr><th>${url}</th><td>${renderInlineCode(pkg.rev || "")}</td></tr>`;
      }).join("")}</tbody></table>`
    : `<p class="empty">No package pins in this group.</p>`;
  const probeBlocks = probes.length
    ? probes.map((probe) => `
        <article class="node-panel">
          <div class="node-panel-header"><h3>${escapeHtml(probe.declaration || "")}</h3>${kindBadge("print axioms")}</div>
          ${renderLean(probe.stdout || "No #print axioms output recorded.")}
          ${probe.stderr ? `<h4>stderr</h4>${renderLean(probe.stderr)}` : ""}
        </article>
      `).join("")
    : `<p class="empty">No #print axioms probes were recorded.</p>`;
  $("mainContent").innerHTML = `
    <header class="node-header">
      <div>
        <h2>Build Info</h2>
        <div class="meta-line">
          <span>public tablet viewer</span>
          <span>${escapeHtml((tablet.targets || []).length)} paper targets</span>
        </div>
      </div>
      <div class="source-links"><a href="data/build-info.toml" target="_blank" rel="noopener">Raw TOML</a></div>
    </header>
    <section class="block">
      <h4>Source</h4>
      ${renderInfoTable([
        ["Git remote", escapeHtml(source.git_remote || "")],
        ["Git branch", escapeHtml(source.git_branch || "")],
        ["Git commit", renderInlineCode(source.git_commit || "")],
        ["Git dirty", escapeHtml(String(Boolean(source.git_dirty)))],
        ["GitHub base", source.github_base ? `<a href="${escapeHtml(source.github_base)}" target="_blank" rel="noopener">${escapeHtml(source.github_base)}</a>` : ""],
      ])}
    </section>
    <section class="block">
      <h4>Lean Build Chain</h4>
      ${renderInfoTable([
        ["Toolchain", renderInlineCode(lean.toolchain || "")],
        ["Lean version", renderInlineCode(lean.lean_version || "")],
        ["Lake version", renderInlineCode(lean.lake_version || "")],
        ["lake-manifest SHA-256", renderInlineCode(lean.lake_manifest_sha256 || "")],
        ["lakefile SHA-256", renderInlineCode(lean.lakefile_sha256 || "")],
      ])}
      <p>The tablet project's Lake file directly requires only the package pins in the first table. The inherited package pins are pulled in by dependencies such as Mathlib; they are recorded as part of the reproducible Lake build chain, but this is not a claim that the tablet imports or uses those packages directly.</p>
      <h4>Direct Lake Dependencies</h4>
      ${renderPackageRows(directPackages)}
      <h4>Inherited Lake Package Pins</h4>
      ${renderPackageRows(inheritedPackages)}
    </section>
    <section class="block">
      <h4>Trellis Export Chain</h4>
      ${renderInfoTable([
        ["Trellis commit", renderInlineCode(trellis.git_commit || "")],
        ["Trellis dirty", escapeHtml(String(Boolean(trellis.git_dirty)))],
        ["Exporter SHA-256", renderInlineCode(trellis.exporter_sha256 || "")],
        ["Semantic script SHA-256", renderInlineCode(trellis.semantic_fingerprint_script_sha256 || "")],
      ])}
    </section>
    <section class="block">
      <h4>Axiom Policy</h4>
      <p>${escapeHtml(axioms.probe_description || "")}</p>
      ${renderInfoTable([
        ["Default allowed axioms", renderStringList(axioms.default_allowed_axioms || [])],
        ["Approved axioms file", escapeHtml(approved.present ? `${approved.path || "APPROVED_AXIOMS.json"} (${approved.empty ? "empty" : "nonempty"})` : "not present")],
        ["Project-approved axioms", renderStringList(approved.global || [])],
        ["Effective allowed axioms", renderStringList(axioms.allowed_axioms || [])],
      ])}
      ${probeBlocks}
    </section>
    <section class="block">
      <h4>Raw TOML</h4>
      ${renderLean(info.toml || "")}
    </section>
  `;
  renderSidebar();
  $("mainContent").focus({ preventScroll: true });
}

function renderMain(name) {
  if (name === BUILD_INFO_ROUTE) {
    renderBuildInfo();
    return;
  }
  const node = tablet.nodes[name] || tablet.nodes[nodeNames[0]];
  if (!node) {
    $("mainContent").innerHTML = `<div class="warning">No nodes found.</div>`;
    return;
  }
  activeNode = node.name;
  document.title = `${node.name} - ${tablet.title}`;
  const closure = (node.semantic_closure || []).filter(nodeExists);
  const deps = directDependencies(node);
  const mathlibImports = node.mathlib_imports || [];
  const closureItems = closure.length
    ? closure.map((dep) => renderNodePanel(dep)).join("")
    : `<p class="empty">The semantic closure of this node has no additional tablet nodes.</p>`;
  const closureWarning = node.semantic_closure_error
    ? `<p class="warning">${escapeHtml(node.semantic_closure_error)}</p>`
    : "";
  $("mainContent").innerHTML = `
    <header class="node-header">
      <div>
        <h2>${escapeHtml(node.name)}</h2>
        <div class="meta-line">
          <span>${escapeHtml(node.kind || "node")}</span>
          ${node.is_target ? "<span>paper target</span>" : ""}
          <span>${(node.imports || []).length} imports</span>
          <span>${closure.length} semantic-closure nodes</span>
        </div>
      </div>
      <div class="source-links">${renderSourceLinks(node)}</div>
    </header>
    ${renderLinks("Imported by", node.imported_by)}
    ${renderNodePanel(node.name, { root: true })}
    <h2 class="section-title">Semantic Closure</h2>
    ${renderSemanticClosureNote()}
    ${closureWarning}
    ${renderNodePills(closure)}
    ${closureItems}
    <h2 class="section-title">Tablet Dependencies</h2>
    ${renderNodePills(deps) || `<p class="empty">This node has no direct tablet dependencies.</p>`}
    <h2 class="section-title">Mathlib</h2>
    ${renderMathlibPills(mathlibImports) || `<p class="empty">No Mathlib imports appear in this node's recursive tablet import closure.</p>`}
  `;
  renderSidebar();
  if (window.MathJax && window.MathJax.typesetPromise) {
    window.MathJax.typesetPromise([$("mainContent")]).catch(() => {});
  }
  $("mainContent").focus({ preventScroll: true });
}

function buildTree(root) {
  const seen = new Set();
  function walk(name) {
    const node = tablet.nodes[name];
    if (!node) return "";
    const repeated = seen.has(name);
    seen.add(name);
    const children = repeated ? [] : (node.imports || []).filter(nodeExists);
    return `<li class="${repeated ? "repeated" : ""}">${compactNodeAnchor(name)}${children.length ? `<ul>${children.map(walk).join("")}</ul>` : ""}</li>`;
  }
  return `<ul class="tree">${walk(root)}</ul>`;
}

function renderSidebar() {
  $("siteTitle").textContent = tablet.title || "Trellis Tablet";
  const buildLink = $("buildInfoLink");
  if (buildLink) {
    buildLink.classList.toggle("active", activeNode === BUILD_INFO_ROUTE);
  }
  $("targetList").innerHTML = (tablet.targets || [])
    .filter((target) => nodeExists(target.node))
    .map((target) => {
      const active = target.node === activeNode ? " active" : "";
      const label = target.label ? `<span class="badge">${escapeHtml(target.label)}</span>` : kindBadge(tablet.nodes[target.node].kind);
      return `<a class="target-link${active}" href="${nodeHref(target.node)}"><span class="node-name">${escapeHtml(target.node)}</span>${label}</a>`;
    })
    .join("");

  $("dependencyOutline").innerHTML = (tablet.targets || [])
    .filter((target) => nodeExists(target.node))
    .map((target) => {
      const open = "";
      const label = target.label ? ` <span class="badge">${escapeHtml(target.label)}</span>` : "";
      return `<details${open}><summary>${escapeHtml(target.node)}${label}</summary>${buildTree(target.node)}</details>`;
    })
    .join("");

  const query = $("nodeSearch").value.trim().toLowerCase();
  const visible = query
    ? nodeNames.filter((name) => name.toLowerCase().includes(query))
    : nodeNames;
  $("allNodeList").innerHTML = visible.map((name) => nodeLink(name)).join("");
}

function routeFromHash() {
  const raw = decodeURIComponent((location.hash || "").replace(/^#/, ""));
  if (raw === BUILD_INFO_ROUTE) return BUILD_INFO_ROUTE;
  return nodeExists(raw) ? raw : ((tablet.targets || []).find((t) => nodeExists(t.node)) || {}).node || nodeNames[0];
}

function firstSearchMatch() {
  const query = $("nodeSearch").value.trim().toLowerCase();
  if (!query) return "";
  return nodeNames.find((name) => name.toLowerCase().includes(query)) || "";
}

async function boot() {
  const response = await fetch("data/tablet.json", { cache: "no-cache" });
  tablet = await response.json();
  nodeNames = Object.keys(tablet.nodes || {}).sort((a, b) => a.localeCompare(b));
  $("nodeSearch").addEventListener("input", renderSidebar);
  $("nodeSearch").addEventListener("keydown", (event) => {
    if (event.key !== "Enter") return;
    const match = firstSearchMatch();
    if (!match) return;
    event.preventDefault();
    location.hash = encodeURIComponent(match);
  });
  document.addEventListener("click", (event) => {
    const link = event.target && event.target.closest ? event.target.closest("a") : null;
    if (!link) return;
    const rawHref = link.getAttribute("href") || "";
    const directName = rawHref.startsWith("#") ? decodeURIComponent(rawHref.slice(1)) : rawHref;
    if (/^[A-Za-z0-9_']+$/.test(directName) && nodeExists(directName)) {
      event.preventDefault();
      location.hash = encodeURIComponent(directName);
    }
  });
  window.addEventListener("hashchange", () => renderMain(routeFromHash()));
  renderMain(routeFromHash());
}

boot().catch((err) => {
  $("mainContent").innerHTML = `<div class="warning">Could not load tablet viewer data: ${escapeHtml(err && err.message ? err.message : err)}</div>`;
});
"""


def export_viewer(args: argparse.Namespace) -> None:
    repo = Path(args.repo).expanduser().resolve()
    out = Path(args.out).expanduser().resolve()
    title = args.title or _title_from_readme(repo)
    payload = _build_payload(
        repo=repo,
        title=title,
        github_base=args.github_base or "",
        skip_semantic_closure=args.skip_semantic_closure,
        semantic_state_json=Path(args.semantic_state_json).expanduser().resolve()
        if args.semantic_state_json
        else None,
        force_lean_semantic=args.force_lean_semantic,
        timeout_secs=args.timeout_secs,
        semantic_batch_size=args.semantic_batch_size,
        semantic_sleep_secs=args.semantic_sleep_secs,
        mathlib_imports_json=Path(args.mathlib_imports_json).expanduser().resolve()
        if args.mathlib_imports_json
        else None,
    )
    out.mkdir(parents=True, exist_ok=True)
    _write(out / "index.html", INDEX_HTML)
    _write(out / "index.php", INDEX_HTML)
    _write(out / "assets" / "viewer.css", CSS)
    _write(out / "assets" / "viewer.js", JS)
    _write(
        out / "data" / "tablet.json",
        json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True),
    )
    _write(out / "data" / "build-info.toml", payload.get("build_info", {}).get("toml", ""))
    node_count = len(payload["nodes"])
    target_count = len(payload["targets"])
    print(f"wrote {out}")
    print(f"nodes={node_count} targets={target_count}")


def main(argv: Iterable[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", help="completed tablet repository")
    parser.add_argument("out", help="output directory for static viewer files")
    parser.add_argument("--title", default="", help="viewer title; defaults to README heading or repo name")
    parser.add_argument(
        "--github-base",
        default="",
        help="base GitHub blob URL, e.g. https://github.com/<owner>/<repo>/blob/<branch>",
    )
    parser.add_argument(
        "--skip-semantic-closure",
        action="store_true",
        help="write the viewer without computing semantic closures",
    )
    parser.add_argument(
        "--semantic-state-json",
        default="",
        help="completed Trellis supervisor_state.json to reuse for semantic closures",
    )
    parser.add_argument(
        "--force-lean-semantic",
        action="store_true",
        help="ignore supervisor_state.json and run Lean semantic fingerprinting",
    )
    parser.add_argument(
        "--timeout-secs",
        type=float,
        default=1800.0,
        help="timeout for the batched Lean semantic-closure run",
    )
    parser.add_argument(
        "--semantic-batch-size",
        type=int,
        default=999999,
        help="number of nodes per Lean semantic-closure invocation",
    )
    parser.add_argument(
        "--semantic-sleep-secs",
        type=float,
        default=0.0,
        help="sleep between semantic-closure batches",
    )
    parser.add_argument(
        "--mathlib-imports-json",
        default="",
        help="precomputed JSON from scripts/precompute_tablet_mathlib_imports.py",
    )
    args = parser.parse_args(list(argv) if argv is not None else None)
    export_viewer(args)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
