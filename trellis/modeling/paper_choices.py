"""Derived preparation proposals using the existing modeling-route authority.

The original paper intake stays immutable. Proposals authorize nothing; export
requires current route review and native Modeling independently reviews support.
"""
from __future__ import annotations

import hashlib
import re
from pathlib import Path

from trellis.modeling.observations import read_json, write_json
from trellis.paper_implementation import canonical_json, evidence_path, relative_path

STORE = Path(".trellis/modeling/paper-choices")
FIELDS = {"node", "source_locator", "rationale", "evidence_path", "consequential"}
MAX_EVIDENCE_BYTES = 16 * 1024 * 1024
MAX_TOTAL_EVIDENCE_BYTES = 64 * 1024 * 1024


def _hash(value) -> str:
    return hashlib.sha256(canonical_json(value)).hexdigest()


def _validate(choices: object) -> list[dict]:
    if not isinstance(choices, list):
        raise ValueError("modeling_choice_proposals must be an array")
    seen = set()
    for choice in choices:
        if not isinstance(choice, dict) or set(choice) != FIELDS:
            raise ValueError("Modeling proposals require source evidence and rationale; approvals and trust fields are not accepted")
        if any(not isinstance(choice.get(k), str) or not choice[k].strip()
               for k in FIELDS - {"consequential"}):
            raise ValueError("Modeling proposal metadata must be nonempty text")
        node = choice["node"]
        if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_']*", node) or node in {"Preamble", "Assumptions", "Axioms"}:
            raise ValueError("A modeling proposal requires an ordinary support-node ID")
        if node in seen or choice["consequential"] is not True:
            raise ValueError("Modeling proposals require unique consequential choices")
        seen.add(node)
        relative_path(choice["evidence_path"])
        if (Path(choice["evidence_path"]).parts[0] not in {"work", "observations", "paper", "upstream"}
                and not choice["evidence_path"].startswith(".trellis/scratch/")):
            raise ValueError("Proposal evidence must belong to candidate source, diagnostics or paper inputs")
    return sorted((dict(c) for c in choices), key=lambda c: c["node"])


def _initial(root: Path) -> list[dict]:
    paper = read_json(root / "config.json").get("paper_input")
    if paper is None:
        return []
    initial = []
    for choice in paper.get("modeling_choices", []):
        if not isinstance(choice, dict) or choice.get("route_decision") is not None:
            raise ValueError("Preparation cannot supply native modeling approvals")
        initial.append({k: v for k, v in choice.items() if k != "route_decision"})
    return initial


def choices(root: Path, state: dict) -> list[dict]:
    record = state.get("paper_modeling_proposals", {})
    if record:
        if not isinstance(record, dict) or not record.get("record") or not record.get("sha256"):
            raise ValueError("Proposed inventory is missing its controller receipt")
        sealed = {k: v for k, v in record.items() if k not in {"record", "sha256"}}
        if (record.get("sha256") != _hash(sealed)
                or read_json(evidence_path(root, record["record"])) != sealed):
            raise ValueError("Recorded modeling proposal differs from its controller receipt")
    proposed = record.get("choices", [])
    return _validate([*_initial(root), *proposed])


def _read_evidence(root: Path, name: str) -> bytes:
    path = evidence_path(root, name)
    if path.stat().st_size > MAX_EVIDENCE_BYTES:
        raise ValueError("Modeling evidence exceeds the per-file size limit")
    with path.open("rb") as stream:
        data = stream.read(MAX_EVIDENCE_BYTES + 1)
    if len(data) > MAX_EVIDENCE_BYTES:
        raise ValueError("Modeling evidence exceeds the per-file size limit")
    return data


def _evidence(root: Path, entries: list[dict]) -> dict[str, str]:
    result, total = {}, 0
    for name in sorted({c["evidence_path"] for c in entries}):
        data = _read_evidence(root, name)
        total += len(data)
        if total > MAX_TOTAL_EVIDENCE_BYTES:
            raise ValueError("Modeling evidence exceeds the aggregate size limit")
        result[name] = hashlib.sha256(data).hexdigest()
    return result


def basis(root: Path, state: dict) -> str:
    entries = choices(root, state)
    return _hash({"choices": entries, "evidence": _evidence(root, entries)})


def propose(root: Path, state: dict, proposals: object) -> None:
    config = read_json(root / "config.json")
    if config.get("paper_input", {}).get("route") != "rust_aeneas":
        raise ValueError("Extraction-model proposals require Rust paper preparation")
    proposed = _validate(proposals)
    old = state.get("paper_modeling_proposals", {})
    choices(root, state)  # Existing controller receipts must remain intact.
    merged = _validate([*_initial(root), *proposed])
    evidence = _evidence(root, merged)
    if old.get("choices", []) == proposed and old.get("evidence_sha256", {}) == evidence:
        return
    record = {"schema": "trellis-paper-modeling-proposals/v1", "purpose": "proposed inventory; no approval",
              "sequence": state["sequence"], "choices": proposed, "evidence_sha256": evidence}
    identity = _hash(record)
    path = STORE / "proposals" / f"{identity}.json"
    if (root / path).exists():
        if read_json(root / path) != record:
            raise ValueError("Recorded modeling proposal changed")
    else:
        write_json(root / path, record)
    state["paper_modeling_proposals"] = {**record, "record": path.as_posix(), "sha256": identity}
    # The existing setup audit covers extraction configuration; only the
    # artifact/implementation audit depends on newly discovered semantics.
    state.get("audits", {}).pop("adaptation", None)
    state.pop("export_check", None)
    state["route_required"] = "Classify and review the proposed extraction-model choices and their source evidence."


def context(root: Path, state: dict) -> dict:
    record = state.get("paper_modeling_proposals", {})
    return {"proposals": record.get("choices", []), "proposal_record": record.get("record")}


def approved_choices(root: Path, state: dict) -> list[dict]:
    entries = choices(root, state)
    if not entries:
        return []
    from trellis.modeling import routes
    current = routes.basis(root, state)
    approved = set()
    for decision in state.get("route_decisions", []):
        review = decision.get("review", {})
        resolution = decision.get("resolution", {})
        if review.get("basis_fingerprint") != current or resolution.get("action") != "approve":
            continue
        attempts = review.get("attempts", [])
        if len(attempts) != 2:
            continue
        for task in resolution.get("tasks", []):
            marker = task.get("id", "")
            if not marker.startswith("paper-model:"):
                continue
            token = re.compile(r"(?<![\w.'-])" + re.escape(marker) + r"(?![\w.'-])")
            if all(token.search(attempt.get("route_tex", "")) for attempt in attempts):
                approved.add(marker.removeprefix("paper-model:"))
    missing = {c["node"] for c in entries} - approved
    if missing:
        raise ValueError("Proposed extraction models require current two-round route approval: " + ", ".join(sorted(missing)))
    return entries


def validate_audit(root: Path, state: dict) -> None:
    current = basis(root, state)
    audit = state.get("audits", {}).get("adaptation", {})
    # Older empty-inventory preparations have no discovery artifact. Preserve
    # their compatibility while rejecting removal of a bound inventory.
    if choices(root, state) or state.get("paper_modeling_proposals") or "paper_choices_basis" in audit:
        if audit.get("paper_choices_basis") != current:
            raise ValueError("Modeling proposal inventory or evidence changed; renew adaptation review")
    approved_choices(root, state)


def export(root: Path, state: dict, destination: Path) -> dict:
    validate_audit(root, state)
    entries = approved_choices(root, state)
    evidence_files = []
    projected = []
    for choice in entries:
        data = _read_evidence(root, choice["evidence_path"])
        name = f"modeling-input/paper-choices/evidence/{hashlib.sha256(data).hexdigest()}.txt"
        target = destination / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
        evidence_files.append(name)
        projected.append({**choice, "evidence_path": name})
    receipt = {"schema": "trellis-paper-modeling-proposals/v1", "basis": basis(root, state),
               "proposals": state.get("paper_modeling_proposals"),
               "initial_choices": read_json(root / "config.json")["paper_input"].get("modeling_choices", []),
               "route_decisions": state.get("route_decisions", []), "choices": projected,
               "approval": "Preparation route only; fresh native route, Corr and preservation/Sound review required."}
    name = "modeling-input/paper-choices/registration.json"
    write_json(destination / name, receipt)
    evidence_files.append(name)
    return {"choices": projected, "evidence_files": sorted(set(evidence_files))}


def check_export(root: Path, state: dict, destination: Path) -> bool:
    """Check retained discovery evidence without changing either tree."""
    validate_audit(root, state)
    path = destination / "modeling-input/paper-choices/registration.json"
    if not path.exists():
        return not choices(root, state) and not state.get("paper_modeling_proposals")
    record = read_json(path)
    if (record.get("basis") != basis(root, state)
            or record.get("proposals") != state.get("paper_modeling_proposals")
            or record.get("route_decisions") != state.get("route_decisions", [])
            or record.get("initial_choices") != read_json(root / "config.json")["paper_input"].get("modeling_choices", [])):
        return False
    from trellis.paper_implementation import MANIFEST
    if (destination / MANIFEST).exists() and read_json(destination / MANIFEST).get("modeling_choices") != record.get("choices"):
        return False
    expected = approved_choices(root, state)
    if len(expected) != len(record.get("choices", [])):
        return False
    for choice, exported in zip(expected, record["choices"]):
        if {k: v for k, v in choice.items() if k != "evidence_path"} != {k: v for k, v in exported.items() if k != "evidence_path"}:
            return False
        if _read_evidence(root, choice["evidence_path"]) != _read_evidence(destination, exported["evidence_path"]):
            return False
    return True
