"""Regressions for the seven independently reproduced engineering defects."""
import copy
import json
from pathlib import Path

import pytest

from test_paper_engineering import root, review, measure, accept, FixtureAgent, PLAN, kernel_result
from trellis.modeling import driver, engineering as eng, performance as perf, performance_loop as loop
from trellis.modeling.observations import read_json, write_json
from trellis.paper_implementation import sha256


def tree(path):
    return {name: (digest, (path / name).stat().st_mode & 0o777)
            for name, digest in perf._inventory(path).items()}


def two_candidates(root, monkeypatch):
    instance, _ = review(root, monkeypatch)
    state = instance.state
    first = measure(root, state)
    code = root / "work/crate/src/lib.rs"
    code.write_text(code.read_text() + "// second reviewed candidate\n")
    eng.apply(root, state, {"purpose": "code_review"}, {"decision": "accept", "reason": "Reviewed second candidate."})
    second = measure(root, state)
    return instance, first, second


def test_f1_handoff_rationale_uses_existing_worker_paths_and_driver(root, monkeypatch):
    instance, _ = review(root, monkeypatch)
    accept(root, instance.state, measure(root, instance.state))
    eng.handoff(root, instance.state)
    assert eng.ADAPTATION_NOTE in instance.state["next"]["instruction"]
    for stage in ("setup", "adaptation"):
        assert any(Path(eng.ADAPTATION_NOTE).is_relative_to(Path(path)) for path in driver.worker_writable_paths(stage))
    instance.checker = lambda *args: {"steps": []}
    def author_note(root, state, task):
        assert task["role"] == "worker"
        assert any(Path(eng.ADAPTATION_NOTE).is_relative_to(Path(path)) for path in driver.worker_writable_paths(state["stage"]))
        (root / eng.ADAPTATION_NOTE).write_text("Retain the algorithm and regression harness; explain representation changes here.\n")
        return kernel_result("implementation", {"summary": "Recorded adaptation rationale."})
    instance.agents = author_note
    instance.step()
    assert instance.state["next"]["role"] == "reviewer"
    assert (root / eng.ADAPTATION_NOTE).is_file()
    assert not {"human_approved", "assumptions"} & instance.state.keys()


@pytest.mark.parametrize("name", ["target/cases.txt", ".git/config", ".lake/input.bin", "__pycache__/data"])
def test_f2_complete_harness_selection_delivery_and_rollback(root, monkeypatch, name):
    instance, _ = review(root, monkeypatch)
    state = instance.state
    path = root / "benchmarks/harness" / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("measured workload\n")
    path.chmod(0o750)
    run = measure(root, state)
    before = tree(root / "benchmarks/harness")
    view = root.parent / "harness-view"
    eng.delivery(root, state, {"role": "performance"}, view)
    assert tree(view / "benchmarks/harness") == before
    original = eng.freeze
    def fail_after_publish(root, state):
        original(root, state)
        raise ValueError("fixture late failure")
    monkeypatch.setattr(eng, "freeze", fail_after_publish)
    with pytest.raises(ValueError, match="late failure"):
        accept(root, state, run)
    assert tree(root / "benchmarks/harness") == before
    assert not (root / eng.STORE / "selection-backup").exists()
    assert not (root / eng.STORE / "baseline.json").exists()
    assert not (root / eng.BASELINE_REPORT).exists()
    monkeypatch.setattr(eng, "freeze", original)
    accept(root, state, run)
    assert tree(root / "benchmarks/harness") == before
    assert state["status"] == "engineering_ready"


@pytest.mark.parametrize("point", ["measurement", "ready", "handoff"])
@pytest.mark.parametrize("damage", ["raw", "mode", "receipt"])
def test_f3_cargo_damage_blocks_before_ready_or_handoff(root, monkeypatch, point, damage):
    instance, _ = review(root, monkeypatch)
    state = instance.state
    run = measure(root, state)
    if point == "handoff":
        accept(root, state, run)
    audit = state["audits"]["engineering"]
    directory = root / perf.STORE / "runs" / audit["checks"]
    record = read_json(directory / "record.json")
    if damage == "receipt":
        record["reason"] = "Changed after independent code approval."
        write_json(directory / "record.json", record)
        # Even a replaced unkeyed receipt seal cannot replace the audit binding.
        (directory / "record.sha256").write_text(sha256(directory / "record.json") + "\n")
    else:
        path = directory / record["commands"][0]["stderr"]
        if damage == "raw":
            path.write_text("changed evidence\n")
        else:
            path.chmod(path.stat().st_mode ^ 0o100)
    before_state = copy.deepcopy(state)
    before_upstream = tree(root / "upstream")
    with pytest.raises(ValueError):
        if point == "measurement": measure(root, state)
        elif point == "ready": accept(root, state, run)
        else: eng.handoff(root, state)
    assert state == before_state
    assert tree(root / "upstream") == before_upstream
    assert not state["engineering"].get("handed_off")


def test_f3_benchmark_changes_reuse_bound_cargo_checks(root, monkeypatch):
    instance, _ = review(root, monkeypatch)
    state = instance.state
    audit = copy.deepcopy(state["audits"]["engineering"])
    (root / "benchmarks/harness/new-workload.txt").write_text("later benchmark data")
    def unexpected(*args): pytest.fail("Unchanged source checks rerun for benchmark-only changes")
    monkeypatch.setattr(perf, "run_measurement", unexpected)
    eng.current_approval(root, state)
    assert eng.check(root, state)["id"] == audit["checks"]
    assert state["audits"]["engineering"] == audit


@pytest.mark.parametrize("damage", ["unlocked", "failed", "timed-out", "wrong-command"])
def test_f3_check_validation_requires_exact_successful_commands(root, monkeypatch, damage):
    instance, _ = review(root, monkeypatch)
    record = copy.deepcopy(instance.state["observation"])
    if damage == "unlocked": record["commands"][0]["argv"].remove("--locked")
    elif damage == "failed": record["commands"][0]["exit_code"] = 1
    elif damage == "timed-out": record["commands"][1]["timeout"] = True
    else: record["commands"][1]["argv"] = ["/usr/bin/true"]
    assert not eng._check_passed(record)


@pytest.mark.parametrize("failure", ["missing-report", "late-freeze"])
def test_f4_acceptance_is_transactional_across_candidate_selection(root, monkeypatch, failure):
    instance, first, second = two_candidates(root, monkeypatch)
    state = instance.state
    code_cache = root / "work/crate/target/keep.txt"
    code_cache.parent.mkdir()
    code_cache.write_text("rollback retains unmeasured build products")
    before_source, before_harness = tree(root / "work/crate"), tree(root / "benchmarks/harness")
    before_state = copy.deepcopy(state)
    state["engineering"]["retry_feedback"] = "An earlier rejected response."
    before_state = copy.deepcopy(state)
    before_plan = (root / "engineering/plan.json").read_bytes()
    if failure == "late-freeze":
        (root / "benchmarks/assessment.md").write_text("Select the earlier candidate.")
        original = eng.freeze
        def fail(root, state):
            original(root, state)
            raise ValueError("late acceptance error")
        monkeypatch.setattr(eng, "freeze", fail)
    with pytest.raises(ValueError):
        loop.apply(root, state, {"action": "accept", "reason": "Select earlier candidate.",
                                "evidence": [first], "report": "benchmarks/assessment.md"})
    assert state == before_state
    assert tree(root / "work/crate") == before_source
    assert tree(root / "benchmarks/harness") == before_harness
    assert (root / "engineering/plan.json").read_bytes() == before_plan
    assert not (root / loop.ACCEPTED).exists()
    assert not (root / eng.STORE / "baseline.json").exists()


def test_f5_baseline_assessment_survives_adaptation_and_exports(root, monkeypatch):
    instance, _ = review(root, monkeypatch)
    state = instance.state
    baseline_run = measure(root, state)
    accept(root, state, baseline_run)
    original_report = (root / "benchmarks/report.md").read_bytes()
    eng.handoff(root, state)
    (root / eng.ADAPTATION_NOTE).write_text("Keep the algorithm and repeat the baseline workloads.\n")
    state["stage"] = "adaptation"
    state["audits"]["adaptation"] = {"decision": "accept"}
    monkeypatch.setattr("trellis.modeling.paper_choices.validate_audit", lambda *args: None)
    loop.begin(root, state)
    run = measure(root, state)
    (root / "benchmarks/report.md").write_text("Separate later adaptation assessment.\n")
    loop.apply(root, state, {"action": "accept", "reason": "Rechecked baseline workloads.",
                            "evidence": [run], "report": "benchmarks/report.md"})
    destination = root.parent / "baseline-export"
    exported = eng.export_evidence(root, state, destination)
    name = "paper-implementation/engineering/baseline-assessment.md"
    assert name in exported and (destination / name).read_bytes() == original_report
    (root / eng.BASELINE_REPORT).write_text("damaged baseline report")
    with pytest.raises(ValueError, match="assessment changed"):
        eng.validate_handoff(root, state)


def test_handoff_rejects_baseline_harness_mode_drift_after_remeasurement(root, monkeypatch):
    instance, _ = review(root, monkeypatch)
    state = instance.state
    selector = root / "benchmarks/harness/workload-selector"
    selector.write_text("permission-sensitive workload selector\n")
    selector.chmod(0o750)
    accept(root, state, measure(root, state))
    eng.handoff(root, state)
    (root / eng.ADAPTATION_NOTE).write_text("Retain the baseline workload.\n")
    state["stage"] = "adaptation"
    state["audits"]["adaptation"] = {"decision": "accept"}
    monkeypatch.setattr("trellis.modeling.paper_choices.validate_audit", lambda *args: None)
    selector.chmod(0o640)
    loop.begin(root, state)
    accept(root, state, measure(root, state))
    with pytest.raises(ValueError, match="retain the baseline.*harness"):
        eng.validate_handoff(root, state)
    with pytest.raises(ValueError, match="retain the baseline.*harness"):
        eng.export_evidence(root, state, root.parent / "mode-drift-export")


def test_f6_profile_outputs_and_failed_measurements_reach_the_view(root, monkeypatch):
    instance, _ = review(root, monkeypatch)
    state = instance.state
    def profiled(argv, *, cwd, env, stdout, stderr, timeout):
        stdout.write_text("profiling tool ran\n"); stderr.write_text("partial profile: unavailable counters\n")
        (cwd / "output/profile.json").write_text('{"dominant_cost":"allocation"}')
        return {"exit_code": 1, "timeout": False}
    monkeypatch.setattr(perf, "_run_bounded", profiled)
    run = measure(root, state)
    view = root.parent / "profile-view"
    manifest = eng.delivery(root, state, {"role": "performance"}, view)
    record = read_json(view / "measurements" / run / "record.json")
    assert not record["successful_run"]
    assert record["current_source"] and record["current_harness"]
    assert f"measurements/{run}/artifacts/output/profile.json" in manifest["files"]
    for name, expected in record["evidence_files"].items():
        assert sha256(view / "measurements" / run / name) == expected
    for command in record["commands"]:
        for field in ("stdout", "stderr", "generated_inputs", "retained_entrypoint"):
            if field in command: assert (view / "measurements" / run / command[field]).is_file()
    assert "basis.json" not in record["evidence_files"]


def test_f7_driver_and_performance_retry_feedback_is_public_and_actionable(root, monkeypatch):
    monkeypatch.setattr(driver, "commit_checkpoint", lambda *args: None)
    instance = driver.Driver(root, agents=FixtureAgent())
    instance.step(); instance.step()
    instance.agents = lambda *args: {}
    instance.step()
    view = root.parent / "verdict-retry"
    eng.delivery(root, instance.state, instance.state["next"], view)
    assert "decision" in read_json(view / "feedback.json")["validation"]
    def succeed(argv, *, cwd, env, stdout, stderr, timeout):
        stdout.write_text(""); stderr.write_text(""); return {"exit_code": 0, "timeout": False}
    monkeypatch.setattr(perf, "_run_bounded", succeed)
    instance.agents = FixtureAgent()
    # Skip rewriting a delivery directory from the same fixture sequence.
    instance.step(); instance.step()
    # Complete code review with the measurement runner fixture.
    instance.step()
    state = instance.state
    loop._dispatch(root, state, {"role": "performance"}, lambda *args: {"action": "accept", "reason": "Select."})
    state["feedback"] += " HOST_ONLY_AENEAS_TRACE"
    view = root.parent / "performance-retry"
    eng.delivery(root, state, state["next"], view)
    feedback = read_json(view / "feedback.json")
    assert "report" in feedback["validation"] and "evidence" in feedback["validation"]
    assert "HOST_ONLY" not in json.dumps(feedback)
