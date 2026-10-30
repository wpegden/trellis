"""Preparation adapter for the kernel's shared modeling-route review."""
from __future__ import annotations

import hashlib
import json
from pathlib import Path

from trellis.modeling.observations import read_json, write_json
from trellis.runtime.kernel_cli import run_kernel_cli

ROLES = {"planner", "critic", "route_audit"}


def targets(root: Path, state: dict) -> list[str]:
    from trellis.paper_implementation import preparation_targets
    return [g["id"] for g in preparation_targets(read_json(root / "config.json")) if g["id"] not in state.get("declined", {})]


def basis(root: Path, state: dict) -> str:
    from trellis.modeling.driver import source_fingerprint
    from trellis.paper_implementation import preparation_paper_digest
    subject = [source_fingerprint(root), preparation_paper_digest(root), targets(root, state)]
    if "paper_input" in read_json(root / "config.json"):
        from trellis.modeling import paper_choices
        subject.append(paper_choices.basis(root, state))
    return hashlib.sha256(json.dumps(subject, sort_keys=True).encode()).hexdigest()


def start(root: Path, state: dict, reason: str) -> None:
    if not reason.strip():
        raise ValueError("Modeling-route review requires the tradeoff to examine")
    state["route_review"] = {"targets": targets(root, state), "basis_fingerprint": basis(root, state),
                             "attempts": [], "pending_route": None}
    state["route_reason"] = reason
    state["route_required"] = reason
    state["next"] = {"role": "planner"}


def context(root: Path, state: dict, role: str) -> dict:
    from trellis.modeling.inspection import observation_summary
    review = state["route_review"]
    result = {"stage": state["stage"], "targets": review["targets"],
              "observation": observation_summary(state["observation"]),
              "scope": state.get("scope", "The supplied source and GOALs; scope choices require justification.")}
    if role == "critic":
        # Same isolation as native GapResearch: one proposal and the original
        # source/GOAL problem, without the reviewer's preference or past verdicts.
        result["route_tex"] = review["pending_route"]
    else:
        result.update(reported_tradeoff=state["route_reason"], attempts=review["attempts"])
    if state.get("feedback"):
        result["response_feedback"] = state["feedback"]
    return result


def apply(root: Path, state: dict, role: str, raw: dict) -> None:
    paper = "paper_input" in read_json(root / "config.json")
    resolution = raw.get("modeling_resolution")
    if (role == "route_audit" and paper
            and isinstance(resolution, dict) and resolution.get("declined_targets")):
        raise ValueError("Paper preparation retains every selected claim; changing selection requires a fresh explicit intake")
    current_basis = basis(root, state)
    if paper and state["route_review"]["basis_fingerprint"] != current_basis:
        prior = state["route_review"]
        path = root / ".trellis/modeling/routes" / f"stale-{state['sequence']:05d}-{prior['basis_fingerprint']}.json"
        write_json(path, {"review": prior, "response": raw, "reason": "Source or proposed-model evidence changed"})
        start(root, state, state["route_reason"])
        state["feedback"] = "Source or proposed-model evidence changed; repeat route review on the current basis."
        return
    inputs = {
        "review": state["route_review"], "role": "audit" if role == "route_audit" else role,
        "raw_payload": raw, "basis_fingerprint": current_basis, "pursued_targets": targets(root, state),
    }
    if paper:
        from trellis.modeling import paper_choices
        inputs["paper_modeling_choices"] = [c["node"] for c in paper_choices.choices(root, state)]
    output = run_kernel_cli({"action": "modeling_route_step", "input": inputs})["output"]
    state["route_review"] = output["review"]
    if role != "route_audit":
        state["next"] = {"role": "route_audit" if output["next_role"] == "audit" else output["next_role"]}
        return
    data = output["data"]
    resolution = data["modeling_resolution"]
    record = {"sequence": state["sequence"], "stage": state["stage"], "review": state["route_review"],
              "resolution": resolution, "assessment": data["scratchpad_replace"]}
    state.setdefault("route_decisions", []).append(record)
    path = root / ".trellis/modeling/routes" / f"{state['sequence']:05d}.json"
    write_json(path, record)
    state.pop("route_review", None)
    state.pop("route_required", None)
    if resolution["action"] == "fail":
        state.update(status="failed", failure=data["scratchpad_replace"])
        return
    if resolution["action"] == "reopen":
        from trellis.modeling.driver import reopen_setup
        reopen_setup(state)
    else:
        state["scope"] = resolution["scope"]
        state["limitations"] = resolution.get("limitations", "")
        for target in resolution.get("declined_targets", []):
            state.setdefault("declined", {})[target] = data["scratchpad_replace"]
        if resolution.get("reopen_setup"):
            from trellis.modeling.driver import reopen_setup
            reopen_setup(state)
    state["feedback"] = f"Approved modeling route: {path}. Implement its tasks and verify the resulting artifacts under the ordinary stage criteria."
    state["next"] = {"role": "reviewer"}
