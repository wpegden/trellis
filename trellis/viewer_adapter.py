"""Viewer-side reconstruction of the legacy DAG viewer contract for trellis.

This module is intentionally viewer-owned. It reads project files, runtime
artifacts, chat history, and git checkpoints and synthesizes the same JSON
shapes consumed by the existing viewer frontend.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional

from trellis.chat_history import read_historical_chats, read_runtime_live_chats, _request_cycles_from_event_log
from trellis.config import load_config
from trellis.history_artifacts import (
    CORR_RESULT_FILENAME,
    PAPER_RESULT_FILENAME,
    REVIEW_RESULT_FILENAME,
    SOUND_RESULT_FILENAME,
    SUPERVISOR_STATE_FILENAME,
    WORKER_HANDOFF_FILENAME,
)
from trellis.prompt_browser import list_prompt_scenarios, render_prompt_scenario
from trellis.project_paths import (
    PROJECT_CONFIG_FILENAME,
    project_config_path,
    project_state_dir_for_repo,
)
from trellis.stall_analyzer import analyze_inflight_request


_TEX_ENV_RE = re.compile(
    r"\\begin\{(theorem|lemma|definition|proposition|corollary|helper)\}(?:\[(.*?)\])?"
)
_TABLET_IMPORT_RE = re.compile(r"^\s*import\s+Tablet\.([A-Za-z0-9_']+)\s*$", re.MULTILINE)
_DECL_START_RE = re.compile(r"^\s*(theorem|lemma|def|noncomputable\s+def)\s+")
_PRINCIPAL_DECL_RE = re.compile(
    r"^\s*(theorem|lemma|def|abbrev|noncomputable\s+def)\s+([A-Za-z0-9_']+)",
    re.MULTILINE,
)
_WORKER_RESULT_ID_RE = re.compile(r"trellis_worker_(\d+)_result\.(?:raw\.json|acceptance\.json)$")
_CHECKPOINT_TAG_RE = re.compile(r"^supervisor2/checkpoint-(\d+)$")
_CHECKPOINT_SUBJECT_RE = re.compile(
    r"supervisor2 checkpoint \d+\s+\|\s+cycle\s+(\d+)(?:\s+\|\s+([^|]+))?(?:\s+\|\s+([^|]+))?(?:\s+\|\s+(.+))?"
)


@dataclass
class RuntimeInfo:
    root: Path
    protocol_state_path: Path
    metadata_path: Path
    bridge_dir: Optional[Path]


@dataclass
class GitSnapshot:
    repo_path: Path
    ref: str

    def read_text(self, rel_path: str) -> str:
        try:
            return subprocess.check_output(
                ["git", "-C", str(self.repo_path), "show", f"{self.ref}:{rel_path}"],
                text=True,
                stderr=subprocess.DEVNULL,
                timeout=10,
            )
        except Exception:
            return ""

    def list_tablet_node_names(self) -> List[str]:
        try:
            raw = subprocess.check_output(
                ["git", "-C", str(self.repo_path), "ls-tree", "-r", "--name-only", self.ref, "--", "Tablet"],
                text=True,
                stderr=subprocess.DEVNULL,
                timeout=10,
            )
        except Exception:
            return []
        names: set[str] = set()
        for rel in raw.splitlines():
            rel = rel.strip()
            if not rel.startswith("Tablet/"):
                continue
            stem = Path(rel).stem
            if stem in {"INDEX", "README", "header"}:
                continue
            if stem:
                names.add(stem)
        return sorted(names)


@dataclass
class WorkingTreeSnapshot:
    repo_path: Path

    def read_text(self, rel_path: str) -> str:
        return _read_text(self.repo_path / rel_path)

    def list_tablet_node_names(self) -> List[str]:
        return _list_repo_tablet_nodes(self.repo_path)


# Match `local macro_rules | `(tactic| sorry) => ...` (or the term variant)
# — a known pattern that masks a literal `sorry` token with a real proof
# body via macro expansion. The compiled proof is sorry-free even though
# the source contains the token, so a naive substring check overcounts
# files as having sorry. Detected here so DAG/Progress views match what
# Lean actually compiled.
_MACRO_RULES_SORRY_RE = re.compile(
    r"macro_rules\s*\|\s*`\(\s*(?:tactic|term)\|\s*sorry\s*\)\s*=>"
)


def _file_has_real_sorry(lean_content: str) -> bool:
    """Approximate "this file's compiled proof actually uses `sorryAx`."

    Strips line + block comments, then looks for a literal `sorry` token.
    If the file contains the `local macro_rules` trick that rewrites
    `sorry` to a real proof body, we treat the literal sorries as masked
    (not real). The truthful signal would be `#print axioms` per node;
    this is the cheap text-only approximation.
    """
    if not lean_content:
        return False
    cleaned = re.sub(r"/-[\s\S]*?-/", "", lean_content)
    cleaned = re.sub(r"--[^\n]*", "", cleaned)
    if not re.search(r"\bsorry\b", cleaned):
        return False
    if _MACRO_RULES_SORRY_RE.search(cleaned):
        return False
    return True


def _recompute_open_nodes_from_repo(present_nodes, repo_path):
    """Override the kernel-computed `open_nodes` with our smarter
    has_sorry detector.

    The kernel populates `committed.open_nodes` (and `live.open_nodes`)
    by calling its own `has_sorry`, which only masks comments — it
    flags the `local macro_rules | `(tactic| sorry) =>` pattern as
    open even though the compiled proof has no `sorryAx`. Until the
    supervisor restart picks up the kernel-side fix, the DAG view
    here is wrong. This helper recomputes the open list from the
    live filesystem using `_file_has_real_sorry`, so the viewer's
    notion of "open" matches reality immediately.
    """
    out = []
    for node in present_nodes or []:
        lean_path = Path(repo_path) / "Tablet" / f"{node}.lean"
        if not lean_path.exists():
            out.append(node)
            continue
        try:
            content = lean_path.read_text(encoding="utf-8")
        except Exception:
            out.append(node)
            continue
        if _file_has_real_sorry(content):
            out.append(node)
    return out


def _read_json(path: Path) -> Dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def _read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except Exception:
        return ""


def _git(repo_path: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", *args],
        cwd=str(repo_path),
        capture_output=True,
        text=True,
        check=check,
        timeout=15,
    )


def _load_runtime_info(repo_path: Path) -> Optional[RuntimeInfo]:
    config_path = project_config_path(repo_path).resolve()
    candidate_roots: List[Path] = []

    repo_state_dir = project_state_dir_for_repo(repo_path)
    runtime_dir = repo_state_dir / "runtime"
    if runtime_dir.exists():
        for child in sorted(runtime_dir.iterdir()):
            if child.is_dir():
                candidate_roots.append(child.resolve())

    extra_roots = os.environ.get("TRELLIS_VIEWER_RUNTIME_ROOTS", "")
    for raw_root in extra_roots.split(os.pathsep):
        raw_root = raw_root.strip()
        if raw_root:
            candidate_roots.append(Path(raw_root).resolve())

    search_roots: List[Path] = []
    for root in (
        repo_path.resolve(),
        repo_path.resolve().parent,
        Path(os.environ.get("PROJECTS_ROOT", "")).resolve() if os.environ.get("PROJECTS_ROOT") else None,
        repo_path.resolve().parent.parent,
    ):
        if root and root.exists():
            search_roots.append(root)

    for search_root in _unique_paths(search_roots):
        candidate_roots.extend(_discover_runtime_roots(search_root, max_depth=4))

    best: Optional[RuntimeInfo] = None
    best_mtime = -1.0
    for root in _unique_paths(candidate_roots):
        metadata_path = root / "runtime_metadata.json"
        protocol_state_path = root / "protocol_state.json"
        if not metadata_path.is_file() or not protocol_state_path.is_file():
            continue
        try:
            metadata = _read_json(metadata_path)
        except Exception:
            continue
        metadata_repo = str(metadata.get("repo_path", "") or "").strip()
        metadata_config = str(metadata.get("config_path", "") or "").strip()
        if metadata_repo and Path(metadata_repo).resolve() == repo_path.resolve():
            pass
        elif metadata_config and Path(metadata_config).resolve() == config_path:
            pass
        else:
            continue
        mtime = protocol_state_path.stat().st_mtime
        bridge_dir = root / "bridge"
        info = RuntimeInfo(
            root=root,
            protocol_state_path=protocol_state_path,
            metadata_path=metadata_path,
            bridge_dir=bridge_dir if bridge_dir.is_dir() else None,
        )
        if mtime > best_mtime:
            best = info
            best_mtime = mtime
    return best


def _unique_paths(paths: Iterable[Path]) -> List[Path]:
    unique: List[Path] = []
    seen: set[Path] = set()
    for path in paths:
        if path in seen:
            continue
        seen.add(path)
        unique.append(path)
    return unique


def _discover_runtime_roots(search_root: Path, *, max_depth: int) -> List[Path]:
    results: List[Path] = []
    base_depth = len(search_root.parts)
    for current, dirs, files in os.walk(search_root):
        current_path = Path(current)
        depth = len(current_path.parts) - base_depth
        if depth > max_depth:
            dirs[:] = []
            continue
        dirs[:] = [
            name
            for name in dirs
            if name not in {".git", "node_modules", "target", "__pycache__", ".lake"}
        ]
        if "runtime_metadata.json" in files and "protocol_state.json" in files:
            results.append(current_path.resolve())
    return results


def _legacy_phase_name(raw: Any) -> str:
    text = str(raw or "").strip()
    mapping = {
        "TheoremStating": "theorem_stating",
        "ProofFormalization": "proof_formalization",
    }
    return mapping.get(text, text.lower() if text else "")


def _legacy_mode_name(raw: Any) -> str:
    text = str(raw or "").strip()
    mapping = {
        "Global": "global",
        "Targeted": "targeted",
        "Repair": "repair",
        "Restructure": "restructure",
    }
    return mapping.get(text, text.lower() if text else "")


def _viewer_verification_status(raw: Any) -> str:
    text = str(raw or "").strip().lower()
    if text.startswith("pass"):
        return "pass"
    if text.startswith("fail"):
        return "fail"
    if text.startswith("struct"):
        return "structural"
    return "?"


def _extract_title_and_env(tex_content: str) -> tuple[str, str]:
    match = _TEX_ENV_RE.search(tex_content or "")
    if not match:
        return "", ""
    return (match.group(2) or "").strip(), (match.group(1) or "").strip()


def _extract_declaration_preview(lean_content: str) -> str:
    lines = lean_content.splitlines()
    result: List[str] = []
    in_decl = False
    for line in lines:
        if not in_decl and _DECL_START_RE.match(line):
            in_decl = True
        if not in_decl:
            continue
        result.append(line)
        stripped = line.strip()
        if ":= sorry" in line or ":= by" in line or stripped.endswith(":="):
            break
    return "\n".join(result).strip()


def _extract_imports(lean_content: str) -> List[str]:
    deps = {match.group(1) for match in _TABLET_IMPORT_RE.finditer(lean_content or "")}
    return sorted(dep for dep in deps if dep)


def _list_repo_tablet_nodes(repo_path: Path) -> List[str]:
    tablet_dir = repo_path / "Tablet"
    if not tablet_dir.is_dir():
        return []
    names: set[str] = set()
    for child in tablet_dir.iterdir():
        if not child.is_file():
            continue
        if child.suffix not in {".lean", ".tex"}:
            continue
        if child.stem in {"INDEX", "README", "header"}:
            continue
        names.add(child.stem)
    return sorted(names)


def _node_file_text(repo_path: Path, node_name: str, suffix: str) -> str:
    return _read_text(repo_path / "Tablet" / f"{node_name}{suffix}")


def _build_nodes_from_snapshot(
    *,
    snapshot: GitSnapshot | WorkingTreeSnapshot,
    protocol_state: Dict[str, Any],
    bridge_state: Dict[str, Dict[str, Any]],
    live_repo_path: Optional[Path] = None,
    activity_override: Optional[Dict[str, Dict[str, bool]]] = None,
) -> Dict[str, Dict[str, Any]]:
    live = protocol_state.get("live") if isinstance(protocol_state.get("live"), dict) else {}
    present_nodes = list(live.get("present_nodes") or [])
    present_node_set = {str(name) for name in present_nodes}
    file_nodes = snapshot.list_tablet_node_names()
    request = protocol_state.get("in_flight_request")
    request_kind = str((request or {}).get("kind", "") or "").strip().lower() if isinstance(request, dict) else ""
    current_worker_request_id = int(request.get("id") or 0) if request_kind == "worker" and isinstance(request, dict) else None
    worker_overlay = (
        _load_active_worker_overlay(live_repo_path, request_id=current_worker_request_id)
        if live_repo_path is not None
        else {"difficulty_updates": {}, "touched_nodes": set()}
    )
    touched_nodes = worker_overlay["touched_nodes"] if request_kind == "worker" else set()
    node_names = sorted(set(str(name) for name in present_nodes) | set(file_nodes))
    if "Preamble" not in node_names and snapshot.read_text("Tablet/Preamble.lean"):
        node_names.insert(0, "Preamble")

    node_kinds = protocol_state.get("node_kinds") if isinstance(protocol_state.get("node_kinds"), dict) else {}
    deps = protocol_state.get("deps") if isinstance(protocol_state.get("deps"), dict) else {}
    target_claims_raw = (
        protocol_state.get("target_claims")
        or protocol_state.get("committed_target_claims")
        or {}
    )
    target_claims = target_claims_raw if isinstance(target_claims_raw, dict) else {}
    proof_nodes = set(protocol_state.get("proof_nodes") or [])
    difficulties = protocol_state.get("node_difficulty") if isinstance(protocol_state.get("node_difficulty"), dict) else {}
    corr_status = protocol_state.get("corr_status") if isinstance(protocol_state.get("corr_status"), dict) else {}
    sound_status = protocol_state.get("sound_status") if isinstance(protocol_state.get("sound_status"), dict) else {}
    sound_assessments = protocol_state.get("sound_assessments") if isinstance(protocol_state.get("sound_assessments"), dict) else {}
    # Fingerprint maps so the per-node verification status can mirror the
    # kernel's `current_corr_state` / `current_sound_state` predicate. A
    # raw status of Pass with a fingerprint mismatch isn't actually
    # passing — the kernel treats it as Unknown (re-verify needed).
    # Without these the viewer tile says "pass" even when the node is
    # listed as an outstanding blocker.
    corr_approved = (
        protocol_state.get("corr_approved_fingerprints")
        if isinstance(protocol_state.get("corr_approved_fingerprints"), dict)
        else {}
    )
    corr_current = live.get("corr_current_fingerprints") if isinstance(live.get("corr_current_fingerprints"), dict) else {}
    sound_approved = (
        protocol_state.get("sound_approved_fingerprints")
        if isinstance(protocol_state.get("sound_approved_fingerprints"), dict)
        else {}
    )
    sound_current = live.get("sound_current_fingerprints") if isinstance(live.get("sound_current_fingerprints"), dict) else {}
    sound_current_parts = (
        live.get("sound_current_fingerprint_parts")
        if isinstance(live.get("sound_current_fingerprint_parts"), dict)
        else {}
    )
    sketch_proof_nodes = set(live.get("sketch_proof_nodes") or [])
    # Substantiveness lane was introduced relatively recently (kernel commit
    # 7198d6c). Older protocol_state.json snapshots — and projects that ran
    # before the lane existed — won't have any of the three substantiveness
    # state maps. Detect the schema rather than the values: a
    # currently-substantiveness-enabled project with no nodes evaluated yet
    # will still emit `substantiveness_status: {}`, which we want to surface
    # as per-node "?", whereas a pre-substantiveness project omits the key
    # entirely and we should skip the field.
    substantiveness_present = (
        "substantiveness_status" in protocol_state
        or "substantiveness_approved_fingerprints" in protocol_state
        or "substantiveness_current_fingerprints" in live
    )
    substantiveness_status = (
        protocol_state.get("substantiveness_status")
        if isinstance(protocol_state.get("substantiveness_status"), dict)
        else {}
    )
    substantiveness_approved = (
        protocol_state.get("substantiveness_approved_fingerprints")
        if isinstance(protocol_state.get("substantiveness_approved_fingerprints"), dict)
        else {}
    )
    substantiveness_current = (
        live.get("substantiveness_current_fingerprints")
        if isinstance(live.get("substantiveness_current_fingerprints"), dict)
        else {}
    )

    activity = _live_activity_map(protocol_state, node_names, touched_nodes)
    if isinstance(activity_override, dict):
        for node_name, node_activity in activity_override.items():
            if node_name not in activity or not isinstance(node_activity, dict):
                continue
            for kind in ("worker", "reviewer", "substantiveness", "correspondence", "soundness"):
                if bool(node_activity.get(kind)):
                    activity[node_name][kind] = True
    corr_issues = bridge_state.get("corr_issues", {})
    sound_issues = bridge_state.get("sound_issues", {})

    nodes: Dict[str, Dict[str, Any]] = {}
    for node_name in node_names:
        lean_content = snapshot.read_text(f"Tablet/{node_name}.lean")
        tex_content = snapshot.read_text(f"Tablet/{node_name}.tex")
        title, tex_env = _extract_title_and_env(tex_content)
        imports = deps.get(node_name)
        if not isinstance(imports, list):
            imports = _extract_imports(lean_content)
        viewer_kind = _viewer_kind(node_name, node_kinds.get(node_name), tex_env, lean_content)
        overlay_difficulty = worker_overlay["difficulty_updates"].get(node_name)
        difficulty = _legacy_mode_name(overlay_difficulty or difficulties.get(node_name))
        verification = {
            "correspondence": _viewer_corr_status(
                node_name=node_name,
                present_nodes=present_node_set,
                touched_nodes=touched_nodes,
                corr_status=corr_status,
                corr_approved_fingerprints=corr_approved,
                corr_current_fingerprints=corr_current,
            ),
            "nl_proof": _viewer_nl_status(
                node_name=node_name,
                viewer_kind=viewer_kind,
                present_nodes=present_node_set,
                touched_nodes=touched_nodes,
                proof_nodes=proof_nodes,
                sound_status=sound_status,
                sound_assessments=sound_assessments,
                sound_approved_fingerprints=sound_approved,
                sound_current_fingerprints=sound_current,
                sound_current_fingerprint_parts=sound_current_parts,
                sketch_proof_nodes=sketch_proof_nodes,
                lean_content=lean_content,
            ),
        }
        if substantiveness_present:
            verification["substantiveness"] = _viewer_substantiveness_status(
                node_name=node_name,
                present_nodes=present_node_set,
                touched_nodes=touched_nodes,
                substantiveness_status=substantiveness_status,
                substantiveness_approved_fingerprints=substantiveness_approved,
                substantiveness_current_fingerprints=substantiveness_current,
            )
        corr_issue = corr_issues.get(node_name)
        if corr_issue:
            verification["correspondence_issue"] = corr_issue
        sound_issue = sound_issues.get(node_name)
        if sound_issue:
            verification["nl_proof_issue"] = sound_issue
        node_target_claims = target_claims.get(node_name) or []
        nodes[node_name] = {
            "status": "open" if not lean_content or _file_has_real_sorry(lean_content) else "closed",
            "kind": viewer_kind,
            "imports": [str(dep) for dep in imports],
            "difficulty": difficulty or "hard",
            "title": title or node_name,
            "texEnv": tex_env,
            "hasSorry": _file_has_real_sorry(lean_content),
            "declaration": _extract_declaration_preview(lean_content),
            "texContent": tex_content,
            "leanContent": lean_content,
            "coversTarget": bool(node_target_claims),
            "verification": verification,
            "activity": activity.get(
                node_name,
                {
                    "worker": False,
                    "reviewer": False,
                    "correspondence": False,
                    "soundness": False,
                },
            ),
        }
    return nodes


def _build_live_nodes(
    *,
    repo_path: Path,
    protocol_state: Dict[str, Any],
    bridge_state: Dict[str, Dict[str, Any]],
) -> Dict[str, Dict[str, Any]]:
    return _build_nodes_from_snapshot(
        snapshot=WorkingTreeSnapshot(repo_path=repo_path),
        protocol_state=protocol_state,
        bridge_state=bridge_state,
        live_repo_path=repo_path,
    )


def _viewer_kind(node_name: str, kernel_kind: Any, tex_env: str, lean_content: str) -> str:
    raw = str(kernel_kind or "").strip()
    if node_name == "Preamble" or raw == "Preamble":
        return "preamble"
    if raw == "Definition":
        return "definition"
    if raw == "Proof":
        return "proof"
    principal = _PRINCIPAL_DECL_RE.search(lean_content or "")
    if principal:
        kind = principal.group(1)
        if "def" in kind or kind == "abbrev":
            return "definition"
        return "proof"
    if tex_env == "definition":
        return "definition"
    return "proof"


def _kernel_current_state(
    *,
    node_name: str,
    status_map: Dict[str, Any],
    approved_map: Dict[str, Any],
    current_map: Dict[str, Any],
) -> str:
    """Mirror the kernel's `current_corr_state` / `current_sound_state`
    predicate (kernel/src/model.rs ~line 1648):

        match (status[node], current[node], approved[node]) {
            (Some(Pass), Some(c), Some(a)) if c == a => Pass,
            (Some(Fail), Some(c), Some(a)) if c == a => Fail,
            _ => Unknown,
        }

    A status of Pass with a fingerprint mismatch (or missing fingerprint
    entry) is NOT pass — the kernel treats it as Unknown (re-needs
    verification), which is why such a node remains an outstanding
    blocker. Returning "pass" from this predicate when the kernel says
    Unknown would lie to the viewer's tile rendering. Returning "?" makes
    the tile honestly say "needs re-verification."
    """
    status = str(status_map.get(node_name) or "").strip().lower()
    approved = approved_map.get(node_name)
    current = current_map.get(node_name)
    # Use `is None` so an empty-string fingerprint counts as PRESENT
    # (Preamble's case — it has no .tex content to fingerprint, the
    # kernel records both as `Some("")`, and they correctly compare
    # equal). The `!fp` truthiness mistake from the chart fix doesn't
    # apply here — None means "no entry in the BTreeMap".
    if approved is None or current is None:
        return "?"
    if approved != current:
        return "?"
    if status.startswith("pass"):
        return "pass"
    if status.startswith("fail"):
        return "fail"
    if status.startswith("struct"):
        return "structural"
    return "?"


def _sound_status_from_assessment(
    *,
    node_name: str,
    sound_assessments: Dict[str, Any],
    sound_current_fingerprint_parts: Dict[str, Any],
) -> Optional[str]:
    assessment = sound_assessments.get(node_name)
    if not isinstance(assessment, dict):
        return None
    status = str(assessment.get("status") or "").strip().lower()
    stored = assessment.get("fingerprints") if isinstance(assessment.get("fingerprints"), dict) else {}
    current = (
        sound_current_fingerprint_parts.get(node_name)
        if isinstance(sound_current_fingerprint_parts.get(node_name), dict)
        else {}
    )
    stored_own = stored.get("own_tex_hash")
    current_own = current.get("own_tex_hash")
    if stored_own and current_own and stored_own != current_own:
        return "?"
    stored_deps = stored.get("dep_statement_hashes") if isinstance(stored.get("dep_statement_hashes"), dict) else {}
    current_deps = current.get("dep_statement_hashes") if isinstance(current.get("dep_statement_hashes"), dict) else {}
    if stored_deps and current_deps and stored_deps != current_deps:
        if status in {"verifierfail", "verifierstructural", "reviewerpinnedfail", "depeditonlystalefail"}:
            return "fail" if status != "verifierstructural" else "structural"
        return "?"
    stored_combined = stored.get("combined_sound_fp")
    current_combined = current.get("combined_sound_fp")
    if stored_combined and current_combined and stored_combined != current_combined:
        if status in {"verifierfail", "verifierstructural", "reviewerpinnedfail", "depeditonlystalefail"}:
            return "fail" if status != "verifierstructural" else "structural"
        return "?"
    if status == "verifierpass":
        return "pass"
    if status in {"verifierfail", "reviewerpinnedfail", "sketchautofail", "depeditonlystalefail"}:
        return "fail"
    if status == "verifierstructural":
        return "structural"
    return "?"


def _viewer_nl_status(
    *,
    node_name: str,
    viewer_kind: str,
    present_nodes: set[str],
    touched_nodes: set[str],
    proof_nodes: set[str],
    sound_status: Dict[str, Any],
    sound_assessments: Dict[str, Any],
    sound_approved_fingerprints: Dict[str, Any],
    sound_current_fingerprints: Dict[str, Any],
    sound_current_fingerprint_parts: Dict[str, Any],
    sketch_proof_nodes: set[str],
    lean_content: str,
) -> str:
    if node_name not in present_nodes or node_name in touched_nodes:
        return "?"
    if node_name == "Preamble" or viewer_kind == "definition":
        return "pass"
    if node_name not in proof_nodes and viewer_kind != "proof":
        return "pass"
    if lean_content and "sorry" not in lean_content:
        return "pass"
    if node_name in sketch_proof_nodes:
        return "fail"
    assessed = _sound_status_from_assessment(
        node_name=node_name,
        sound_assessments=sound_assessments,
        sound_current_fingerprint_parts=sound_current_fingerprint_parts,
    )
    if assessed is not None:
        return assessed
    return _kernel_current_state(
        node_name=node_name,
        status_map=sound_status,
        approved_map=sound_approved_fingerprints,
        current_map=sound_current_fingerprints,
    )


def _viewer_corr_status(
    *,
    node_name: str,
    present_nodes: set[str],
    touched_nodes: set[str],
    corr_status: Dict[str, Any],
    corr_approved_fingerprints: Dict[str, Any],
    corr_current_fingerprints: Dict[str, Any],
) -> str:
    if node_name not in present_nodes or node_name in touched_nodes:
        return "?"
    return _kernel_current_state(
        node_name=node_name,
        status_map=corr_status,
        approved_map=corr_approved_fingerprints,
        current_map=corr_current_fingerprints,
    )


def _viewer_substantiveness_status(
    *,
    node_name: str,
    present_nodes: set[str],
    touched_nodes: set[str],
    substantiveness_status: Dict[str, Any],
    substantiveness_approved_fingerprints: Dict[str, Any],
    substantiveness_current_fingerprints: Dict[str, Any],
) -> str:
    if node_name not in present_nodes or node_name in touched_nodes:
        return "?"
    return _kernel_current_state(
        node_name=node_name,
        status_map=substantiveness_status,
        approved_map=substantiveness_approved_fingerprints,
        current_map=substantiveness_current_fingerprints,
    )


def _worker_result_request_id(path: Path) -> Optional[int]:
    match = _WORKER_RESULT_ID_RE.search(path.name)
    if not match:
        return None
    try:
        return int(match.group(1))
    except Exception:
        return None


def _load_active_worker_overlay(repo_path: Path, request_id: Optional[int] = None) -> Dict[str, Any]:
    state_dir = project_state_dir_for_repo(repo_path)
    runtime_dir = state_dir / "runtime"
    latest_raw: Optional[Path] = None
    latest_acceptance: Optional[Path] = None
    if runtime_dir.exists():
        for child in runtime_dir.iterdir():
            staging = child / "staging"
            if staging.is_dir():
                for raw_path in staging.glob("trellis_worker_*_result.raw.json"):
                    if request_id is not None and _worker_result_request_id(raw_path) != request_id:
                        continue
                    if latest_raw is None or raw_path.stat().st_mtime > latest_raw.stat().st_mtime:
                        latest_raw = raw_path
            # `.acceptance.json` lives in `private/` (worker-unwritable
            # — see _bridge_private_state_dir in bridge.py). The diagnostic
            # viewer overlay just needs read access.
            private = child / "private"
            if private.is_dir():
                for acceptance_path in private.glob("trellis_worker_*_result.acceptance.json"):
                    if request_id is not None and _worker_result_request_id(acceptance_path) != request_id:
                        continue
                    if latest_acceptance is None or acceptance_path.stat().st_mtime > latest_acceptance.stat().st_mtime:
                        latest_acceptance = acceptance_path
    payload: Dict[str, Any] = {}
    if latest_raw and latest_raw.is_file():
        try:
            payload = _read_json(latest_raw)
        except Exception:
            payload = {}
    acceptance: Dict[str, Any] = {}
    if latest_acceptance and latest_acceptance.is_file():
        try:
            acceptance = _read_json(latest_acceptance)
        except Exception:
            acceptance = {}
    difficulty_updates = payload.get("difficulty_updates")
    if not isinstance(difficulty_updates, dict):
        difficulty_updates = {}
    touched_nodes: set[str] = set()
    for key in ("difficulty_updates", "semantic_dep_updates", "target_claim_updates", "proof_node_updates"):
        value = payload.get(key)
        if isinstance(value, dict):
            touched_nodes.update(str(node) for node in value.keys())
    return {
        "raw": payload,
        "acceptance": acceptance,
        "difficulty_updates": difficulty_updates,
        "touched_nodes": touched_nodes,
    }


def _live_activity_map(
    protocol_state: Dict[str, Any],
    node_names: Iterable[str],
    touched_nodes: set[str],
) -> Dict[str, Dict[str, bool]]:
    activity = {
        name: {
            "worker": False,
            "reviewer": False,
            "correspondence": False,
            "soundness": False,
            "substantiveness": False,
        }
        for name in node_names
    }
    request = protocol_state.get("in_flight_request")
    if not isinstance(request, dict):
        return activity
    kind = str(request.get("kind", "") or "").strip().lower()
    if kind == "worker":
        focus_nodes: List[str] = []
        active_node = str(request.get("active_node", "") or "").strip()
        held_target = str(request.get("held_target", "") or "").strip()
        if active_node:
            focus_nodes.append(active_node)
        elif held_target:
            focus_nodes.append(held_target)
        elif touched_nodes:
            focus_nodes.extend(sorted(touched_nodes))
        else:
            acceptance = request.get("worker_acceptance")
            if isinstance(acceptance, dict):
                focus_nodes.extend(str(n) for n in acceptance.get("authorized_nodes") or [])
        for name in focus_nodes:
            if name in activity:
                activity[name]["worker"] = True
    elif kind == "corr":
        for name in request.get("corr_verify_nodes") or []:
            if name in activity:
                activity[name]["correspondence"] = True
    elif kind == "sound":
        focus_nodes = list(request.get("sound_verify_nodes") or [])
        focus = str(request.get("sound_verify_node", "") or "").strip()
        if focus:
            focus_nodes.append(focus)
        for name in focus_nodes:
            if name in activity:
                activity[name]["soundness"] = True
    elif kind == "paper":
        # Paper-mode requests come in two flavors: target-mode (paper_verify_targets
        # populated) which exercises the paper-faithfulness lane against configured
        # targets, and per-node-mode (substantiveness_verify_nodes populated) which
        # exercises the substantiveness lane on individual frontier nodes. Both
        # flavors get rendered as substantiveness-active for the relevant nodes —
        # the viewer doesn't separately model paper-faithfulness as a per-node
        # activity since faithfulness scopes to whole-target coverings, not
        # individual nodes.
        for name in request.get("substantiveness_verify_nodes") or []:
            if name in activity:
                activity[name]["substantiveness"] = True
    elif kind == "review":
        focus_nodes: set[str] = set()
        active_node = str(request.get("active_node", "") or "").strip()
        if active_node:
            focus_nodes.add(active_node)
        for blocker in request.get("blockers") or []:
            if not isinstance(blocker, dict):
                continue
            obj = blocker.get("object")
            if isinstance(obj, dict):
                node = str(obj.get("node", "") or "").strip()
                if node:
                    focus_nodes.add(node)
        for name in focus_nodes:
            if name in activity:
                activity[name]["reviewer"] = True
    return activity


def _load_bridge_state(runtime_info: Optional[RuntimeInfo]) -> Dict[str, Dict[str, Any]]:
    result: Dict[str, Dict[str, Any]] = {}
    if runtime_info is None or runtime_info.bridge_dir is None:
        return result
    for name in (
        "latest_worker.json",
        "latest_paper.json",
        "latest_corr.json",
        "latest_sound.json",
        "latest_review.json",
    ):
        path = runtime_info.bridge_dir / name
        if not path.is_file():
            continue
        try:
            result[name] = _read_json(path)
        except Exception:
            continue
    result["corr_issues"] = _bridge_corr_issues(result.get("latest_corr.json", {}))
    result["sound_issues"] = _bridge_sound_issues(result.get("latest_sound.json", {}))
    return result


def _read_snapshot_json(snapshot: GitSnapshot | WorkingTreeSnapshot, rel_path: str) -> Dict[str, Any]:
    text = snapshot.read_text(rel_path)
    if not text.strip():
        return {}
    try:
        data = json.loads(text)
    except Exception:
        return {}
    return data if isinstance(data, dict) else {}


def _load_history_state(snapshot: GitSnapshot | WorkingTreeSnapshot) -> Dict[str, Any]:
    payload = _read_snapshot_json(snapshot, f".trellis-history/{SUPERVISOR_STATE_FILENAME}")
    state = payload.get("state")
    return state if isinstance(state, dict) else {}


def _load_history_bridge_state(snapshot: GitSnapshot | WorkingTreeSnapshot) -> Dict[str, Dict[str, Any]]:
    result = {
        "latest_worker.json": _read_snapshot_json(snapshot, f".trellis-history/{WORKER_HANDOFF_FILENAME}"),
        "latest_paper.json": _read_snapshot_json(snapshot, f".trellis-history/{PAPER_RESULT_FILENAME}"),
        "latest_corr.json": _read_snapshot_json(snapshot, f".trellis-history/{CORR_RESULT_FILENAME}"),
        "latest_sound.json": _read_snapshot_json(snapshot, f".trellis-history/{SOUND_RESULT_FILENAME}"),
        "latest_review.json": _read_snapshot_json(snapshot, f".trellis-history/{REVIEW_RESULT_FILENAME}"),
    }
    result = {name: payload for name, payload in result.items() if payload}
    result["corr_issues"] = _bridge_corr_issues(result.get("latest_corr.json", {}))
    result["sound_issues"] = _bridge_sound_issues(result.get("latest_sound.json", {}))
    return result


def _bridge_corr_issues(payload: Dict[str, Any]) -> Dict[str, str]:
    per_node: Dict[str, List[str]] = {}
    normalized = payload.get("normalized")
    node_lane_updates = normalized.get("node_lane_updates") if isinstance(normalized, dict) else {}
    reviewer_evidence = normalized.get("reviewer_evidence") if isinstance(normalized, dict) else {}
    if isinstance(node_lane_updates, dict):
        for lane_id, lane_updates in node_lane_updates.items():
            if not isinstance(lane_updates, dict):
                continue
            lane_evidence = reviewer_evidence.get(lane_id, {}) if isinstance(reviewer_evidence, dict) else {}
            corr_evidence = lane_evidence.get("correspondence") if isinstance(lane_evidence, dict) else {}
            issue_map: Dict[str, List[str]] = {}
            generic_issues: List[str] = []
            if isinstance(corr_evidence, dict):
                raw_issues = corr_evidence.get("issues")
                if isinstance(raw_issues, list):
                    for item in raw_issues:
                        if isinstance(item, dict):
                            node = str(item.get("node", "") or "").strip()
                            description = str(item.get("description", "") or "").strip()
                            if node and description:
                                issue_map.setdefault(node, []).append(description)
                        else:
                            text = str(item or "").strip()
                            if text:
                                generic_issues.append(text)
            lane_summary = str((lane_evidence or {}).get("summary", "") or "").strip() if isinstance(lane_evidence, dict) else ""
            lane_comments = str((lane_evidence or {}).get("comments", "") or "").strip() if isinstance(lane_evidence, dict) else ""
            for node_name, update in lane_updates.items():
                update_value = ""
                if isinstance(update, dict):
                    update_value = str(update.get("Set", "") or update.get("set", "") or "").strip().lower()
                else:
                    update_value = str(update or "").strip().lower()
                if update_value != "fail":
                    continue
                parts = issue_map.get(str(node_name), [])[:]
                if not parts and generic_issues:
                    parts.extend(generic_issues)
                if lane_summary:
                    parts.append(f"[{lane_id}] {lane_summary}")
                if lane_comments:
                    parts.append(lane_comments)
                if parts:
                    per_node.setdefault(str(node_name), []).extend(parts)
    if per_node:
        return {node: "\n\n".join(dict.fromkeys(parts)) for node, parts in per_node.items()}

    issues: Dict[str, List[str]] = {}
    member_responses = payload.get("member_responses")
    if not isinstance(member_responses, list):
        return {}
    for member in member_responses:
        response_payload = (((member or {}).get("payload") or {}).get("data") or {})
        if not isinstance(response_payload, dict):
            continue
        corr = response_payload.get("correspondence")
        comments = str(response_payload.get("comments", response_payload.get("feedback", "")) or "").strip()
        if not isinstance(corr, dict):
            continue
        if str(corr.get("decision", "") or "").strip().upper() == "PASS":
            continue
        # Post-2026-04-30: corr-node payloads carry `verdicts[]` (per-node
        # Pass/Fail with optional comment), mirroring the substantiveness lane.
        # Older payloads with the legacy `issues[]` shape still parse cleanly
        # for replay/historical viewer state, so accept either source — but
        # never both for the same node (a Fail verdict's comment is the
        # canonical issue text).
        raw_verdicts = corr.get("verdicts")
        if isinstance(raw_verdicts, list):
            for item in raw_verdicts:
                if not isinstance(item, dict):
                    continue
                if str(item.get("verdict", "") or "").strip() != "Fail":
                    continue
                node = str(item.get("node", "") or "").strip()
                description = str(item.get("comment", "") or "").strip()
                if node and description:
                    issues.setdefault(node, []).append(description)
                    if comments:
                        issues[node].append(comments)
        raw_issues = corr.get("issues")
        if isinstance(raw_issues, list):
            for item in raw_issues:
                if not isinstance(item, dict):
                    continue
                node = str(item.get("node", "") or "").strip()
                description = str(item.get("description", "") or "").strip()
                if node and description:
                    issues.setdefault(node, []).append(description)
                    if comments:
                        issues[node].append(comments)
        # If the verifier emitted neither verdicts nor issues but did set
        # `comments`, broadcast the lane-level commentary to every requested
        # node (preserves the legacy fallback when an agent says something
        # general but doesn't enumerate per-node detail).
        if (
            not isinstance(raw_verdicts, list)
            and not isinstance(raw_issues, list)
            and comments
        ):
            for node in payload.get("response", {}).get("verify_nodes", []) if isinstance(payload.get("response"), dict) else []:
                issues.setdefault(str(node), []).append(comments)
    return {node: "\n\n".join(dict.fromkeys(parts)) for node, parts in issues.items()}


def _bridge_sound_issues(payload: Dict[str, Any]) -> Dict[str, str]:
    issues: Dict[str, str] = {}
    member_responses = payload.get("member_responses")
    if not isinstance(member_responses, list):
        return issues
    for member in member_responses:
        response_payload = (((member or {}).get("payload") or {}).get("data") or {})
        if not isinstance(response_payload, dict):
            continue
        soundness = response_payload.get("soundness")
        node = str(response_payload.get("node", "") or "").strip()
        if not node or not isinstance(soundness, dict):
            continue
        decision = str(soundness.get("decision", "") or "").strip().upper()
        if decision == "SOUND":
            continue
        text = str(soundness.get("explanation", "") or "").strip()
        if text:
            issues[node] = text
    return issues


def _blank_activity_map(node_names: Iterable[str]) -> Dict[str, Dict[str, bool]]:
    return {
        name: {
            "worker": False,
            "reviewer": False,
            "correspondence": False,
            "soundness": False,
        }
        for name in node_names
    }


def _mark_activity(
    activity: Dict[str, Dict[str, bool]],
    *,
    nodes: Iterable[str],
    kind: str,
) -> None:
    for node in nodes:
        name = str(node or "").strip()
        if name in activity:
            activity[name][kind] = True


def _corr_artifact_nodes(payload: Dict[str, Any]) -> List[str]:
    nodes: set[str] = set()
    normalized = payload.get("normalized")
    if isinstance(normalized, dict):
        node_lane_updates = normalized.get("node_lane_updates")
        if isinstance(node_lane_updates, dict):
            for lane_updates in node_lane_updates.values():
                if isinstance(lane_updates, dict):
                    nodes.update(str(node) for node in lane_updates.keys())
    response = payload.get("response")
    if isinstance(response, dict):
        for key in ("verify_nodes", "corr_verify_nodes"):
            values = response.get(key)
            if isinstance(values, list):
                nodes.update(str(node) for node in values)
    return sorted(node for node in nodes if node)


def _sound_artifact_nodes(payload: Dict[str, Any]) -> List[str]:
    nodes: set[str] = set()
    response = payload.get("response")
    if isinstance(response, dict):
        for key in ("sound_verify_nodes", "verify_nodes"):
            values = response.get(key)
            if isinstance(values, list):
                nodes.update(str(node) for node in values)
        focus = str(response.get("sound_verify_node", "") or response.get("node", "") or "").strip()
        if focus:
            nodes.add(focus)
    normalized = payload.get("normalized")
    if isinstance(normalized, dict):
        focus = str(normalized.get("node", "") or "").strip()
        if focus:
            nodes.add(focus)
    return sorted(node for node in nodes if node)


def _review_artifact_task_nodes(payload: Dict[str, Any]) -> List[str]:
    nodes: set[str] = set()
    response = payload.get("response")
    if not isinstance(response, dict):
        return []
    next_active = str(response.get("next_active", "") or "").strip()
    if next_active:
        nodes.add(next_active)
    for blocker in response.get("task_blockers") or []:
        if not isinstance(blocker, dict):
            continue
        obj = blocker.get("object")
        if isinstance(obj, dict):
            node = str(obj.get("node", "") or "").strip()
            if node:
                nodes.add(node)
    return sorted(node for node in nodes if node)


def _historical_activity_map(
    *,
    snapshot: GitSnapshot | WorkingTreeSnapshot,
    protocol_state: Dict[str, Any],
    bridge_state: Dict[str, Dict[str, Any]],
) -> Dict[str, Dict[str, bool]]:
    live = protocol_state.get("live") if isinstance(protocol_state.get("live"), dict) else {}
    present_nodes = [str(name) for name in (live.get("present_nodes") or [])]
    node_names = sorted(set(present_nodes) | set(snapshot.list_tablet_node_names()))
    if "Preamble" not in node_names and snapshot.read_text("Tablet/Preamble.lean"):
        node_names.insert(0, "Preamble")
    activity = _blank_activity_map(node_names)
    _mark_activity(
        activity,
        nodes=_corr_artifact_nodes(bridge_state.get("latest_corr.json", {})),
        kind="correspondence",
    )
    _mark_activity(
        activity,
        nodes=_sound_artifact_nodes(bridge_state.get("latest_sound.json", {})),
        kind="soundness",
    )
    _mark_activity(
        activity,
        nodes=_review_artifact_task_nodes(bridge_state.get("latest_review.json", {})),
        kind="reviewer",
    )
    return activity


def _build_live_viewer_state(repo_path: Path) -> Dict[str, Any]:
    runtime_info = _load_runtime_info(repo_path)
    protocol_state: Dict[str, Any] = {}
    if runtime_info is not None:
        protocol_state = _read_json(runtime_info.protocol_state_path)
    bridge_state = _load_bridge_state(runtime_info)
    human_input = _read_text(repo_path / "HUMAN_INPUT.md")
    last_review = _extract_last_review(bridge_state.get("latest_review.json", {}))
    pending_task = protocol_state.get("pending_task") if isinstance(protocol_state.get("pending_task"), dict) else {}
    pending_node = _string_or_empty(pending_task.get("node")) if isinstance(pending_task, dict) else ""
    active_node = _string_or_empty(protocol_state.get("active_node")) or pending_node
    live_snapshot = protocol_state.get("live") if isinstance(protocol_state.get("live"), dict) else {}
    committed_snapshot = (
        protocol_state.get("committed") if isinstance(protocol_state.get("committed"), dict) else {}
    )

    state = {
        "phase": _legacy_phase_name(protocol_state.get("phase")),
        "cycle": int(protocol_state.get("cycle", 0) or 0),
        "active_node": active_node,
        "theorem_soundness_target": _string_or_empty(protocol_state.get("held_target")),
        "theorem_target_edit_mode": _legacy_mode_name(protocol_state.get("target_edit_mode")),
        "theorem_correspondence_blocked": _has_open_correspondence_blocker(protocol_state),
        "last_worker_handoff": _extract_last_worker_handoff(bridge_state.get("latest_worker.json", {})),
        "last_review": last_review,
        "open_blockers": _format_blockers((protocol_state.get("in_flight_request") or {}).get("blockers")),
        "open_rejections": _extract_open_rejections(bridge_state),
        "review_log": [],
        "validation_summary": _extract_validation_summary(bridge_state),
        "stuck_recovery_attempts": 0,
        "human_input": human_input,
        "human_input_at_cycle": int(protocol_state.get("cycle", 0) or 0) if protocol_state.get("human_input_outstanding") else None,
        "awaiting_human_input": _awaiting_human_input(protocol_state, runtime_info),
        "protected_reapproval_nodes": list(
            (protocol_state.get("in_flight_request") or {}).get("protected_reapproval_nodes") or []
        ),
        "cleanup_last_good_commit": "",
        "agent_token_usage": {},
        "resume_from": "",
        "in_flight_analysis": analyze_inflight_request(
            repo_path,
            runtime_root=runtime_info.root if runtime_info is not None else None,
        ),
        # Configured paper targets and the live coverage map (target → covering
        # node ids). Surfaced so the human-review zip's README can derive the
        # set of protected target roots (= union of covering nodes) and from
        # there the narrow Lean semantic closure that bounds what the human
        # reviewer is actually being asked to vouch for.
        "configured_targets": list(protocol_state.get("configured_targets") or []),
        # Coarse-DAG node set (nodes present when theorem-stating was approved).
        # Surfaced so the viewer's "Coarse + open only" DAG filter can identify
        # which nodes are protected coarse-DAG entries vs proof-phase helpers.
        # Empty for pre-implementation runs.
        "coarse_dag_nodes": list(protocol_state.get("coarse_dag_nodes") or []),
        "coverage": {
            str(target): list(nodes or [])
            for target, nodes in (live_snapshot.get("coverage") or {}).items()
        },
        "live": {
            "present_nodes": list(live_snapshot.get("present_nodes") or []),
            "open_nodes": _recompute_open_nodes_from_repo(
                live_snapshot.get("present_nodes") or [], repo_path
            ),
        },
        "committed": {
            "present_nodes": list(committed_snapshot.get("present_nodes") or []),
            "open_nodes": _recompute_open_nodes_from_repo(
                committed_snapshot.get("present_nodes") or [], repo_path
            ),
        },
        # Local-closure mirrors (Patches C-A through C-R-d): the viewer's
        # `augmentViewerStateClosure` (viewer/server.js:2682) reads these
        # to derive per-node `closure_status` and the top-level
        # `local_closure_summary`. Committed mirrors are preferred so the
        # coloring stays tier-consistent with `committed.open_nodes`;
        # live-tier fields are passed through as fallback for callers
        # that prefer them.
        "committed_local_closure_records": dict(
            protocol_state.get("committed_local_closure_records") or {}
        ),
        "committed_local_closure_unverified_nodes": list(
            protocol_state.get("committed_local_closure_unverified_nodes") or []
        ),
        "committed_local_closure_failures": dict(
            protocol_state.get("committed_local_closure_failures") or {}
        ),
        "local_closure_records": dict(
            protocol_state.get("local_closure_records") or {}
        ),
        "local_closure_unverified_nodes": list(
            protocol_state.get("local_closure_unverified_nodes") or []
        ),
        "local_closure_failures": dict(
            protocol_state.get("local_closure_failures") or {}
        ),
    }
    return {
        "state": state,
        "meta": {
            "source": _meta_source(protocol_state),
            "in_flight_cycle": int(protocol_state.get("cycle", 0) or 0),
            "cycle_checkpoint": None,
        },
        "nodes": _build_live_nodes(repo_path=repo_path, protocol_state=protocol_state, bridge_state=bridge_state),
    }


def _awaiting_human_input(
    protocol_state: Dict[str, Any],
    runtime_info: Optional["RuntimeInfo"] = None,
) -> bool:
    request = protocol_state.get("in_flight_request")
    if isinstance(request, dict) and _normalized_request_kind(request.get("kind")) == "human_gate":
        # A fresh HumanGate is currently in flight — always show. This branch
        # short-circuits the staleness check below, so a new gate at a later
        # cycle correctly re-shows the dialog even if a viewer-meta sidecar
        # from a prior approved gate is still on disk.
        return True
    if not bool(protocol_state.get("human_input_outstanding")):
        return False
    # `human_input_outstanding` is set but the in-flight request is no longer
    # a human_gate. If we've already recorded an approval/feedback for this
    # outstanding gate AND the supervisor has committed past that cycle,
    # suppress the dialog so the viewer dismisses it automatically.
    if _human_gate_response_stale(runtime_info, protocol_state):
        return False
    return True


def _meta_source(protocol_state: Dict[str, Any]) -> str:
    request = protocol_state.get("in_flight_request")
    if not isinstance(request, dict):
        return "live"
    kind = _normalized_request_kind(request.get("kind"))
    if kind == "worker":
        return "worker"
    if kind in {"corr", "sound"}:
        return "verification"
    if kind == "review":
        return "reviewer"
    if kind == "human_gate":
        return "cycle"
    return "live"


def _extract_last_worker_handoff(payload: Dict[str, Any]) -> Dict[str, Any]:
    raw = payload.get("raw")
    if not isinstance(raw, dict):
        return {}
    return {
        "outcome": str(raw.get("outcome", "") or ""),
        "summary": str(raw.get("summary", "") or ""),
        "comments": str(raw.get("comments", raw.get("feedback", "")) or ""),
    }


def _extract_last_review(payload: Dict[str, Any]) -> Dict[str, Any]:
    raw = payload.get("raw")
    response = payload.get("response")
    if isinstance(response, dict):
        return {
            "decision": _normalized_review_decision(response.get("decision")),
            "reason": str((raw or {}).get("reason", "") or ""),
        }
    if isinstance(raw, dict):
        return {
            "decision": _normalized_review_decision(raw.get("decision")),
            "reason": str(raw.get("reason", "") or ""),
        }
    return {}


def _normalized_request_kind(value: Any) -> str:
    raw = str(value or "").strip().lower()
    token = re.sub(r"[^a-z0-9]+", "", raw)
    normalized = {
        "worker": "worker",
        "paper": "paper",
        "corr": "corr",
        "sound": "sound",
        "review": "review",
        "humangate": "human_gate",
    }.get(token)
    return normalized or raw


def _normalized_review_decision(value: Any) -> str:
    raw = str(value or "").strip()
    if not raw:
        return ""
    snake = re.sub(r"(?<!^)(?=[A-Z])", "_", raw).replace("-", "_").replace(" ", "_")
    snake = re.sub(r"_+", "_", snake).strip("_").lower()
    return snake


def _extract_validation_summary(bridge_state: Dict[str, Dict[str, Any]]) -> Dict[str, Any]:
    worker = bridge_state.get("latest_worker.json", {})
    raw = worker.get("raw")
    if not isinstance(raw, dict):
        return {}
    return {
        "outcome": str(raw.get("outcome", "") or ""),
        "summary": str(raw.get("summary", "") or ""),
        "validation_errors": [
            str(item) for item in worker.get("validation_errors", []) or []
        ],
    }


def _extract_open_rejections(bridge_state: Dict[str, Dict[str, Any]]) -> List[str]:
    worker = bridge_state.get("latest_worker.json", {})
    final_outcome = str(worker.get("final_outcome", "") or "").strip().lower()
    if final_outcome not in {"invalid", "malformed"}:
        return []
    errors: List[str] = []
    for key in ("validation_errors", "contract_errors", "errors"):
        for item in worker.get(key, []) or []:
            text = str(item or "").strip()
            if text:
                errors.append(text)
    return list(dict.fromkeys(errors))


def _historical_open_blockers(
    protocol_state: Dict[str, Any],
    bridge_state: Dict[str, Dict[str, Any]],
) -> List[str]:
    live_blockers = _format_blockers((protocol_state.get("in_flight_request") or {}).get("blockers"))
    if live_blockers:
        return live_blockers
    review = bridge_state.get("latest_review.json", {})
    response = review.get("response")
    if not isinstance(response, dict):
        return []
    blockers = []
    blockers.extend(response.get("task_blockers") or [])
    blockers.extend(response.get("reset_blockers") or [])
    return _format_blockers(blockers)


def _string_or_empty(value: Any) -> str:
    return str(value or "").strip()


def _has_open_correspondence_blocker(protocol_state: Dict[str, Any]) -> bool:
    request = protocol_state.get("in_flight_request")
    blockers = request.get("blockers") if isinstance(request, dict) else []
    if not isinstance(blockers, list):
        return False
    for blocker in blockers:
        if not isinstance(blocker, dict):
            continue
        kind = str(blocker.get("kind", "") or "").strip().lower()
        if kind in {"correspondence", "targetcorrespondence", "corr", "targetcorr"}:
            return True
    return False


def _format_blockers(raw_blockers: Any) -> List[str]:
    blockers: List[str] = []
    if not isinstance(raw_blockers, list):
        return blockers
    for blocker in raw_blockers:
        if not isinstance(blocker, dict):
            continue
        kind = str(blocker.get("kind", "") or "").strip()
        obj = blocker.get("object")
        if isinstance(obj, dict):
            target = obj.get("target")
            node = obj.get("node")
            if target:
                blockers.append(f"{kind}:{target}")
                continue
            if node:
                blockers.append(f"{kind}:{node}")
                continue
        blockers.append(kind or json.dumps(blocker, sort_keys=True))
    return blockers


def _list_cycles(repo_path: Path) -> List[Dict[str, Any]]:
    try:
        tags = _git(repo_path, "tag", "-l", "supervisor2/checkpoint-*", "--sort=refname").stdout.splitlines()
    except Exception:
        return []
    by_cycle: Dict[int, Dict[str, Any]] = {}
    for tag in tags:
        match = _CHECKPOINT_TAG_RE.fullmatch(tag.strip())
        if not match:
            continue
        event_count = int(match.group(1))
        subject = _git(repo_path, "log", "-1", "--format=%s", tag, check=False).stdout.strip()
        parsed = _parse_checkpoint_subject(subject)
        if parsed is None:
            continue
        cycle, phase, outcome, _active = parsed
        existing = by_cycle.get(cycle)
        if existing is None or event_count > int(existing.get("_event_count", -1)):
            by_cycle[cycle] = {
                "cycle": cycle,
                "phase": phase,
                "outcome": outcome,
                "_event_count": event_count,
                "_tag": tag,
            }
    result = []
    for cycle in sorted(by_cycle):
        entry = dict(by_cycle[cycle])
        entry.pop("_event_count", None)
        entry.pop("_tag", None)
        result.append(entry)
    return result


def _parse_checkpoint_subject(subject: str) -> Optional[tuple[int, str, str, str]]:
    match = _CHECKPOINT_SUBJECT_RE.fullmatch(subject.strip())
    if not match:
        return None
    cycle = int(match.group(1))
    phase = _legacy_phase_name((match.group(2) or "").strip())
    outcome = (match.group(3) or "").strip()
    active = (match.group(4) or "").strip()
    return (cycle, phase, outcome, active)


def _build_historical_viewer_state(repo_path: Path, cycle: int) -> Dict[str, Any]:
    cycles = _list_cycles(repo_path)
    cycle_entry = next((entry for entry in cycles if int(entry.get("cycle", -1)) == cycle), None)
    if cycle_entry is None:
        raise FileNotFoundError(f"no checkpoint for cycle {cycle}")
    try:
        tags = _git(repo_path, "tag", "-l", "supervisor2/checkpoint-*", "--sort=refname").stdout.splitlines()
    except Exception as exc:
        raise FileNotFoundError(f"cannot list checkpoints for cycle {cycle}: {exc}") from exc
    chosen_tag: Optional[str] = None
    chosen_event = -1
    for tag in tags:
        match = _CHECKPOINT_TAG_RE.fullmatch(tag.strip())
        if not match:
            continue
        event_count = int(match.group(1))
        subject = _git(repo_path, "log", "-1", "--format=%s", tag, check=False).stdout.strip()
        parsed = _parse_checkpoint_subject(subject)
        if parsed is None or parsed[0] != cycle:
            continue
        if event_count > chosen_event:
            chosen_event = event_count
            chosen_tag = tag.strip()
    if not chosen_tag:
        raise FileNotFoundError(f"no checkpoint tag for cycle {cycle}")
    snapshot = GitSnapshot(repo_path=repo_path, ref=chosen_tag)
    protocol_state = _load_history_state(snapshot)
    bridge_state = _load_history_bridge_state(snapshot)
    if protocol_state:
        activity_override = _historical_activity_map(
            snapshot=snapshot,
            protocol_state=protocol_state,
            bridge_state=bridge_state,
        )
        human_input = ""
        last_review = _extract_last_review(bridge_state.get("latest_review.json", {}))
        pending_task = (
            protocol_state.get("pending_task")
            if isinstance(protocol_state.get("pending_task"), dict)
            else {}
        )
        pending_node = _string_or_empty(pending_task.get("node")) if isinstance(pending_task, dict) else ""
        active_node = _string_or_empty(protocol_state.get("active_node")) or pending_node
        live_snapshot = protocol_state.get("live") if isinstance(protocol_state.get("live"), dict) else {}
        committed_snapshot = (
            protocol_state.get("committed") if isinstance(protocol_state.get("committed"), dict) else {}
        )
        return {
            "state": {
                "phase": _legacy_phase_name(protocol_state.get("phase")),
                "cycle": int(protocol_state.get("cycle", cycle) or cycle),
                "active_node": active_node,
                "theorem_soundness_target": _string_or_empty(protocol_state.get("held_target")),
                "theorem_target_edit_mode": _legacy_mode_name(protocol_state.get("target_edit_mode")),
                "theorem_correspondence_blocked": _has_open_correspondence_blocker(protocol_state),
                "last_worker_handoff": _extract_last_worker_handoff(bridge_state.get("latest_worker.json", {})),
                "last_review": last_review,
                "open_blockers": _historical_open_blockers(protocol_state, bridge_state),
                "open_rejections": _extract_open_rejections(bridge_state),
                "review_log": [],
                "validation_summary": _extract_validation_summary(bridge_state),
                "stuck_recovery_attempts": 0,
                "human_input": human_input,
                "human_input_at_cycle": None,
                "awaiting_human_input": _awaiting_human_input(protocol_state),
                "cleanup_last_good_commit": "",
                "agent_token_usage": {},
                "resume_from": "",
                "live": {
                    "present_nodes": list(live_snapshot.get("present_nodes") or []),
                    "open_nodes": list(live_snapshot.get("open_nodes") or []),
                },
                "committed": {
                    "present_nodes": list(committed_snapshot.get("present_nodes") or []),
                    "open_nodes": list(committed_snapshot.get("open_nodes") or []),
                },
                "coarse_dag_nodes": list(protocol_state.get("coarse_dag_nodes") or []),
            },
            "meta": {
                "source": "git",
                "in_flight_cycle": int(protocol_state.get("cycle", cycle) or cycle),
                "cycle_checkpoint": chosen_tag,
            },
            "nodes": _build_nodes_from_snapshot(
                snapshot=snapshot,
                protocol_state=protocol_state,
                bridge_state=bridge_state,
                activity_override=activity_override,
            ),
        }
    node_names = snapshot.list_tablet_node_names()
    nodes: Dict[str, Dict[str, Any]] = {}
    for node_name in node_names:
        lean_content = snapshot.read_text(f"Tablet/{node_name}.lean")
        tex_content = snapshot.read_text(f"Tablet/{node_name}.tex")
        title, tex_env = _extract_title_and_env(tex_content)
        viewer_kind = _viewer_kind(node_name, None, tex_env, lean_content)
        nodes[node_name] = {
            "status": "closed" if not _file_has_real_sorry(lean_content) else "open",
            "kind": viewer_kind,
            "imports": _extract_imports(lean_content),
            "difficulty": "hard",
            "title": title or node_name,
            "texEnv": tex_env,
            "hasSorry": _file_has_real_sorry(lean_content),
            "declaration": _extract_declaration_preview(lean_content),
            "texContent": tex_content,
            "leanContent": lean_content,
            "verification": {
                "correspondence": "?",
                "nl_proof": "pass" if viewer_kind in {"definition", "preamble"} else "?",
            },
            "activity": {
                "worker": False,
                "reviewer": False,
                "correspondence": False,
                "soundness": False,
            },
        }
    return {
        "state": {
            "phase": str(cycle_entry.get("phase", "") or ""),
            "cycle": cycle,
            "active_node": "",
            "theorem_soundness_target": "",
            "theorem_target_edit_mode": "",
            "theorem_correspondence_blocked": False,
            "last_worker_handoff": {},
            "last_review": {},
            "open_blockers": [],
            "open_rejections": [],
            "review_log": [],
            "validation_summary": {},
            "stuck_recovery_attempts": 0,
            "human_input": "",
            "human_input_at_cycle": None,
            "awaiting_human_input": False,
            "cleanup_last_good_commit": "",
            "agent_token_usage": {},
            "resume_from": "",
        },
        "meta": {
            "source": "git",
            "in_flight_cycle": cycle,
            "cycle_checkpoint": chosen_tag,
        },
        "nodes": nodes,
    }


def _feedback_get(repo_path: Path) -> Dict[str, Any]:
    runtime_info = _load_runtime_info(repo_path)
    protocol_state = _read_json(runtime_info.protocol_state_path) if runtime_info else {}
    bridge_state = _load_bridge_state(runtime_info)
    last_review = _extract_last_review(bridge_state.get("latest_review.json", {}))
    return {
        "awaiting_input": _awaiting_human_input(protocol_state, runtime_info),
        "phase": _legacy_phase_name(protocol_state.get("phase")),
        "last_review_decision": str(last_review.get("decision", "") or ""),
        "last_review_reason": str(last_review.get("reason", "") or ""),
        "human_input": _read_text(repo_path / "HUMAN_INPUT.md"),
    }


_HUMAN_GATE_VIEWER_META_NAME = "human_gate_response.viewer_meta.json"


def _feedback_post(repo_path: Path, *, action: str, feedback: str) -> Dict[str, Any]:
    runtime_info = _load_runtime_info(repo_path)
    if runtime_info is None:
        raise RuntimeError("no active runtime found for project")
    normalized_action = str(action or "").strip().lower()
    if normalized_action not in {"approve", "feedback"}:
        raise RuntimeError('action must be "approve" or "feedback"')
    if normalized_action == "feedback":
        (repo_path / "HUMAN_INPUT.md").write_text(feedback or "", encoding="utf-8")
    response_path = runtime_info.root / "human_gate_response.json"
    response_path.write_text(
        json.dumps({"choice": normalized_action}, indent=2) + "\n",
        encoding="utf-8",
    )
    # Viewer-only sidecar: stamp the cycle at which this approval/feedback was
    # written so the viewer can dismiss the human-input dialog once the
    # supervisor has committed at least one full cycle past it. The kernel /
    # bridge ignore this file (only `human_gate_response.json` is consumed),
    # so adding fields here is safe.
    try:
        protocol_state = _read_json(runtime_info.protocol_state_path)
    except Exception:
        protocol_state = {}
    approved_cycle = int(protocol_state.get("cycle", 0) or 0)
    meta_path = runtime_info.root / _HUMAN_GATE_VIEWER_META_NAME
    try:
        meta_path.write_text(
            json.dumps(
                {
                    "choice": normalized_action,
                    "cycle": approved_cycle,
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
    except Exception:
        # Sidecar is best-effort — never fail the approval over it.
        pass
    if normalized_action == "approve":
        return {"ok": True, "message": "Approval signal written. Runtime will continue."}
    return {"ok": True, "message": "Feedback written. Runtime will continue."}


def _human_gate_response_stale(
    runtime_info: Optional["RuntimeInfo"], protocol_state: Dict[str, Any]
) -> bool:
    """Return True iff a prior viewer approval/feedback has been recorded AND
    the supervisor has advanced past its cycle. Used to dismiss the human-
    input dialog once one full cycle has committed post-approval.

    Returns False (i.e. NOT stale) when:
      - no sidecar exists (no prior approval to dismiss), OR
      - sidecar cycle >= current cycle (the gate-cycle hasn't fully elapsed).

    A fresh HumanGate is never affected because `_awaiting_human_input`
    checks `in_flight_request.kind == 'human_gate'` first and only consults
    this helper for the `human_input_outstanding` fallback path.
    """
    if runtime_info is None:
        return False
    meta_path = runtime_info.root / _HUMAN_GATE_VIEWER_META_NAME
    if not meta_path.is_file():
        return False
    try:
        meta = json.loads(meta_path.read_text(encoding="utf-8"))
    except Exception:
        return False
    try:
        approved_cycle = int(meta.get("cycle", 0) or 0)
    except (TypeError, ValueError):
        return False
    current_cycle = int(protocol_state.get("cycle", 0) or 0)
    return current_cycle > approved_cycle


def _artifact_title(name: str) -> str:
    text = str(name or "").strip().replace("_", " ")
    text = re.sub(r"\s+", " ", text)
    return text or "Chat"


def _live_chats(repo_path: Path, cycle: int) -> Dict[str, Any]:
    runtime_info = _load_runtime_info(repo_path)
    request_cycles: Optional[Dict[int, int]] = None
    if runtime_info is not None:
        request_cycles = _request_cycles_from_event_log(runtime_info.root / "event_log.jsonl")
    return read_runtime_live_chats(repo_path, cycle, request_cycles=request_cycles)


def _semantic_closure_for_nodes(
    repo_path: Path, node_names: Iterable[str]
) -> Dict[str, Any]:
    """Return the narrow Lean semantic closure for each named node.

    Reads the most-recent successful sidecar from
    `<runtime>/checker-state/semantic-payloads/`. The sidecar's `payload`
    field is the `||`-separated output of
    `scripts/lean_semantic_fingerprint.lean`: `root|<seed>` followed by
    `const|<name>|<kind>|...` lines for each Tablet const reached, and
    `extern|<name>` lines at the Mathlib boundary.

    The walk enters a theorem's `type` only (not its proof body) and a
    definition's `type` AND `value`. Result is the set of *project-defined*
    names (Tablet.* per the script's `isTabletConst` filter) whose
    meaning-bearing change would shift the seed's hash. Lean-elaborator-
    generated artefacts (`<Foo>._proof_*`, `<Foo>.match_*`,
    `<Foo>._cstage_*`, `<Foo>._eq_*`, recursors) are aggregated under
    their parent top-level Tablet name — they're not user-authored.
    """
    requested = sorted({str(n).strip() for n in node_names if str(n).strip()})
    if not requested:
        return {"closures": {}, "ok": True}

    runtime_info = _load_runtime_info(repo_path)
    if runtime_info is None:
        return {"closures": {}, "ok": False, "error": "no active runtime found"}
    payload_dir = runtime_info.root / "checker-state" / "semantic-payloads"
    if not payload_dir.is_dir():
        return {
            "closures": {n: None for n in requested},
            "ok": False,
            "error": f"semantic-payloads cache not present at {payload_dir}",
        }

    # Group sidecars by node_name, pick most recent successful one.
    latest_by_node: Dict[str, Dict[str, Any]] = {}
    for sidecar in payload_dir.glob("*.json"):
        try:
            entry = json.loads(sidecar.read_text(encoding="utf-8"))
        except Exception:
            continue
        if not entry.get("ok"):
            continue
        node = str(entry.get("node_name") or "").strip()
        if node not in requested:
            continue
        ts = float(entry.get("created_ts") or 0.0)
        prior = latest_by_node.get(node)
        if prior is None or ts > float(prior.get("created_ts") or 0.0):
            latest_by_node[node] = entry

    closures: Dict[str, Any] = {}
    for node in requested:
        entry = latest_by_node.get(node)
        if entry is None:
            closures[node] = None
            continue
        payload_text = str(entry.get("payload") or "")
        # Per-line entries are joined with `||` (see fingerprintPayloadFor).
        # Top-level Tablet projects: aggregate `Foo._proof_1` / `Foo.match_1`
        # / `Foo._cstage_*` / `Foo._eq_*` under `Foo`. Plain top-level
        # `const|Foo|...` lines stay as `Foo`. We deliberately do NOT
        # aggregate user-authored compound names like `def Foo.bar` —
        # but those are unusual in Tablet and the closure surface is for
        # human reviewers who can recognise them in the original file.
        node_names_raw: set[str] = set()
        for chunk in payload_text.split("||"):
            chunk = chunk.strip()
            if not chunk.startswith("const|"):
                continue
            after = chunk[len("const|"):]
            name = after.split("|", 1)[0].strip()
            if not name:
                continue
            # Skip the seed itself; the human reviewer already has it
            # in the protected set.
            if name == node:
                continue
            # Aggregate Lean-internal artefacts under the top-level
            # Tablet name (the file the user authored).
            top = name.split(".", 1)[0]
            if top:
                node_names_raw.add(top)
        closures[node] = sorted(node_names_raw)
    return {"closures": closures, "ok": True}


def main() -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)

    live_state = sub.add_parser("live-state")
    live_state.add_argument("repo_path")

    cycles = sub.add_parser("cycles")
    cycles.add_argument("repo_path")

    state_at = sub.add_parser("state-at")
    state_at.add_argument("repo_path")
    state_at.add_argument("cycle", type=int)

    chats = sub.add_parser("chats")
    chats.add_argument("repo_path")

    chats_at = sub.add_parser("chats-at")
    chats_at.add_argument("repo_path")
    chats_at.add_argument("cycle", type=int)

    feedback_get = sub.add_parser("feedback-get")
    feedback_get.add_argument("repo_path")

    feedback_post = sub.add_parser("feedback-post")
    feedback_post.add_argument("repo_path")
    feedback_post.add_argument("action")
    feedback_post.add_argument("--feedback", default="")

    prompts_catalog = sub.add_parser("prompts-catalog")
    prompts_catalog.add_argument("repo_path")

    prompts_render = sub.add_parser("prompts-render")
    prompts_render.add_argument("repo_path")
    prompts_render.add_argument("scenario_id")

    semantic_closure = sub.add_parser("semantic-closure")
    semantic_closure.add_argument("repo_path")
    semantic_closure.add_argument(
        "--node",
        action="append",
        default=[],
        help="Tablet node to compute the narrow Lean semantic closure for. "
             "Repeatable.",
    )

    args = parser.parse_args()
    repo_path = Path(args.repo_path).resolve()
    payload: Any

    if args.command == "live-state":
        payload = _build_live_viewer_state(repo_path)
    elif args.command == "cycles":
        payload = _list_cycles(repo_path)
    elif args.command == "state-at":
        payload = _build_historical_viewer_state(repo_path, args.cycle)
    elif args.command == "chats":
        live_state = _build_live_viewer_state(repo_path)
        cycle = int((live_state.get("state") or {}).get("cycle", 0) or 0)
        payload = _live_chats(repo_path, cycle)
    elif args.command == "chats-at":
        payload = read_historical_chats(repo_path, args.cycle)
    elif args.command == "feedback-get":
        payload = _feedback_get(repo_path)
    elif args.command == "feedback-post":
        payload = _feedback_post(repo_path, action=args.action, feedback=args.feedback)
    elif args.command == "prompts-catalog":
        payload = list_prompt_scenarios(repo_path)
    elif args.command == "prompts-render":
        payload = render_prompt_scenario(repo_path, args.scenario_id)
    elif args.command == "semantic-closure":
        payload = _semantic_closure_for_nodes(repo_path, args.node)
    else:  # pragma: no cover - argparse guards this
        raise RuntimeError(f"unknown command {args.command!r}")

    json.dump(payload, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":  # pragma: no cover - CLI entrypoint
    raise SystemExit(main())
