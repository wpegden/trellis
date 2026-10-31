"""Select compatible predecessor source for a fresh PV Modeling draft.

The native genesis importer separately admits retained verification evidence.
This source-selection report cannot authorize a closure or verifier verdict.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import re

from scripts.extract_pv_model import _strip_lean_comments
from trellis.json_io import save_json
from trellis.modeling.axioms import CANONICAL_AXIOMS
from trellis.modeling.modules import imports
from trellis.pv_model_support import (
    TYPE_LAYOUT, boundary_inventory, compose_model_support, sync_model_support,
)
from trellis.pv_revision.store import source_manifest

REPORT = Path("modeling-input/preserved-dag.json")
_BOUNDARY_FIELDS = ("name", "extracted_signature", "rust_symbol", "raw_declaration_sha256")


def _read(path: Path) -> dict:
    return json.loads(path.read_bytes())


def _code(text: str) -> str:
    # Source-location comments change after unrelated Rust edits. Preserve all
    # executable text, including whitespace and literals; this is only a reuse
    # filter and cannot authorize a proof or replace semantic checking.
    return _strip_lean_comments(text.splitlines()).strip()


def _sha(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def plan_preservation(repo: Path) -> dict | None:
    marker = repo / "modeling-input/revision-from.json"
    if not marker.is_file():
        return None
    receipt = _read(marker)
    if receipt.get("schema") != "trellis-pv-revision-successor/v1":
        raise ValueError("Unsupported predecessor revision record")
    record = receipt["predecessor"]
    previous = Path(record["repo"])
    revision = Path(receipt["revision_root"])
    frozen = revision / "predecessor"
    raw_state = (frozen / "protocol_state.json").read_bytes()
    if _sha(raw_state) != record["protocol_state_sha256"]:
        raise ValueError("Preserved predecessor state differs from the revision record")
    if source_manifest(frozen / "Tablet") != record["tablet_manifest"]:
        raise ValueError("Preserved predecessor Tablet differs from the revision record")
    package = _read(revision / "reviews" / receipt["package_digest"] / "review.json")
    if package["digest"] != receipt["package_digest"]:
        raise ValueError("Preservation review package differs from the successor record")
    state = json.loads(raw_state)
    present = set(state["live"]["present_nodes"])
    old_models = {item["node"]: item for item in _read(previous / "trellis.config.json")["pv_tablet"]["extraction_models"]}
    new_models = {item["node"]: item for item in _read(repo / "trellis.config.json")["pv_tablet"]["extraction_models"]}
    ordinary = present - old_models.keys() - {"Preamble", "Assumptions"}
    blocked: dict[str, dict] = {}
    revalidation: dict[str, dict] = {}
    changed_models = set()
    graph = {node: set(state.get("deps", {}).get(node, [])) for node in present}
    for node in sorted(present):
        path = frozen / "Tablet" / f"{node}.lean"
        if not path.is_file() or (node in ordinary and not path.with_suffix(".tex").is_file()):
            blocked[node] = {"reason": "missing paired predecessor source"}
            continue
        graph[node].update(module[7:] for module in imports(path.read_text()) if module.startswith("Tablet."))
        absent = graph[node] - present
        if absent:
            blocked[node] = {"reason": "missing predecessor dependencies", "dependencies": sorted(absent)}
    for node, model in old_models.items():
        current = new_models.get(node)
        if current is None:
            blocked[node] = {"reason": "extracted declaration removed"}
        elif (_code(model["lean"]) != _code(current["lean"])
                or model.get("namespace_context") != current.get("namespace_context")):
            changed_models.add(node)
            revalidation[node] = {"reason": "extracted declaration changed; fresh source replaces predecessor"}
    # A changed generated declaration still exists in the new program. Its
    # *current* imports, not the old execution graph, determine which authored
    # files can be retained. Native evidence admission remains independent.
    for node, model in new_models.items():
        path = repo / "Tablet" / f"{node}.lean"
        graph[node] = ({module[7:] for module in imports(path.read_text()) if module.startswith("Tablet.")}
                       if path.is_file() else set(model.get("model_dependencies", [])))
    available = present | new_models.keys()

    findings = {item["node"]: item.get("reason", "revision audit finding")
                for item in package.get("audit_invalidation", [])}
    before = package.get("original_collection", {"goals": package.get("original_goals", [])})
    after = package.get("proposed_collection", {"goals": package.get("proposed_goals", [])})
    new_goals = {goal["id"]: goal for goal in after["goals"]}
    changed_goals = {goal["id"] for goal in before["goals"]
                     if before.get("context", "") != after.get("context", "")
                     or new_goals.get(goal["id"]) != goal}
    policy_path = repo / "APPROVED_AXIOMS.json"
    policy = _read(policy_path) if policy_path.is_file() else {}
    for node in sorted(ordinary):
        claims = set(state.get("target_claims", {}).get(node, []))
        affected = claims & (changed_goals | findings.keys())
        if affected:
            revalidation[node] = {"reason": "changed or audit-affected goal requires fresh verification; source selection grants no approval",
                                  "goals": sorted(affected)}
        if changed := claims & changed_goals:
            blocked[node] = {"reason": "requirement changed or retired; predecessor authoring remains historical reference",
                             "goals": sorted(changed)}
        # The fresh positive task must not import its old counterexample as a
        # principal theorem (nor collide with the ordinary dormant pair store).
        negative = {goal for primary, goal in state.get("authored_decide_goals", {}).items()
                    if state.get("pv_live_polarity", {}).get(primary) == "disprove"
                    and state.get("configured_challenge_targets", {}).get(primary, {}).get("name") != node}
        if claims & negative:
            blocked[node] = {"reason": "predecessor negative carrier; retained as historical reference, not fresh positive authoring",
                             "goals": sorted(claims & negative)}
        closure = state.get("local_closure_records", {}).get(node)
        allowed = set(CANONICAL_AXIOMS) | set(policy.get("global", [])) | set(policy.get("nodes", {}).get(node, []))
        if closure and (extra := set(closure.get("kernel_axioms", [])) - allowed - {"sorryAx"}):
            blocked[node] = {"reason": "requires predecessor assumptions", "axioms": sorted(extra)}

    raw = (repo / "generated.lean").read_text()
    boundary = boundary_inventory(raw)
    support = _read(frozen / "Tablet/model-support.json")
    matched = [binding for binding in support["bindings"] if any(
        all(binding[key] == item[key] for key in _BOUNDARY_FIELDS) for item in boundary)]
    for binding in support["bindings"]:
        if binding not in matched:
            blocked[binding["node"]] = {"reason": "primitive binding changed or removed", "binding": binding["name"]}
    native = _read(repo / "modeling-input/manifest.json")

    while True:
        changed = False
        for node in sorted(available - blocked.keys()):
            affected = graph[node] & blocked.keys() | (graph[node] - available)
            if affected:
                blocked[node] = {"reason": "affected dependency", "dependencies": sorted(affected)}
                changed = True
        kept = ordinary - blocked.keys()
        selected = {"schema": support["schema"],
                    "imports": [node for node in support["imports"] if node in kept],
                    "bindings": [binding for binding in matched if binding["node"] in kept]}
        composed = compose_model_support(raw, selected, repo=frozen,
            lean_imports=native.get("lean_imports", []),
            expose_types=native.get("model_support_layout") == TYPE_LAYOUT)
        if ("Preamble" not in revalidation and
                _code(composed["preamble"]) != _code((frozen / "Tablet/Preamble.lean").read_text())):
            revalidation["Preamble"] = {"reason": "model support or extracted preamble changed"}
        if not changed:
            break

    authored = sorted(ordinary - blocked.keys())
    # Fresh generated Lean always wins. Applicable preservation prose can be
    # reused, but remains subject to the new generation's correspondence audit.
    model_prose = sorted(node for node in (old_models.keys() & new_models.keys()) - blocked.keys() - changed_models
                         if (frozen / "Tablet" / f"{node}.tex").is_file())
    if "Preamble" not in blocked and "Preamble" not in revalidation and (frozen / "Tablet/Preamble.tex").is_file():
        model_prose.append("Preamble")
    files = {}
    for node in authored:
        for suffix in ("lean", "tex"):
            name = f"Tablet/{node}.{suffix}"
            files[name] = {"source": str(frozen / name), "sha256": _sha((frozen / name).read_bytes())}
    for node in model_prose:
        name = f"Tablet/{node}.tex"
        files[name] = {"source": str(frozen / name), "sha256": _sha((frozen / name).read_bytes())}
    # Copy only cited evidence, not old build trees or complete runtime logs.
    evidence = set()
    for name, item in files.items():
        if name.endswith(".tex"):
            evidence.update(re.findall(r"\\evidenceref\{([^{}]+)\}", Path(item["source"]).read_text()))
    for name in sorted(evidence):
        relative = Path(name)
        if relative.is_absolute() or ".." in relative.parts or not name.startswith("reference/"):
            raise ValueError(f"Invalid predecessor evidence path: {name}")
        source = previous / relative
        if not source.is_file() or not source.resolve().is_relative_to((previous / "reference").resolve()):
            raise ValueError(f"Missing predecessor evidence: {name}")
        files[name] = {"source": str(source), "sha256": _sha(source.read_bytes())}
    return {"schema": "trellis-preserved-dag/v1", "package_digest": receipt["package_digest"],
            "predecessor_repo": str(previous), "predecessor_state_sha256": _sha(raw_state),
            "authored_nodes": authored, "generated_prose_nodes": model_prose,
            "node_kinds": {node: state["node_kinds"][node] for node in authored},
            "dependencies": {node: sorted(graph[node]) for node in authored},
            "excluded": {node: blocked[node] for node in sorted(blocked)},
            "authoring_requires_revalidation": {node: revalidation[node] for node in sorted(revalidation)},
            "model_support": selected, "files": files,
            "verification": "Native genesis checks each retained closure and verifier result separately; source reuse alone grants no approval."}


def preserve_successor_dag(repo: Path) -> dict | None:
    """Seed source before native initialization; fail rather than overwrite work."""
    plan = plan_preservation(repo)
    if plan is None:
        return None
    previous_report = repo / REPORT
    if previous_report.is_file() and _read(previous_report) != plan:
        raise ValueError("Successor preservation inputs changed")
    authored = set(plan["authored_nodes"])
    for name, item in plan["files"].items():
        raw = Path(item["source"]).read_bytes()
        if _sha(raw) != item["sha256"]:
            raise ValueError(f"Predecessor changed during preservation: {name}")
        target = repo / name
        if target.exists() and (target.parts[-2] != "Tablet" or target.stem in authored):
            if target.read_bytes() != raw:
                raise ValueError(f"Preservation would overwrite successor work: {name}")
    for name, item in plan["files"].items():
        target = repo / name
        target.parent.mkdir(parents=True, exist_ok=True)
        raw = Path(item["source"]).read_bytes()
        if _sha(raw) != item["sha256"]:
            raise ValueError(f"Predecessor changed during preservation: {name}")
        target.write_bytes(raw)
    save_json(repo / "Tablet/model-support.json", plan["model_support"])
    sync_model_support(repo, write=True)
    save_json(previous_report, plan)
    marker = repo / "modeling-input/revision-from.json"
    receipt = _read(marker)
    receipt["revalidation"] = ("Reuse prior proofs where applicable. The successor preserves verification only "
                               "where its inputs still match; use current runtime status for outstanding checks.")
    save_json(marker, receipt)
    return plan
