"""Exercise complete preparation prompts against artifact and driver contracts."""
import copy
import json
import re
from pathlib import Path
from types import SimpleNamespace

import pytest

from trellis.modeling import driver, routes
from trellis.paper_implementation import LeanPreparationDriver
from trellis.runtime.kernel_cli import run_kernel_cli


def save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value))


@pytest.fixture
def preparation(tmp_path):
    root = tmp_path / "preparation"
    paper = {"route": "lean_native", "paper_path": "paper/main.tex",
             "targets": [{"id": "thm:main", "selected_text": "Correct decoder output.",
                          "excluded_clauses": [{"text": "Linear running time.", "reason": "Deferred."}]}],
             "entry_points": [{"node_id": "Decoder", "declaration": "Decoder", "source_path": "Tablet/Decoder.lean"}],
             "implementation_roots": {"thm:main": ["Decoder"]},
             "modeling_choices": [{"node": "Model_union", "source_locator": "crate/src/lib.rs:11", "consequential": True}]}
    save(root / "config.json", {"paper_input": paper, "tools": {}})
    state = {"stage": "lean_implementation", "status": "active", "sequence": 7,
             "next": {"role": "worker"}, "verification_protocol": "paper-lean-preparation/v1",
             "audits": {}, "history": [], "feedback": "", "declined": {}, "observation": {}}
    save(root / ".trellis/modeling/state.json", state)
    (root / ".trellis/modeling/requests").mkdir()
    return root, state


def prompt(root, state, role, **extra):
    return driver.agent_prompt(root, state, {"role": role, **extra},
                               SimpleNamespace(raw=root / "response.raw.json", done=root / "response.done"))


def response_shape(text):
    return json.loads(re.search(r"^Response: (\{.*\})\.", text, re.MULTILINE)[1])


@pytest.mark.parametrize("role,field,choices", [
    ("worker", None, [None]), ("reviewer", "action", ["work", "audit", "fail"]),
    ("audit", "decision", ["accept", "revise"]),
])
@pytest.mark.parametrize("checked", [False, True])
def test_every_advertised_lean_decision_has_a_validated_driver_transition(preparation, role, field, choices, checked, monkeypatch):
    monkeypatch.setattr("trellis.modeling.performance.current_basis", lambda root: "fixture-candidate")
    root, initial = preparation
    observation = {"passed": checked, "subject_sha256": "subject", "resource_identity": "tools"}
    initial["observation"] = observation
    text = prompt(root, initial, role)
    assert text.count("Supporting files are preserved under `paper/`") == 1
    assert "paper/TRELLIS_REFERENCES.json" in text
    assert text.count("Computational proof evidence requires Lean proofs") == 1
    assert "Supplied and regenerated data have the same obligations" in text
    assert "paper_implementation.py check" not in text
    assert "supervisor refreshes ordinary checking" in text
    shape = response_shape(text)
    assert set(shape) == ({"summary"} if field is None else {field, "instruction" if role == "reviewer" else "reason"})
    if field:
        assert shape[field].split("|") == choices
    for choice in choices:
        raw = dict(shape)
        if field:
            raw[field] = choice
        validator = {"worker": "worker", "reviewer": "review", "audit": "audit"}[role]
        checked_payload = run_kernel_cli({"action": f"validate_preparation_{validator}_result", "raw_payload": raw})["output"]
        assert checked_payload["ok"], checked_payload
        state = copy.deepcopy(initial)
        state["next"] = {"role": role}
        save(root / ".trellis/modeling/state.json", state)
        instance = LeanPreparationDriver(root, agents=lambda *args: checked_payload["data"], checker=lambda *args: observation)
        if not checked and choice in {"audit", "accept"}:
            assert "pass" in text
            with pytest.raises(ValueError, match="pass|failed"):
                instance.step()
            assert instance.state["status"] == "active"
        else:
            instance.step()
            assert instance.state["history"][-1]["response"] == checked_payload["data"]


def test_lean_audit_revision_reason_and_reviewer_proposal_reach_next_prompt(preparation):
    root, state = preparation
    state["next"] = {"role": "audit"}
    save(root / ".trellis/modeling/state.json", state)
    reason = "The implementation omits the boundary correction required by the paper."
    observation = {"passed": True, "subject_sha256": "subject", "resource_identity": "tools"}
    instance = LeanPreparationDriver(root, agents=lambda *args: {"decision": "revise", "reason": reason}, checker=lambda *args: observation)
    revised = instance.step()
    assert revised["feedback"] == reason
    text = prompt(root, revised, "reviewer")
    assert reason in text and str(root / ".trellis/modeling/state.json") in text
    instance.agents = lambda *args: {"action": "audit", "instruction": "Repaired boundary correction; inspect its source correspondence."}
    ready = instance.step()
    assert ready["next"]["instruction"].startswith("Repaired boundary")
    audit = driver.agent_prompt(root, ready, ready["next"], SimpleNamespace(raw=root / "raw", done=root / "done"))
    assert '"reviewer_proposal": "Repaired boundary' in audit


@pytest.mark.parametrize("stage", ["setup", "adaptation"])
@pytest.mark.parametrize("role", ["worker", "reviewer", "audit", "planner", "critic", "route_audit"])
def test_complete_rust_preparation_prompts_keep_selection_and_actual_role_contract(preparation, stage, role):
    root, state = preparation
    state.update(stage=stage, verification_protocol="native-v1", route_reason="Review union modeling.",
                 route_review={"targets": ["thm:main"], "attempts": [], "pending_route": "paper-model:Model_union"})
    text = prompt(root, state, role)
    assert text.count("Supporting files are preserved under `paper/`") == 1
    assert "paper/TRELLIS_REFERENCES.json" in text
    assert text.count("Computational proof evidence requires Lean proofs") == 1
    assert "Supplied and regenerated data have the same obligations" in text
    shape = response_shape(text)
    assert "Correct decoder output." in text and "Linear running time." in text
    assert '"source_locator": "crate/src/lib.rs:11"' in text
    assert "GOAL.md" not in text and "managed native launcher" not in text
    if role != "worker":
        assert "requires empty `declined_targets`" in text
        assert "smallest reduction" not in text
    if role == "reviewer":
        assert ("Use reopen" in text) == (stage == "adaptation")
        assert ("reopen" in shape["action"].split("|")) == (stage == "adaptation")
    if role == "route_audit":
        assert shape["modeling_resolution"]["declined_targets"] == []
        assert shape["modeling_resolution"]["tasks"][0]["id"] == "task"


def test_paper_route_decline_rejected_before_kernel_or_state_mutation(preparation, monkeypatch):
    root, state = preparation
    state["route_review"] = {"targets": ["thm:main", "thm:other"], "attempts": []}
    original = copy.deepcopy(state)
    monkeypatch.setattr(routes, "run_kernel_cli", lambda *args: pytest.fail("Rejected selection must not enter route transition"))
    with pytest.raises(ValueError, match="retains every selected claim"):
        routes.apply(root, state, "route_audit", {"modeling_resolution": {"action": "approve", "declined_targets": ["thm:other"]}})
    assert state == original


@pytest.mark.parametrize("critic_decision", ["accept", "reject"])
@pytest.mark.parametrize("resolution_action", ["approve", "reopen", "fail"])
def test_paper_route_prompt_shapes_follow_real_two_round_protocol(preparation, critic_decision, resolution_action):
    root, state = preparation
    state.update(stage="adaptation", route_reason="Review union modeling.")
    review = {"targets": ["thm:main"], "basis_fingerprint": "basis", "attempts": [], "pending_route": None}
    roles = ["planner", "critic", "planner", "critic", "route_audit"]
    for role in roles:
        state["route_review"] = review
        raw = response_shape(prompt(root, state, role))
        if role in {"planner", "critic"}:
            raw["report"] = "The source relation and the alternative supported-source implementation must cover every selected paper hypothesis and conclusion. " * 3
        if role == "planner":
            raw["route_tex"] = "paper-model:Model_union: retain the paper algorithm; justify the concrete mutable-state relation."
        elif role == "critic":
            raw.update(gap_decision=critic_decision, gap_feedback="Check the mutable-state relation for every branch.")
        else:
            raw["modeling_resolution"]["action"] = resolution_action
            if resolution_action != "approve":
                raw["modeling_resolution"] = {"action": resolution_action}
        output = run_kernel_cli({"action": "modeling_route_step", "input": {
            "review": review, "role": "audit" if role == "route_audit" else role, "raw_payload": raw,
            "basis_fingerprint": "basis", "pursued_targets": ["thm:main"]}})["output"]
        review = output["review"]
    assert len(review["attempts"]) == 2
    assert output["data"]["modeling_resolution"]["action"] == resolution_action


def test_ordinary_pv_route_decline_still_uses_existing_transition(preparation, monkeypatch):
    root, state = preparation
    save(root / "config.json", {"goals": [{"id": "main"}, {"id": "other"}]})
    state.update(stage="adaptation", route_review={"targets": ["main", "other"], "attempts": []})
    resolution = {"action": "approve", "scope": "main", "limitations": "other deliberately declined", "declined_targets": ["other"]}
    monkeypatch.setattr(routes, "basis", lambda *args: "basis")
    monkeypatch.setattr(routes, "run_kernel_cli", lambda payload: {"output": {"review": {}, "data": {"modeling_resolution": resolution, "scratchpad_replace": "Existing PV route review."}}})
    routes.apply(root, state, "route_audit", {"modeling_resolution": resolution})
    assert state["declined"] == {"other": "Existing PV route review."}
    assert "review_route" in response_shape("\n" + driver.response_contract("reviewer", "setup"))["action"]
