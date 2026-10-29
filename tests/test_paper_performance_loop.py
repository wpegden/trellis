"""Preparation control tests; real measurement isolation lives in its own suite."""
import json
from dataclasses import make_dataclass
from types import SimpleNamespace

import pytest

from trellis.modeling import performance_loop as loop
from trellis.modeling.observations import read_json, write_json
from trellis.paper_implementation import sha256


@pytest.fixture(params=["lean_implementation", "adaptation"])
def candidate(tmp_path, monkeypatch, request):
    root = tmp_path / "preparation"
    root.mkdir()
    write_json(root / "config.json", {"paper_input": {
        "route": "lean_native" if request.param == "lean_implementation" else "rust_aeneas",
        "modeling_choices": []}})
    state = {"stage": request.param, "status": "active", "sequence": 0, "history": [],
             "audits": {request.param: {"decision": "accept", "basis": "audited-source"}}}
    basis = {"value": "source-build-tools"}
    monkeypatch.setattr(loop.performance, "current_basis", lambda root: basis["value"])
    monkeypatch.setattr("trellis.modeling.driver.check_halt", lambda root: None)
    loop.begin(root, state)
    (root / "benchmarks/report.md").write_text("Coverage, measurements and reasons to stop.")
    return root, state, basis


def measured(root, state, monkeypatch):
    run_id = "a" * 32
    path = root / loop.performance.STORE / "runs" / run_id / "record.json"
    record = {"id": run_id, "successful_run": True, "evidence_files": {}}
    write_json(path, record)
    record.update(record_path=path.relative_to(root).as_posix(), record_sha256=sha256(path))
    monkeypatch.setattr(loop.performance, "run_measurement", lambda *args: record)
    monkeypatch.setattr(loop.performance, "validate_evidence", lambda root, state, ids: [record])
    loop.apply(root, state, {"action": "measure", "reason": "test entry point", "commands": []})
    return {"action": "accept", "reason": "No worthwhile remaining improvement", "evidence": [run_id], "report": "benchmarks/report.md"}


def test_measure_accept_export_pins_evidence_without_proof_or_human_state(candidate, monkeypatch, tmp_path):
    root, state, _ = candidate
    response = measured(root, state, monkeypatch)
    loop.apply(root, state, response)
    assert state["status"] == "prepared"
    assert state["stage"] in {"modeling", "lean_implementation"}
    approval, records = loop.validate_acceptance(root, state)
    files = loop.export_evidence(root, state, tmp_path / "export")
    assert "paper-implementation/performance/review.json" in files
    assert "paper-implementation/performance/report.md" in files
    assert approval["evidence"] == [records[0]["id"]]
    assert not {"human_approved", "assumptions", "target_claims", "cost_obligations"} & state.keys()


def test_improvement_removes_acceptance_and_requires_algorithm_review(candidate, monkeypatch):
    root, state, _ = candidate
    response = measured(root, state, monkeypatch)
    loop.apply(root, state, response)
    loop.apply(root, state, {"action": "improve", "reason": "Allocation dominates", "instruction": "Reuse the existing buffer"})
    assert state["status"] == "active" and state["next"]["role"] == "reviewer"
    assert not state["audits"]
    assert not (root / loop.ACCEPTED).exists()
    assert not state["performance"].get("approval")
    assert state["performance"]["history"]
    with pytest.raises(ValueError, match="audit approval"):
        loop.apply(root, state, response)


@pytest.mark.parametrize("change", ["basis", "audit", "report", "harness", "receipt"])
def test_acceptance_invalidated_by_each_reviewed_dependency(candidate, monkeypatch, change):
    root, state, basis = candidate
    response = measured(root, state, monkeypatch)
    loop.apply(root, state, response)
    if change == "basis":
        basis["value"] += "changed"
    elif change == "audit":
        next(iter(state["audits"].values()))["reason"] = "different review"
    elif change == "report":
        (root / "benchmarks/report.md").write_text("different assessment")
    elif change == "harness":
        (root / "benchmarks/harness/new.py").write_text("pass")
    else:
        write_json(root / loop.ACCEPTED, {})
    with pytest.raises(ValueError):
        loop.validate_acceptance(root, state)


def test_changed_candidate_reopens_before_dispatch(candidate):
    root, state, basis = candidate
    basis["value"] = "changed"
    def agent(*args):
        pytest.fail("Stale candidate must return through algorithm review first")
    loop.step(root, state, agent)
    assert state["next"]["role"] == "reviewer" and not state["audits"]


def test_changed_candidate_during_agent_burst_reopens(candidate):
    root, state, basis = candidate
    def agent(*args):
        basis["value"] = "changed during burst"
        return {"action": "fail", "reason": "stale"}
    loop.step(root, state, agent)
    assert state["next"]["role"] == "reviewer" and state["status"] == "active"


def test_invalid_response_stays_with_performance_agent(candidate):
    root, state, _ = candidate
    loop.step(root, state, lambda *args: {"action": "accept", "reason": ""})
    assert state["next"]["role"] == "performance" and state["feedback"]


def test_empty_assessment_cannot_satisfy_acceptance(candidate, monkeypatch):
    root, state, _ = candidate
    raw = measured(root, state, monkeypatch)
    (root / "benchmarks/report.md").write_text("  \n")
    with pytest.raises(ValueError, match="nonempty"):
        loop.apply(root, state, raw)
    assert state["status"] == "active" and not (root / loop.ACCEPTED).exists()


@pytest.mark.parametrize("failure", [OSError("measurement host failure"), KeyboardInterrupt()])
def test_performance_host_failure_and_interrupt_save_state(candidate, monkeypatch, failure):
    root, state, _ = candidate
    calls = []
    monkeypatch.setattr("trellis.agent_wrapper.executor.record_host_failure", lambda *a, **kw: calls.append(kw))
    monkeypatch.setattr("trellis.runtime.bridge.any_halt_marker_present", lambda *a: False)
    def agent(*args):
        raise failure
    with pytest.raises(type(failure)):
        loop.step(root, state, agent)
    saved = json.loads((root / ".trellis/modeling/state.json").read_text())
    assert saved["sequence"] == 1 and saved["last_error"]
    if isinstance(failure, Exception):
        assert saved["status"] == "halted" and calls[0]["operation"] == "performance_step"
    else:
        assert saved["status"] == "active" and not calls


def test_performance_respects_existing_agent_halt_marker(candidate, monkeypatch):
    root, state, _ = candidate
    marker = {"present": False}
    monkeypatch.setattr("trellis.runtime.bridge.any_halt_marker_present", lambda *a: marker["present"])
    def agent(*args):
        marker["present"] = True
        raise RuntimeError("Agent feedback halted the run")
    with pytest.raises(RuntimeError):
        loop.step(root, state, agent)
    assert state["status"] == "halted" and state["resume_status"] == "active"


def test_paper_performance_driver_dispatch_skips_source_checker(candidate, monkeypatch):
    from trellis.modeling.driver import Driver
    from trellis.paper_implementation import LeanPreparationDriver
    root, state, _ = candidate
    driver = object.__new__(LeanPreparationDriver if state["stage"] == "lean_implementation" else Driver)
    driver.root, driver.state = root, state
    driver.config = read_json(root / 'config.json')
    driver.agents = lambda *args: {"action": "fail", "reason": "No representative workload available"}
    driver.checker = lambda *args: pytest.fail("Harness-only dispatch must not re-run source checking")
    assert driver.step()["status"] == "failed"


def test_canonical_exports_reject_unaccepted_performance_before_build_or_copy(candidate, monkeypatch, tmp_path):
    from trellis.modeling import native
    from trellis.paper_implementation import export_lean_preparation
    root, state, _ = candidate
    state["status"] = "prepared"
    destination, runtime = tmp_path / "export", tmp_path / "runtime"
    if state["stage"] == "lean_implementation":
        write_json(root / ".trellis/modeling/state.json", state)
        monkeypatch.setattr("trellis.paper_implementation.check_lean_preparation", lambda *args: pytest.fail("Missing performance acceptance must fail before rebuilding"))
        with pytest.raises(ValueError, match="performance acceptance"):
            export_lean_preparation(root, destination, runtime, kernel_command="unused")
    else:
        state["audits"]["setup"] = {"decision": "accept", "basis": "setup"}
        state["observation"] = {"raw_models": ["generated.lean"]}
        write_json(root / ".trellis/modeling/state.json", state)
        write_json(root / "config.json", {"paper_input": {}})
        monkeypatch.setattr(native, "setup_fingerprint", lambda *args: "setup")
        monkeypatch.setattr(native, "source_fingerprint", lambda *args: "audited-source")
        with pytest.raises(ValueError, match="performance acceptance"):
            native.prepare(root, destination, runtime)
    assert not destination.exists() and not runtime.exists()


def test_rust_stage_audit_starts_performance_only_for_paper(tmp_path, monkeypatch):
    from trellis.modeling import driver
    monkeypatch.setattr(driver, "bootstrap_configuration_errors", lambda *args: [])
    monkeypatch.setattr(driver, "source_fingerprint", lambda *args: "source")
    monkeypatch.setattr(loop.performance, "current_basis", lambda *args: "candidate")
    for paper in (False, True):
        root = tmp_path / str(paper)
        write_json(root / "config.json", {"paper_input": {}} if paper else {})
        item = object.__new__(driver.Driver)
        item.root = root
        item.state = {"stage": "adaptation", "status": "active", "audits": {},
                      "observation": {"steps": [{"name": "lean", "exit_code": 0}]},
                      "export_check": {"axiom_compatibility": {"sha256": "compatibility"}}}
        item.validate_export = lambda: None
        item.apply({"role": "audit", "kind": "advance", "stage": "adaptation"},
                   {"decision": "accept", "reason": "Correspondence reviewed", "route_compliance": "not_needed"})
        assert item.state["status"] == ("active" if paper else "prepared")
        assert (item.state["next"]["role"] == "performance") == paper


def test_performance_role_provider_override_and_reviewer_fallback():
    from trellis.modeling.driver import preparation_provider
    config = SimpleNamespace(worker="worker", reviewer="reviewer")
    assert preparation_provider(config, {}, "performance") == "reviewer"
    inputs = {"launch_config": {"preparation": {"performance": {"provider": "codex", "model": "performance-model"}}}}
    assert preparation_provider(config, inputs, "performance").model == "performance-model"


def test_mathlib_measurement_selects_exact_core_inputs_and_pins_proof_metadata(tmp_path):
    from trellis.paper_implementation import NATIVE_LAKEFILE, NATIVE_LOCK, PROOF_INVENTORY, MATHLIB_LOCK, lean_lakefile, lean_lake_manifest
    write_json(tmp_path / "config.json", {"paper_input": {"entry_points": [{"node_id": "Algorithm"}]}, "proof_project": {"kind": "mathlib"}})
    candidate = tmp_path / "work/lean"
    (candidate / "Tablet").mkdir(parents=True)
    source = candidate / "Tablet/Algorithm.lean"
    source.write_text("def Algorithm : Nat := 2\n")
    for name, text in ((NATIVE_LAKEFILE, lean_lakefile(["Algorithm"])), (NATIVE_LOCK, lean_lake_manifest()),
                       ("lean-toolchain", "pinned-toolchain"), ("lakefile.toml", "proof project with Mathlib"),
                       (PROOF_INVENTORY, "pinned-proof-dependencies"), (MATHLIB_LOCK, "pinned-Mathlib-lock")):
        path = candidate / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    inputs, proof = loop.performance._implementation_inputs(tmp_path, "lean_native")
    assert inputs["Tablet/Algorithm.lean"] == source
    assert inputs["lakefile.toml"] == candidate / NATIVE_LAKEFILE
    assert inputs["lake-manifest.json"] == candidate / NATIVE_LOCK
    assert set(proof) == {PROOF_INVENTORY, MATHLIB_LOCK}
    assert proof[PROOF_INVENTORY] == sha256(candidate / PROOF_INVENTORY)


@pytest.mark.parametrize("raw", [
    {"action": "measure", "reason": "Measure implementation", "commands": [{"name": "run", "argv": ["/build/probe", ""], "cwd": "build", "kind": "run"}]},
    {"action": "improve", "reason": "Allocation dominates", "instruction": "Reuse the buffer"},
    {"action": "accept", "reason": "No worthwhile remaining improvement", "evidence": ["a" * 32], "report": "benchmarks/report.md"},
    {"action": "fail", "reason": "Representative inputs unavailable"},
])
def test_advertised_performance_actions_use_real_artifact_validation(tmp_path, raw):
    from trellis.checking import validate_json_artifact
    path = tmp_path / "response.json"
    write_json(path, raw)
    result = validate_json_artifact("preparation-performance-result", path)
    assert result["ok"], result
    assert result["data"] == raw


def test_performance_dispatch_selects_own_base_and_confines_writes(candidate, monkeypatch):
    from trellis.modeling import driver
    from trellis.config import SandboxConfig
    from trellis.agent_wrapper.protocol import SingleAgentRequest
    from trellis.agent_wrapper.executor import _base_instructions_key_for_request
    root, state, _ = candidate
    config = make_dataclass("ConfigFixture", ["sandbox", "reviewer"])(SandboxConfig(), "review-provider")
    write_json(root / "config.json", {"paper_input": {}, "tools": {}})
    monkeypatch.setattr(driver, "load_config", lambda path: config)
    requests = []
    def build(**kwargs):
        sandbox = kwargs["config"].sandbox
        assert sandbox.extra_writable_paths == ("benchmarks",)
        return SingleAgentRequest(request_id="performance", cycle=0, kind="performance", burst_role=kwargs["burst_role"],
            provider=SimpleNamespace(), prompt="", work_dir=root, state_dir=root / ".trellis/agents",
            session_name="performance", lane=kwargs["lane"], timeout_seconds=120,
            artifact=kwargs["artifact"], log_dir=root / ".trellis/logs", sandbox=sandbox)
    monkeypatch.setattr(driver, "_single_request_common", build)
    def execute(request):
        requests.append(request)
        return SimpleNamespace(ok=True, payload={"action": "fail", "reason": "fixture"},
                               to_dict=lambda: {"ok": True})
    monkeypatch.setattr(driver, "execute_agent_request", execute)
    # Request serialization expects a provider dataclass; keep that external detail out of this scope test.
    monkeypatch.setattr(SingleAgentRequest, "to_dict", lambda self: {"request_id": self.request_id})
    driver.Agents()(root, state, {"role": "performance"})
    request = requests[0]
    assert _base_instructions_key_for_request(request) == "performance"
    assert request.burst_role == "reviewer" and request.artifact.kind == "preparation-performance-result"
    assert "Write harness files" in request.prompt


@pytest.mark.parametrize("scenario", ["first", "measured", "improvement", "stale"])
def test_render_complete_performance_prompt(tmp_path, monkeypatch, scenario):
    from trellis.modeling.driver import agent_prompt
    from trellis.agents.codex_headless import BASE_INSTRUCTIONS_DIR
    root = tmp_path / scenario
    write_json(root / "config.json", {"paper_input": {"route": "lean_native"}})
    state = {"stage": "lean_implementation", "sequence": 1, "performance": {"runs": []}, "audits": {}, "feedback": ""}
    if scenario == "measured":
        state["performance"].update(runs=["a" * 32], latest_measurement={"successful_run": True})
    if scenario == "improvement":
        state["audits"] = {"lean_implementation": {"decision": "accept", "reason": "Reused storage preserves proof invariants"}}
    if scenario == "stale":
        state["feedback"] = "Harness changed; measure current inputs"
    text = (BASE_INSTRUCTIONS_DIR / "performance.md").read_text() + "\n" + agent_prompt(
        root, state, {"role": "performance"}, SimpleNamespace(raw=root / "raw.json", done=root / "done"))
    (root / "full-prompt.txt").write_text(text)
    assert text.count("Supporting files are preserved under `paper/`") == 1
    assert "paper/TRELLIS_REFERENCES.json" in text
    assert text.count("Computational proof evidence requires Lean proofs") == 1
    assert "preparation-performance-result" in text and "natural-language proof" in text
    assert "GOAL.md" not in text
    for action in ("measure", "improve", "accept", "fail"):
        assert f"`{action}`" in text
