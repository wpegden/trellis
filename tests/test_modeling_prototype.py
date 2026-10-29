import json
from pathlib import Path
import subprocess

import pytest

from trellis.modeling.driver import Driver, agent_prompt, initialize, setup_fingerprint, source_fingerprint
from trellis.modeling.observations import fingerprint, passed, write_json
from trellis.sandbox import declared_repo_writable_paths
from trellis.config import SandboxConfig


@pytest.fixture(autouse=True)
def local_feedback_log(tmp_path, monkeypatch):
    monkeypatch.setattr("trellis.agent_wrapper.executor.project_feedback_log_path",
                        lambda state: tmp_path / "feedback.jsonl")


@pytest.fixture
def run(tmp_path):
    source = tmp_path / "source"
    source.mkdir()
    (source / "lib.rs").write_text("pub fn f(x: u8) -> u8 { x }\n")
    (source / "Cargo.toml").write_text('[package]\nname="test"\nversion="0.1.0"\n')
    goals = tmp_path / "goals.json"
    write_json(goals, {"goals": [{"id": "g", "text": "f is strictly increasing"}]})
    root = tmp_path / "run"
    initialize(root, source, goals, {}, model="gpt-5.6-terra", effort="high")
    return root


def checked(root, output):
    output = output or root / "observations/fixture"
    output.mkdir(parents=True, exist_ok=True)
    raw = output / "M.lean"
    raw.write_text("import Aeneas\nnamespace demo\n/-- [demo::f] -/\ndef f := 1\nend demo\n")
    observation = {"work_fingerprint": fingerprint(root / "work"), "steps": [
        {"name": s, "exit_code": 0, **({"module": "M"} if s == "lean" else {})}
        for s in ("cargo", "charon", "aeneas", "lean")],
        "placeholders": 0, "error": "", "raw_models": {"M": str(raw)}}
    bind_axiom_fixture(observation)
    return observation


def bind_axiom_fixture(observation):
    """Mock successful tool evidence for scheduler tests; real Lean has its own tests."""
    import hashlib
    from trellis.modeling.axioms import SCHEMA, probe_identity
    raw = Path(observation["raw_models"]["M"])
    report = raw.with_name("axioms.json")
    write_json(report, {"schema": SCHEMA, "module": "M", "complete": True,
                        "declarations": [], "error_lines": []})
    sha = hashlib.sha256(raw.read_bytes()).hexdigest()
    observation.update(preparation_checks=probe_identity(), environment="fixture", modules={"M": str(raw)},
                       axiom_dependencies={"schema": SCHEMA, "probe_sha256": probe_identity(),
                         "environment": "fixture", "raw_sha256": {"M": sha}, "modules": {"M": {
                           "source_sha256": sha, "report": str(report),
                           "report_sha256": hashlib.sha256(report.read_bytes()).hexdigest()}}})


def test_export_defect_halts_before_audit_and_keeps_evidence(run, monkeypatch):
    from trellis.modeling.driver import check_halt
    monkeypatch.setenv("TRELLIS_SYSTEM_FEEDBACK_HALT", "1")
    driver = Driver(run, checker=checked)
    driver.refresh()
    driver.state.update(stage="adaptation", next={"role": "audit", "kind": "advance", "stage": "adaptation"})
    raw = Path(driver.state["observation"]["raw_models"]["M"])
    raw.write_text(raw.read_text().replace("def f := 1", "unrecognized_command f := 1"))
    driver.agents = lambda *args: pytest.fail("An exporter failure must not launch the audit")
    with pytest.raises(RuntimeError, match="export_validation"):
        driver.step()
    state = json.loads(driver.path.read_text())
    assert state["status"] == "halted" and "adaptation" not in state["audits"]
    evidence = json.loads(Path(state["host_failure"]["evidence"]).read_text())
    assert not evidence["passed"] and "unrecognized generated declaration" in evidence["error"]
    with pytest.raises(RuntimeError, match="halted"):
        check_halt(run)


@pytest.mark.parametrize("support_handoff", [False, True])
def test_cyclic_export_returns_to_adaptation_and_can_be_repaired(run, support_handoff):
    from trellis.modeling.native import NativeExportCandidateError

    driver = Driver(run, checker=checked)
    driver.refresh()
    task = {"role": "audit", "kind": "advance", "stage": "adaptation"}
    driver.state.update(stage="adaptation", next=task)
    raw = Path(driver.state["observation"]["raw_models"]["M"])
    # A method projects through its later implementation record, which in turn
    # contains that method. This is not a missing foundational definition.
    cyclic = ("import Aeneas\nnamespace demo\n"
              "/-- [demo::Trait] -/\nstructure Trait where\n  member : Nat\n"
              "/-- [demo::method] -/\ndef method : Nat := inst.member\n"
              "/-- [demo::inst] -/\n@[reducible]\nimpl_def inst : Trait := { member := method }\n"
              "end demo\n")
    raw.write_text(cyclic)
    result = {"decision": "accept", "route_compliance": "not_needed", "reason": "reviewed"}
    if support_handoff:
        driver.state["observation"]["steps"][-1]["exit_code"] = 1
        result["support_obligations"] = ["Provide missing model definitions"]
    # Neither an accepting audit nor a support-handoff label can waive this.
    with pytest.raises(NativeExportCandidateError, match="cyclic generated imports"):
        driver.apply(task, result)
    driver.state["next"] = task
    driver.agents = lambda *args: pytest.fail("Cyclic packaging must not reach the audit")
    driver.step()
    saved = json.loads(driver.path.read_text())
    assert saved["status"] == "active" and saved["stage"] == "adaptation"
    assert saved["next"] == {"role": "reviewer"}
    assert "adaptation" not in saved["audits"] and "host_failure" not in saved
    assert "Revise the adaptation" in saved["feedback"]
    evidence = json.loads(Path(saved["export_check"]["report"]).read_text())
    assert not evidence["passed"] and evidence["kind"] == "candidate"
    assert evidence["cycle"][0] == evidence["cycle"][-1]
    assert {"Tablet.inst", "Tablet.method"} <= set(evidence["cycle"])
    assert saved["history"][-1]["response"]["export_validation_error"] == saved["feedback"]
    # Revision preserves normal independent stage approval and can progress.
    raw.write_text(cyclic.replace("def method : Nat := inst.member", "def method : Nat := 1"))
    bind_axiom_fixture(driver.state["observation"])
    driver.state["next"] = task
    driver.agents = lambda *args: result
    driver.step()
    assert driver.state["status"] == "prepared" and driver.state["stage"] == "modeling"
    assert driver.state["export_check"]["passed"]


def test_axiom_probe_timeout_returns_to_review_and_can_retry(run):
    driver = Driver(run, checker=checked)
    driver.refresh()
    task = {"role": "audit", "kind": "advance", "stage": "adaptation"}
    driver.state.update(stage="adaptation", next=task)
    observation = driver.state["observation"]
    observation["axiom_dependencies"]["modules"]["M"] = {
        "error": "Axiom observation timed out; see probe.log", "timeout": True}
    driver.agents = lambda *args: pytest.fail("Incomplete evidence must precede audit")
    driver.step()
    assert driver.state["status"] == "active"
    assert driver.state["next"] == {"role": "reviewer"}
    assert "host_failure" not in driver.state and "adaptation" not in driver.state["audits"]
    assert "timed out" in driver.state["feedback"]
    bind_axiom_fixture(observation)
    driver.state["next"] = task
    driver.agents = lambda *args: {"decision": "accept", "reason": "reviewed", "route_compliance": "not_needed"}
    driver.step()
    assert driver.state["status"] == "prepared"


def test_changed_probe_refreshes_unchanged_source_and_drops_export_check(run):
    calls = []
    def checker(root, output):
        calls.append(output)
        return checked(root, output)
    driver = Driver(run, checker=checker)
    driver.refresh()
    previous_basis = driver.state["observation"]["work_fingerprint"]
    driver.state["observation"]["preparation_checks"] = "prior-probe"
    driver.state["export_check"] = {"passed": True}
    driver.refresh()
    from trellis.modeling.axioms import probe_identity
    assert len(calls) == 2
    assert driver.state["observation"]["work_fingerprint"] == previous_basis
    assert driver.state["observation"]["preparation_checks"] == probe_identity()
    assert "export_check" not in driver.state


def test_run_has_input_baseline_and_ignores_runtime_artifacts(run):
    (run / "work/crate/lib.rs").write_text("pub fn f(x: u8) -> u8 { 0 }\n")
    (run / "driver.log").write_text("runtime log\n")
    status = subprocess.check_output(["git", "status", "--porcelain"], cwd=run, text=True)
    assert status.strip() == "M work/crate/lib.rs"
    baseline = subprocess.check_output(["git", "show", "HEAD:work/crate/lib.rs"], cwd=run, text=True)
    assert baseline == (run / "upstream/lib.rs").read_text()


@pytest.mark.parametrize("support_handoff", [False, True])
def test_axiom_blockers_return_to_adaptation_before_audit(run, support_handoff):
    import hashlib
    from trellis.modeling.native import NativeExportCandidateError
    driver = Driver(run, checker=checked)
    driver.refresh()
    task = {"role": "audit", "kind": "advance", "stage": "adaptation"}
    driver.state.update(stage="adaptation", next=task)
    observation = driver.state["observation"]
    evidence = observation["axiom_dependencies"]["modules"]["M"]
    path = Path(evidence["report"])
    report = json.loads(path.read_text())
    report["declarations"] = [{"name": "demo.f", "line": 4, "pending": False,
                               "axioms": ["Library.Formatter", "demo.f._native.decide.ax_1"]}]
    write_json(path, report)
    evidence["report_sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
    result = {"decision": "accept", "route_compliance": "not_needed", "reason": "reviewed"}
    if support_handoff:
        observation["steps"][-1]["exit_code"] = 1
        result["support_obligations"] = ["Missing float carrier"]
    with pytest.raises(NativeExportCandidateError, match="unapproved axioms"):
        driver.apply(task, result)
    driver.state["next"] = task
    driver.agents = lambda *args: pytest.fail("A known trust blocker must precede audit")
    driver.step()
    assert driver.state["status"] == "active" and driver.state["stage"] == "adaptation"
    assert driver.state["next"] == {"role": "reviewer"}
    assert "host_failure" not in driver.state and "adaptation" not in driver.state["audits"]
    assert "Library.Formatter" in driver.state["feedback"]
    assert "_native.decide" in driver.state["feedback"]
    bind_axiom_fixture(observation)
    driver.state["next"] = task
    driver.agents = lambda *args: result
    driver.step()
    assert driver.state["status"] == "prepared"


@pytest.mark.parametrize("role", ["reviewer", "audit"])
@pytest.mark.parametrize("snapshot_enabled", [False, True])
def test_preparation_source_recourse_uses_preparation_contract(run, monkeypatch, role, snapshot_enabled):
    from types import SimpleNamespace
    if snapshot_enabled:
        monkeypatch.setenv("TRELLIS_REVIEWER_SOURCE_SNAPSHOT", "/readonly/source")
        monkeypatch.setenv("TRELLIS_REVIEWER_SOURCE_SHA", "revision-123")
    else:
        monkeypatch.delenv("TRELLIS_REVIEWER_SOURCE_SNAPSHOT", raising=False)
        monkeypatch.delenv("TRELLIS_REVIEWER_SOURCE_SHA", raising=False)
    driver = Driver(run, checker=checked)
    driver.refresh()
    prompt = agent_prompt(run, driver.state, {"role": role}, SimpleNamespace(raw=run / "raw", done=run / "done"))
    assert ("/readonly/source" in prompt) == snapshot_enabled
    assert "worker burst was judged" not in prompt
    assert "{{" not in prompt


def test_unexpected_preparation_dispatch_failure_is_durable(run):
    def broken(*args):
        raise ValueError("prompt construction failed")
    driver = Driver(run, agents=broken, checker=checked)
    with pytest.raises(RuntimeError, match="preparation_step"):
        driver.step()
    saved = json.loads(driver.path.read_text())
    assert saved["status"] == "halted"
    assert saved["resume_status"] == "active"
    assert saved["host_failure"]["operation"] == "preparation_step"
    assert "prompt construction failed" in Path(saved["host_failure"]["evidence"]).read_text()


def test_preparation_prompt_budgets_total_text_and_preserves_full_context(run):
    from types import SimpleNamespace
    from trellis.agents.codex_headless import BASE_INSTRUCTIONS_DIR
    from trellis.modeling.driver import PREPARATION_PROMPT_CHAR_BUDGET

    driver = Driver(run, checker=checked)
    driver.refresh()
    driver.state["feedback"] = "Detailed finding. " * 2000
    driver.state["history"] = [{"task": {"role": "worker"},
                                "response": {"summary": "Detailed work. " * 2000}}]
    for role in ("reviewer", "worker", "audit"):
        task = {"role": role, "instruction": "Detailed assignment. " * 2000}
        artifacts = SimpleNamespace(raw=run / f"{role}.raw.json", done=run / f"{role}.done")
        prompt = agent_prompt(run, driver.state, task, artifacts)
        base_role = "worker" if role == "worker" else "reviewer"
        assert len(prompt) + len((BASE_INSTRUCTIONS_DIR / f"{base_role}.md").read_text()) <= PREPARATION_PROMPT_CHAR_BUDGET
        assert prompt.count("## Modeling and goal stating (Phase 0)") == 1
        assert prompt.count("not collectively stronger") == 1
        assert (run / "FAITHFULNESS.md").read_text().strip() in prompt
        assert '"observation": {' in prompt
        if role != "audit":
            assert '"latest_work": {' in prompt
        assert '"stage_check": "cargo"' in prompt
        assert "[truncated;" in prompt and str(artifacts.done) in prompt
        full = json.loads((run / f".trellis/modeling/requests/00000-{role}.json").read_text())
        assert full["task"]["reviewer_proposal" if role == "audit" else "instruction"] == task["instruction"]


def test_preparation_prompt_uses_full_context_when_preview_structure_cannot_fit(run, monkeypatch):
    from types import SimpleNamespace
    from trellis.agents.codex_headless import BASE_INSTRUCTIONS_DIR
    from trellis.modeling.driver import PREPARATION_PROMPT_CHAR_BUDGET

    # Even zero-length prose leaves an unbounded number of keys and omission
    # notices. Reopening adds more context without changing the audit contract.
    driver = Driver(run, checker=checked)
    driver.refresh()
    requirements = {f"requirement_{i}": "Preserve this source behavior." for i in range(1000)}
    driver.state["route_decisions"] = [{"resolution": requirements, "assessment": "approved", "sequence": 1}]
    reopening = run / ".trellis/modeling/reopened-from.json"
    write_json(reopening, {"disposition": {"reason": "Rebuild the extraction basis."}})
    monkeypatch.setenv("TRELLIS_REVIEWER_SOURCE_SNAPSHOT", "/readonly/source")
    monkeypatch.setenv("TRELLIS_REVIEWER_SOURCE_SHA", "revision-123")
    original_state = json.dumps(driver.state)
    for role in ("reviewer", "worker", "audit"):
        task = {"role": role, "instruction": "Review the complete approved route."}
        artifacts = SimpleNamespace(raw=run / f"{role}.raw.json", done=run / f"{role}.done")
        prompt = agent_prompt(run, driver.state, task, artifacts)
        base_role = "worker" if role == "worker" else "reviewer"
        assert len(prompt) + len((BASE_INSTRUCTIONS_DIR / f"{base_role}.md").read_text()) <= PREPARATION_PROMPT_CHAR_BUDGET
        assert "Context preview omitted" in prompt
        assert "Read the full role context file below before acting" in prompt
        assert (run / "FAITHFULNESS.md").read_text().strip() in prompt
        assert str(artifacts.raw) in prompt and str(artifacts.done) in prompt
        context_file = run / f".trellis/modeling/requests/00000-{role}.json"
        assert str(context_file) in prompt
        full = json.loads(context_file.read_text())
        assert full["state"]["approved_route"]["resolution"] == requirements
        assert full["task"]["reviewer_proposal" if role == "audit" else "instruction"] == task["instruction"]
        assert full["reopened_preparation"]["record"] == str(reopening)
    assert json.dumps(driver.state) == original_state


def test_preparation_prompt_does_not_omit_instructions_or_mask_other_errors(run, monkeypatch):
    from types import SimpleNamespace
    import trellis.modeling.driver as driver_module
    import trellis.modeling.inspection as inspection

    driver = Driver(run, checker=checked)
    driver.refresh()
    artifacts = SimpleNamespace(raw=run / "raw.json", done=run / "done")
    with monkeypatch.context() as patch:
        patch.setattr(driver_module, "PREPARATION_PROMPT_CHAR_BUDGET", 100)
        with pytest.raises(ValueError, match="instructions and context reference exceed"):
            agent_prompt(run, driver.state, {"role": "audit"}, artifacts)
    def broken_preview(*args, **kwargs):
        raise ValueError("invalid context")
    monkeypatch.setattr(inspection, "context_preview", broken_preview)
    with pytest.raises(ValueError, match="invalid context"):
        agent_prompt(run, driver.state, {"role": "audit"}, artifacts)


def audited(driver):
    driver.refresh()
    for stage in ("setup", "adaptation"):
        driver.state["stage"] = stage
        driver.apply({"role": "audit", "kind": "advance", "stage": stage},
                     {"decision": "accept", "route_compliance": "not_needed", "reason": "reviewed the current artifacts"})


def test_worker_cannot_advance_and_audit_is_required(run):
    driver = Driver(run, checker=checked)
    initial = dict(driver.state["next"])
    assert initial["role"] == "worker" and initial["instruction"]
    seen = []
    def respond(root, state, task):
        seen.append(dict(task))
        return {"summary": "done", "action": "finish"}
    driver.agents = respond
    driver.step()
    assert seen == [initial]
    assert driver.state["next"] == {"role": "reviewer"}
    assert driver.state["stage"] == "setup"
    assert driver.state["audits"] == {}
    with pytest.raises(ValueError, match="Unknown reviewer action"):
        driver.apply({"role": "reviewer"}, {"action": "finish", "instruction": "done"})
    driver.apply({"role": "reviewer"}, {"action": "audit", "instruction": "setup is ready"})
    assert driver.state["next"]["role"] == "audit"


def test_failed_extraction_cannot_pass_stage_audit(run):
    driver = Driver(run, checker=checked)
    driver.refresh()
    driver.state["stage"] = "adaptation"
    driver.state["observation"]["steps"] = [{"name": "aeneas", "exit_code": 1}]
    with pytest.raises(ValueError, match="successful lean"):
        driver.apply({"role": "audit", "kind": "advance", "stage": "adaptation"}, {"decision": "accept", "route_compliance": "not_needed", "reason": "looks fine"})


@pytest.mark.parametrize("blocker", [None, "extraction", "placeholder", "unchecked", "timeout"])
def test_audited_model_support_handoff_requires_completed_extraction(run, blocker):
    driver = Driver(run, checker=checked)
    driver.refresh()
    driver.state["stage"] = "adaptation"
    observation = driver.state["observation"]
    observation["steps"][-1]["exit_code"] = 1
    if blocker == "extraction":
        observation["steps"][-2]["exit_code"] = 1
    elif blocker == "placeholder":
        observation["placeholders"] = 1
    elif blocker == "unchecked":
        observation["steps"].pop()
    elif blocker == "timeout":
        observation["steps"][-1]["execution"] = {"timeout": True}
    task = {"role": "audit", "kind": "advance", "stage": "adaptation"}
    result = {"decision": "accept", "route_compliance": "not_needed", "reason": "The extracted primitive needs a model definition",
              "support_obligations": ["Primitive operation is absent from the installed library"]}
    if blocker:
        with pytest.raises(ValueError, match="Stage audit needs"):
            driver.apply(task, result)
        assert driver.state["status"] == "active"
    else:
        driver.apply(task, result)
        assert driver.state["status"] == "prepared"
        assert driver.state["audits"]["adaptation"]["support_obligations"] == result["support_obligations"]


@pytest.mark.parametrize("stage,edited", [("adaptation", "extraction.json")])
def test_earlier_stage_worker_edits_are_restored_and_referred_to_reviewer(run, stage, edited):
    original = (run / "work" / edited).read_bytes()

    def agent(root, state, task):
        (root / "work" / edited).write_text("changed\n")
        return {"summary": "attempted earlier-stage repair"}

    driver = Driver(run, agents=agent, checker=checked)
    audited(driver)
    driver.state["status"] = "active"
    driver.state["stage"] = stage
    driver.state["next"] = {"role": "worker", "instruction": "repair"}
    driver.step()
    assert (run / "work" / edited).read_bytes() == original
    assert driver.state["next"] == {"role": "reviewer"}
    assert "reopening" in driver.state["feedback"]
    assert driver.state["status"] == "active"


def test_transport_failure_halts_run_with_pending_task_and_evidence(run):
    def agent(*args):
        raise RuntimeError("transport interrupted")

    driver = Driver(run, agents=agent, checker=checked)
    pending = dict(driver.state["next"])
    with pytest.raises(RuntimeError, match="preparation_step"):
        driver.step()
    restarted = Driver(run, checker=checked)
    assert restarted.state["status"] == "halted"
    assert restarted.state["resume_status"] == "active"
    assert "transport interrupted" in Path(restarted.state["host_failure"]["evidence"]).read_text()
    assert restarted.state["next"] == pending
    assert not (run / "handoff.json").exists()


def test_shared_feedback_halt_is_reflected_in_preparation_status(run):
    from trellis.modeling.driver import check_halt, runtime_root
    from trellis.runtime.bridge import system_feedback_halt_marker_path

    def agent(*args):
        marker = system_feedback_halt_marker_path(runtime_root(run))
        marker.parent.mkdir(parents=True, exist_ok=True)
        write_json(marker, {"system_feedback": "operator repair needed"})
        check_halt(run)

    driver = Driver(run, agents=agent, checker=checked)
    pending = dict(driver.state["next"])
    with pytest.raises(RuntimeError, match="halted"):
        driver.step()
    restarted = Driver(run, checker=checked)
    assert restarted.state["status"] == "halted"
    assert restarted.state["resume_status"] == "active"
    assert restarted.state["next"] == pending


def test_goal_identity_is_explicit_and_shared_context_is_preserved(tmp_path):
    source = tmp_path / "source"
    source.mkdir()
    goal = tmp_path / "GOAL.md"
    goal.write_text("## Obligations\n### first\nFirst claim.\n")
    root = tmp_path / "run"
    with pytest.raises(ValueError, match="explicit IDs"):
        initialize(root, source, goal, {}, model="gpt-5.6-terra", effort="high")
    write_json(goal, {"context": "Shared meaning.", "goals": [{"id": "first", "text": "Complete first claim."},
                                                             {"id": "second", "text": "Complete second claim."}]})
    initialize(root, source, goal, {}, model="gpt-5.6-terra", effort="high")
    config = json.loads((root / "config.json").read_text())
    assert [g["id"] for g in config["goals"]] == ["first", "second"]
    assert (root / "goals.json").read_bytes() == goal.read_bytes()
    assert "Shared meaning." in (root / "GOAL.md").read_text()
    assert "Complete second claim." in (root / "GOAL.md").read_text()




def test_adaptation_can_update_crate_build_metadata(run):
    def agent(root, state, task):
        (root / "work/crate/Cargo.toml").write_text('[package]\nname="adapted"\nversion="0.1.0"\n')
        return {"summary": "adapted the crate harness"}

    driver = Driver(run, agents=agent, checker=checked)
    audited(driver)
    driver.state["status"] = "active"
    driver.state["stage"] = "adaptation"
    driver.state["next"] = {"role": "worker", "instruction": "adapt"}
    driver.step()
    assert "adapted" in (run / "work/crate/Cargo.toml").read_text()
    assert not driver.state["feedback"]
    assert driver.state["audits"]["setup"]["basis"] == setup_fingerprint(run)


def test_later_compilation_error_does_not_block_setup_audit(run):
    observation = checked(run, None)
    observation["error"] = "invalid module graph"
    assert passed(observation, "cargo")
    assert not passed(observation, "lean")
    observation["stable"] = False
    assert not passed(observation, "cargo")


def test_interrupt_restores_closed_source_and_retains_pending_work(run):
    original = (run / "work/extraction.json").read_bytes()

    def agent(root, state, task):
        (root / "work/extraction.json").write_text("changed\n")
        raise KeyboardInterrupt()

    driver = Driver(run, agents=agent, checker=checked)
    audited(driver)
    driver.state["status"] = "active"
    driver.state["stage"] = "adaptation"
    driver.state["next"] = {"role": "worker", "instruction": "complete the model"}
    with pytest.raises(KeyboardInterrupt):
        driver.step()
    assert (run / "work/extraction.json").read_bytes() == original
    restarted = Driver(run, checker=checked)
    assert restarted.state["next"]["role"] == "worker"
    assert restarted.state["last_error"] == "KeyboardInterrupt"
    assert restarted.state["status"] == "active"


def test_setup_reopening_requires_audit_and_invalidates_approval(run):
    from trellis.modeling.driver import response_contract
    from trellis.modeling.inspection import role_context

    driver = Driver(run, checker=checked)
    audited(driver)
    driver.state.update(status="active", stage="adaptation")

    def respond(task, result):
        driver.state["next"] = task
        driver.agents = lambda *args: result
        driver.step()

    assert '"action": "work|audit|review_route|reopen|fail"' in response_contract("reviewer", "adaptation")
    driver.apply({"role": "reviewer"}, {"action": "reopen", "instruction": "Repair extraction settings"})
    task = driver.state["next"]
    assert task["kind"] == "reopen"
    respond(task, {"decision": "revise", "reason": "First establish the cause"})
    assert driver.state["stage"] == "adaptation"
    assert driver.state["audits"]
    assert "approved_setup_reopening" not in role_context(driver.state, {"role": "reviewer"})
    reason = "The extraction setting needs repair"
    respond(task, {"decision": "accept", "route_compliance": "not_needed", "reason": reason})
    assert driver.state["stage"] == "setup"
    assert driver.state["audits"] == {}
    grant = {"audit_sequence": driver.state["sequence"], "from_stage": "adaptation", "to_stage": "setup",
             "decision": "accept", "reason": reason}
    for role in ("reviewer", "worker", "audit"):
        context = role_context(driver.state, {"role": role})
        assert context["approved_setup_reopening"] == grant
        assert context["feedback"] == "" and driver.state["feedback"] == reason
    assert '"action": "work|audit|review_route|fail"' in response_contract("reviewer", "setup")

    # Invalid reviewer/audit attempts remain history, without replacing the
    # accepted reopening or discarding its authority from subsequent context.
    respond({"role": "reviewer"}, {"action": "reopen", "instruction": "Reopen again"})
    feedback = driver.state["feedback"]
    context = role_context(driver.state, {"role": "reviewer"})
    assert context["feedback"] == feedback and "Only adaptation" in feedback
    assert context["approved_setup_reopening"] == grant
    respond(task, {"decision": "accept", "route_compliance": "not_needed", "reason": "Invalid repeated approval"})
    assert role_context(driver.state, {"role": "reviewer"})["approved_setup_reopening"] == grant

    respond({"role": "reviewer"}, {"action": "audit", "instruction": "Revised setup is ready"})
    assert role_context(driver.state, {"role": "audit"})["approved_setup_reopening"] == grant
    respond(driver.state["next"], {"decision": "accept", "route_compliance": "not_needed", "reason": "Reviewed revised setup"})
    assert driver.state["stage"] == "adaptation"
    assert "approved_setup_reopening" not in role_context(driver.state, {"role": "reviewer"})


@pytest.mark.parametrize("pause_at_completion", [False, True])
def test_interrupted_bootstrap_has_retryable_failure_receipt(run, tmp_path, monkeypatch, pause_at_completion):
    from scripts import pv_modeling
    from trellis.modeling import native
    driver = Driver(run, checker=checked)
    audited(driver)
    driver.save()
    repo, runtime = tmp_path / "native", tmp_path / "runtime"
    def interrupted(root, destination, output, **kwargs):
        destination.mkdir()
        (destination / "partial.txt").write_text("keep this evidence")
        if pause_at_completion:
            write_json(native.export_directory(root, destination) / "prepared.json", {"launch_environment": {}})
            (root / ".trellis/modeling/pause").touch()
            return destination / "trellis.config.json"
        raise KeyboardInterrupt
    monkeypatch.setattr(native, "prepare", interrupted)
    with pytest.raises(RuntimeError, match="Native bootstrap interrupted"):
        pv_modeling._main(["run", str(run), "--repo", str(repo), "--runtime", str(runtime), "--no-viewer"])
    state = json.loads(driver.path.read_text())
    assert state["status"] == "halted" and state["resume_status"] == "prepared"
    evidence = json.loads(Path(state["host_failure"]["evidence"]).read_text())
    assert evidence["interrupted"] is True
    archive = native.preserve_failed_bootstrap(run, repo, runtime)
    assert (archive / "partial-repository/partial.txt").read_text() == "keep this evidence"
    assert not repo.exists()


def test_controller_stop_is_not_cleared_by_driver_start(run, tmp_path, monkeypatch):
    from scripts.pv_modeling import _main
    root = run
    stop = tmp_path / 'controller-stop'
    stop.touch()
    monkeypatch.setattr(Driver, 'step', lambda *_: pytest.fail('dispatched across controller stop'))
    assert _main(['run', str(root), '--stop-file', str(stop)]) == 0
    assert stop.exists()
    assert json.loads((root / '.trellis/modeling/state.json').read_text())['sequence'] == 0


def test_duplicate_preparation_driver_is_refused_cleanly(run):
    import fcntl
    from scripts.pv_modeling import _main
    with (run / '.trellis/modeling/runner.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        with pytest.raises(ValueError, match='already has an active driver'):
            _main(['run', str(run)])
