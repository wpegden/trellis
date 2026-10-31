import json
from pathlib import Path

import pytest

from trellis.pv_revision.driver import Driver, initialize


def plan(remedy="repair"):
    disposition = {"goal_id": "g", "remedy": remedy, "reason": "formal counterexample",
                   "replacement_goals": [], "surviving_obligations": [], "task_ids": []}
    tasks = []
    if remedy == "repair":
        disposition["task_ids"] = ["fix"]
        tasks = [{"id": "fix", "title": "fix", "authorization": "change behavior for bounded inputs",
                  "paths": ["lib.rs"], "goal_ids": ["g"], "checks": [{"id": "regression", "argv": ["regression", "{source}"], "expected_exit": 0}]}]
    elif remedy == "replace":
        disposition["replacement_goals"] = [{"id": "g2", "text": "bounded obligation"}]
    return {"summary": "compare remedies", "alternatives": [], "dispositions": [disposition], "tasks": tasks}


def make_driver(tmp_path, responses, *, checker=lambda *_: {"ok": True}, goals=None):
    source = tmp_path / "source"
    source.mkdir()
    (source / "lib.rs").write_text("old\n")
    (source / "other.rs").write_text("unrelated\n")
    root = tmp_path / "revision"
    initialize(root, source, {"context": "shared meaning", "goals": goals or [{"id": "g", "text": "goal"}]}, ["g"])
    def agents(root, state, task):
        response = responses.pop(0)
        return response(root, state, task) if callable(response) else response
    return Driver(root, agents=agents, checker=checker)


def worker(content="fixed\n", outcome="complete", path="lib.rs"):
    def run(root, state, task):
        (Path(task["writable_source"]) / path).write_text(content)
        return {"outcome": outcome, "summary": "changed", "alternatives": [
            {"approach": "bounded change", "status": "tested", "rationale": "narrow fix", "evidence": ["regression evidence"]}]}
    return run


def verdict(decision="pass", binding=None):
    return lambda root, state, task: {"decision": decision, "binding": binding or state["binding"],
                                    "reason": "reviewed exact patch", "alternatives_assessment": "matches patch"}


def review(action, task_id="fix"):
    return {"action": action, "task_id": task_id, "reason": "reviewed"}


def test_multistep_scope_retry_and_human_gate(tmp_path):
    responses = [plan(), review("work"), worker("bad\n"), verdict("fail"), review("work"), worker(), verdict(),
                 review("complete"), verdict()]
    driver = make_driver(tmp_path, responses)
    for _ in range(4):
        driver.step()
    assert (driver.root / driver.state["source"] / "lib.rs").read_text() == "old\n"
    assert len(driver.state["attempts"]) == 1
    # Resume durably between attempts.
    driver = Driver(driver.root, agents=driver.agents, checker=driver.checker)
    while responses:
        driver.step()
    assert driver.state["status"] == "awaiting_human"
    assert len(driver.state["attempts"]) == 2
    assert (driver.root / "sources/original/lib.rs").read_text() == "old\n"
    package = driver.state["human_package"]
    assert package["proposed_collection"]["context"] == "shared meaning"
    driver.decide("accept", digest=package["digest"], reason="approved")
    assert driver.state["status"] == "accepted_for_modeling"


def test_goal_only_human_amend_and_deny(tmp_path):
    responses = [plan("replace"), review("complete"), verdict(), plan("retire"), review("complete"), verdict()]
    driver = make_driver(tmp_path, responses)
    for _ in range(3):
        driver.step()
    old_digest = driver.state["human_package"]["digest"]
    driver.decide("amend", digest=old_digest, reason="change requirements", amendment="Retire instead")
    assert driver.state["next"]["role"] == "audit"
    assert "human_package" not in driver.state
    for _ in range(3):
        driver.step()
    with pytest.raises(ValueError, match="stale"):
        driver.decide("accept", digest=old_digest, reason="old approval")
    driver.decide("deny", digest=driver.state["human_package"]["digest"], reason="declined")
    assert driver.state["status"] == "denied"


def test_incomplete_audit_and_malformed_output_are_transactional(tmp_path):
    bad = plan()
    bad["dispositions"] = []
    driver = make_driver(tmp_path, [bad, {"summary": "broken", "dispositions": [None]}])
    for _ in range(2):
        driver.step()
        assert driver.state["plan"] is None
        assert driver.state["next"] == {"role": "audit"}
        assert driver.state["feedback"]
    assert len(driver.state["history"]) == 2
    assert (driver.root / "requests/00002-audit/response.json").exists()


def affected_goal_plan():
    audit = plan()
    audit["dispositions"].append({
        "goal_id": "diagnostic", "remedy": "replace",
        "reason": "The repaired public behavior changes the legacy diagnostic's domain",
        "replacement_goals": [{"id": "diagnostic", "text": "Diagnostic on the revised domain"}],
    })
    return audit


def test_affected_proved_goal_revision_keeps_original_evidence_and_human_gate(tmp_path):
    goals = [{"id": "g", "text": "Public requirement"},
             {"id": "diagnostic", "text": "Old implementation diagnostic"},
             {"id": "unaffected", "text": "Independent requirement"}]
    responses = [affected_goal_plan(), review("work"), worker(), verdict(), review("complete"), verdict()]
    driver = make_driver(tmp_path, responses, goals=goals)
    while responses:
        driver.step()
    assert driver.state["status"] == "awaiting_human", driver.state.get("feedback")
    assert driver.state["negated_goals"] == ["g"]
    assert driver.state["original_goals"] == goals
    assert {goal["id"]: goal["text"] for goal in driver.state["proposed_goals"]} == {
        "g": "Public requirement", "diagnostic": "Diagnostic on the revised domain",
        "unaffected": "Independent requirement"}
    assert driver.state["human_decisions"] == []
    package = driver.state["human_package"]
    assert package["original_collection"]["goals"] == goals
    assert package["proposed_collection"]["goals"] == driver.state["proposed_goals"]
    assert (driver.root / "sources/original/lib.rs").read_text() == "old\n"
    driver.step()
    assert driver.state["status"] == "awaiting_human"


@pytest.mark.parametrize("defect,diagnostic", [
    ("missing_refuted", "every refuted goal"),
    ("duplicate", "at most once"),
    ("unknown", "existing goals"),
])
def test_additional_dispositions_do_not_bypass_inventory_validation(tmp_path, defect, diagnostic):
    audit = affected_goal_plan()
    if defect == "missing_refuted":
        audit["dispositions"].pop(0)
    elif defect == "duplicate":
        audit["dispositions"].append(audit["dispositions"][-1])
    else:
        audit["dispositions"][-1]["goal_id"] = "unknown"
    goals = [{"id": "g", "text": "Public requirement"}, {"id": "diagnostic", "text": "Diagnostic"}]
    driver = make_driver(tmp_path, [audit], goals=goals)
    driver.step()
    assert diagnostic in driver.state["feedback"]
    assert driver.state["plan"] is None
    assert driver.state["proposed_goals"] == goals
    assert driver.state["tasks"] == {}


def test_stale_scope_and_enforced_paths_do_not_promote(tmp_path):
    responses = [plan(), review("work"), worker(path="other.rs"), verdict(binding="stale"), verdict()]
    driver = make_driver(tmp_path, responses)
    for _ in range(4):
        driver.step()
    assert "Stale" in driver.state["feedback"]
    driver.step()
    assert "enforceable" in driver.state["feedback"]
    assert driver.state["tasks"]["fix"]["status"] == "pending"
    assert driver.state["source"] == "sources/original"


def test_failed_worker_cannot_count_as_success(tmp_path):
    responses = [plan(), review("work"), worker(outcome="exhausted"), verdict(), review("complete")]
    driver = make_driver(tmp_path, responses)
    for _ in range(5):
        driver.step()
    assert "cannot count as success" in driver.state["feedback"]
    assert driver.state["status"] == "active"
    assert not driver.state["attempts"][0]["accepted"]


def test_checks_and_tampering_block_acceptance(tmp_path):
    driver = make_driver(tmp_path, [plan(), review("work"), worker(), verdict()], checker=lambda *_: {"ok": False})
    for _ in range(4):
        driver.step()
    assert "independent regression" in driver.state["feedback"]
    assert driver.state["source"] == "sources/original"


def test_human_gate_rechecks_source_bytes(tmp_path):
    driver = make_driver(tmp_path, [plan("replace"), review("complete"), verdict()])
    for _ in range(3):
        driver.step()
    package = driver.state["human_package"]
    (driver.root / driver.state["source"] / "lib.rs").write_text("tampered")
    with pytest.raises(ValueError, match="source changed"):
        driver.decide("accept", digest=package["digest"], reason="approve")
    assert driver.state["status"] == "awaiting_human"


def test_model_mismatch_routes_to_modeling_review(tmp_path):
    driver = make_driver(tmp_path, [plan("model_mismatch")])
    driver.step()
    assert driver.state["status"] == "requires_modeling_review"
    assert driver.state["proposed_goals"] == driver.state["original_goals"]


def test_scope_change_reaudits_from_original_and_preserves_attempt(tmp_path):
    responses = [plan(), review("work"), worker(), verdict(), review("reaudit"), plan("replace")]
    driver = make_driver(tmp_path, responses)
    for _ in range(4):
        driver.step()
    accepted_source = driver.state["source"]
    assert accepted_source != "sources/original"
    driver.step()
    assert driver.state["source"] == "sources/original"
    assert driver.state["tasks"] == {}
    assert len(driver.state["attempts"]) == 1
    assert (driver.root / accepted_source / "lib.rs").read_text() == "fixed\n"
    driver.step()
    assert driver.state["tasks"] == {}
    assert driver.state["proposed_goals"] == [{"id": "g2", "text": "bounded obligation"}]


def test_artifact_contract_rejects_bad_payload(tmp_path):
    from trellis.checking import validate_json_artifact
    path = tmp_path / "artifact.json"
    path.write_text(json.dumps({"summary": "missing outcome and alternatives"}))
    result = validate_json_artifact("pv-revision-worker-result", path)
    assert result["ok"] is False
    path.write_text(json.dumps(plan("replace")))
    assert validate_json_artifact("pv-revision-audit-result", path)["ok"] is True


def test_two_tasks_chain_exact_base_and_candidate(tmp_path):
    audit = plan()
    audit["tasks"].append({"id": "second", "title": "second repair", "authorization": "repair companion behavior",
                           "paths": ["other.rs"], "goal_ids": ["g"], "checks": [{"id": "regression", "argv": ["regression", "{source}"], "expected_exit": 0}]})
    audit["dispositions"][0]["task_ids"].append("second")
    responses = [audit, review("work"), worker(), verdict(), review("work", "second"),
                 worker("companion fixed\n", path="other.rs"), verdict(), review("complete"), verdict()]
    driver = make_driver(tmp_path, responses)
    while responses:
        driver.step()
    assert driver.state["status"] == "awaiting_human"
    first, second = driver.state["attempts"]
    assert second["base"] == first["manifest"]
    assert second["base_source"] == str(driver.root / first["source"])
    assert first["binding"] != second["binding"]
    package = driver.state["human_package"]
    assert len(package["task_diffs"]) == 2
    assert {row["path"] for row in package["aggregate_diff"]["changes"]} == {"lib.rs", "other.rs"}


@pytest.mark.parametrize("feedback", ["", "Audit tool needs repair"])
def test_production_agents_construct_confined_request_without_paid_execution(tmp_path, monkeypatch, feedback):
    import os
    from trellis.pv_revision.agents import Agents
    from trellis.agent_wrapper.protocol import SingleAgentResponse
    from trellis.runtime import bridge
    from trellis.sandbox import _repo_writable_paths
    from trellis.modeling.environment import source_burst_environment

    source = tmp_path / "source"
    source.mkdir()
    (source / "lib.rs").write_text("old")
    reference = tmp_path / "reference"
    reference.mkdir()
    root = tmp_path / "revision"
    template = Path(__file__).resolve().parents[1] / "examples/trellis.native.pv.config.json"
    # This boundary test exercises real initialization/materialization and
    # request construction without selecting the developer's local kernel
    # binary. Binary/source identity is verified by deployment tests separately.
    monkeypatch.setattr("trellis.runtime_snapshot._active_kernel_binary_path", lambda: None)
    initialize(root, source, [{"id": "g", "text": "goal"}], ["g"], config_template=template,
               tools={"env": {"CARGO_NET_OFFLINE": "true"}}, handoff={"reference_paths": [str(reference)]})
    assert (root / ".git").is_dir()
    assert (root / ".trellis/scripts/check.py").is_file()
    state = json.loads((root / "state.json").read_text())
    candidate = root / "sources/isolated"
    from trellis.pv_revision.store import copy_source_tree
    copy_source_tree(root / "sources/original", candidate)
    state["sequence"] = 1
    directory = root / "requests/00001-worker"
    directory.mkdir(parents=True)
    (directory / "context.json").write_text("{}")
    monkeypatch.setattr(bridge, "seed_burst_home", lambda *_args, **_kwargs: tmp_path / "burst-home")
    observed = {}
    payload = {"outcome": "complete", "summary": "done", "alternatives": [], "system_feedback": feedback}
    def execute(request):
        observed["request"] = request
        observed["readonly"] = os.environ.get("TRELLIS_SANDBOX_EXTRA_RO_PATHS")
        if feedback:
            runtime = root.with_name(root.name + "-runtime")
            runtime.mkdir(exist_ok=True)
            (runtime / "system_feedback_halt.json").write_text(json.dumps({"system_feedback": feedback}))
        return SingleAgentResponse(request_id=request.request_id, cycle=request.cycle, kind=request.kind,
                                   burst_role=request.burst_role, ok=True,
                                   payload=payload)
    monkeypatch.setattr("trellis.pv_revision.agents.execute_agent_request", execute)
    old_readonly = os.environ.get("TRELLIS_SANDBOX_EXTRA_RO_PATHS")
    assert Agents()(root, state, {"role": "worker", "task_id": "fix", "writable_source": str(candidate)}) == payload
    request = observed["request"]
    assert request.work_dir == root
    assert request.fresh is True
    from trellis.agent_wrapper.executor import _base_instructions_key_for_request
    assert _base_instructions_key_for_request(request) == "worker"
    assert request.burst_role == "reviewer"
    assert request.artifact.kind == "pv-revision-worker-result"
    assert request.sandbox.enabled is True
    writable = _repo_writable_paths(root, role=request.burst_role, extra_writable_paths=request.sandbox.extra_writable_paths)
    assert candidate in writable
    assert root / "sources/original" not in writable
    assert root / "state.json" not in writable
    assert root / "Tablet" not in writable
    assert str(reference) in observed["readonly"].split(os.pathsep)
    assert os.environ.get("TRELLIS_SANDBOX_EXTRA_RO_PATHS") == old_readonly
    scratch = root / ".trellis/scratch/source-tools"
    environment = source_burst_environment(root, scratch)
    assert environment["CARGO_NET_OFFLINE"] == "true"
    assert Path(environment["BASH_ENV"]).is_file()


def test_repeated_invalid_responses_halt_without_acceptance(tmp_path):
    driver = make_driver(tmp_path, [{}, {}, {}])
    for _ in range(3):
        driver.step()
    assert driver.state["status"] == "halted"
    assert driver.state["plan"] is None
    assert len(driver.state["history"]) == 3
    assert all((driver.root / item["evidence"] / "error.json").is_file() for item in driver.state["history"])


def test_system_feedback_preserves_response_and_halts_on_first_burst(tmp_path):
    audit = {**plan(), "system_feedback": "Controller restriction needs repair"}
    driver = make_driver(tmp_path, [audit])
    driver.step()
    assert driver.state["status"] == "halted"
    assert driver.state["feedback"] == audit["system_feedback"]
    assert driver.state["plan"] is None
    assert driver.state["history"][0]["response"] == audit
    driver.step()
    assert driver.state["sequence"] == 1


@pytest.mark.parametrize("marker", ["system_feedback_halt.json", "checker_disagreement_halt.json"])
def test_runtime_halt_does_not_consume_three_retry_bursts(tmp_path, marker):
    def halted(root, state, task):
        runtime = root.with_name(root.name + "-runtime")
        runtime.mkdir()
        (runtime / marker).write_text('{"error":"host failure"}')
        raise RuntimeError("Revision agent runtime halted; inspect logs before recovery")
    driver = make_driver(tmp_path, [halted])
    driver.step()
    assert driver.state["status"] == "halted"
    assert "inspect logs" in driver.state["feedback"]
    driver.step()
    assert driver.state["sequence"] == 1
    assert driver.state["plan"] is None


def test_retry_keeps_exact_completed_task_and_failed_burst_evidence(tmp_path):
    responses = [plan(), review("work"), worker(), verdict(), {}, {}, {}, review("complete"), verdict()]
    driver = make_driver(tmp_path, responses)
    for _ in range(7):
        driver.step()
    assert driver.state["status"] == "halted"
    accepted_source = driver.state["source"]
    attempt = driver.state["attempts"][0]
    history = list(driver.state["history"])
    driver.recover(reason="Corrected the reviewer response contract")
    assert driver.state["status"] == "active"
    assert driver.state["next"] == {"role": "reviewer"}
    assert driver.state["source"] == accepted_source
    assert driver.state["tasks"]["fix"]["status"] == "complete"
    assert driver.state["attempts"] == [attempt]
    assert driver.state["history"] == history
    assert driver.state["consecutive_response_errors"] == 0
    assert driver.state["operator_actions"][0]["previous"]["status"] == "halted"
    driver = Driver(driver.root, agents=driver.agents, checker=driver.checker)
    while responses:
        driver.step()
    assert driver.state["status"] == "awaiting_human"


def test_reaudit_after_denial_clears_approvals_and_retains_rejected_package(tmp_path):
    driver = make_driver(tmp_path, [plan(), review("work"), worker(), verdict(), review("complete"), verdict()])
    for _ in range(6):
        driver.step()
    package = driver.state["human_package"]
    source = driver.state["source"]
    driver.decide("deny", digest=package["digest"], reason="The repair changes the wrong contract")
    before = driver.path.read_bytes()
    with pytest.raises(ValueError, match="different recovery"):
        driver.recover(reason="retry denial")
    assert driver.path.read_bytes() == before
    driver.recover(reason="Reconsider the requirement and repair scope", reaudit=True)
    assert driver.state["status"] == "active" and driver.state["next"] == {"role": "audit"}
    assert driver.state["source"] == "sources/original"
    assert driver.state["tasks"] == {} and driver.state["plan"] is None
    assert "human_package" not in driver.state and "aggregate_scope" not in driver.state
    assert len(driver.state["attempts"]) == 1 and len(driver.state["human_decisions"]) == 1
    assert (driver.root / source / "lib.rs").read_text() == "fixed\n"
    assert (driver.root / "reviews" / package["digest"] / "review.json").exists()


def test_cli_retry_reaudit_and_runtime_halt_guards(tmp_path, capsys):
    from scripts.pv_revision import main
    runtime = tmp_path / "runtime"
    runtime.mkdir()
    (runtime / "pv_revision_ready.json").write_text(json.dumps({"handoff": {"schema_version": 1}}))
    source = tmp_path / "source"
    source.mkdir()
    (source / "lib.rs").write_text("old\n")
    root = runtime / "pv-revision"
    initialize(root, source, [{"id": "g", "text": "goal"}], ["g"])
    driver = Driver(root, agents=lambda *_: {})
    for _ in range(3):
        driver.step()
    old = driver.path.read_bytes()
    marker = runtime / "system_feedback_halt.json"
    marker.write_text('{}')
    assert main(["retry", str(runtime), "--reason", "Investigated"]) == 1
    assert "preserved runtime halt" in capsys.readouterr().out
    assert driver.path.read_bytes() == old
    marker.unlink()
    assert main(["retry", str(runtime), "--reason", "Repaired tool delivery"]) == 0
    assert json.loads(capsys.readouterr().out)["state"] == "active"
    driver = Driver(root, agents=lambda *_: plan("model_mismatch"))
    driver.step()
    assert driver.state["status"] == "requires_modeling_review"
    assert main(["retry", str(runtime), "--reason", "Try again"]) == 1
    assert "different recovery" in capsys.readouterr().out
    assert main(["reaudit", str(runtime), "--reason", "Modeling assumptions clarified"]) == 0
    assert json.loads(capsys.readouterr().out)["state"] == "active"
    recovered = Driver(root).state
    assert recovered["phase"] == "audit" and recovered["plan"] is None
    assert len(recovered["history"]) == 4
    assert [item["action"] for item in recovered["operator_actions"]] == ["retry", "reaudit"]


def test_final_checks_reopen_task_regressed_by_later_repair(tmp_path):
    audit = plan()
    audit["tasks"].append({"id": "second", "title": "Update companion behavior",
                           "authorization": "Update the companion interface in other.rs",
                           "paths": ["other.rs"], "goal_ids": ["g"],
                           "checks": [{"id": "companion", "argv": ["regression", "{source}"], "expected_exit": 0}]})
    audit["dispositions"][0]["task_ids"].append("second")
    calls = []
    def check(source, attempt):
        ident = attempt["authorization"]["id"]
        implementation = (source / "lib.rs").read_text()
        companion = (source / "other.rs").read_text()
        # Task 1's initial repair is correct for the original companion but
        # needs an interface adjustment after independently valid task 2.
        ok = (implementation == "fixed-v2\n" or
              (implementation == "fixed-v1\n" and companion == "unrelated\n")) if ident == "fix" else companion == "new companion\n"
        receipt = {"ok": ok, "task_id": ident, "implementation": implementation, "companion": companion}
        calls.append({"aggregate": attempt["id"].startswith("aggregate-"), **receipt})
        return receipt
    responses = [audit, review("work"), worker("fixed-v1\n"), verdict(), review("work", "second"),
                 worker("new companion\n", path="other.rs"), verdict(), review("complete"),
                 review("work"), worker("fixed-v2\n"), verdict(), review("complete"), verdict()]
    driver = make_driver(tmp_path, responses, checker=check)
    for _ in range(7):
        driver.step()
    assert all(task["status"] == "complete" for task in driver.state["tasks"].values())
    assert all(attempt["accepted"] and attempt["checks"]["ok"] for attempt in driver.state["attempts"])
    driver.step()
    assert driver.state["status"] == "active"
    assert driver.state["next"] == {"role": "reviewer"}
    assert driver.state["tasks"]["fix"]["status"] == "pending"
    assert driver.state["tasks"]["second"]["status"] == "complete"
    assert driver.state["aggregate_checks"]["fix"]["ok"] is False
    assert driver.state["aggregate_checks"]["second"]["ok"] is True
    assert "Final candidate regression" in driver.state["feedback"]
    assert "human_package" not in driver.state
    assert not (driver.root / "human-package.json").exists()
    # A restart retains the failed final observations and the reopened task.
    driver = Driver(driver.root, agents=driver.agents, checker=check)
    assert driver.state["aggregate_checks"]["fix"]["ok"] is False
    while len(responses) > 1:
        driver.step()
    assert driver.state["next"] == {"role": "aggregate_scope"}
    assert all(receipt["ok"] for receipt in driver.state["aggregate_checks"].values())
    assert "human_package" not in driver.state
    assert [(call["task_id"], call["ok"]) for call in calls if call["aggregate"]] == [
        ("fix", False), ("second", True), ("fix", True), ("second", True)]
    driver.step()
    assert driver.state["status"] == "awaiting_human"
    assert len(driver.state["attempts"]) == 3
    assert driver.state["human_package"]["aggregate_checks"]["fix"]["implementation"] == "fixed-v2\n"
