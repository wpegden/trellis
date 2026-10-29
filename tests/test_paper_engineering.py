"""The existing Driver runs engineering fixtures without provider/model calls."""
import copy
import json
import os
from pathlib import Path
import shutil
import subprocess

import pytest

from trellis.modeling import driver, engineering as eng, performance as perf, performance_loop as loop
from trellis.modeling.observations import read_json, write_json
from trellis.paper_implementation import initialize_rust_preparation, paper_input
from trellis.runtime.kernel_cli import run_kernel_cli

def rust_bin():
    if os.environ.get("TRELLIS_TEST_RUSTC"):
        return Path(os.environ["TRELLIS_TEST_RUSTC"]).parent
    if shutil.which("rustup"):
        result = subprocess.run(["rustup", "which", "cargo"], capture_output=True, text=True, check=True)
        return Path(result.stdout.strip()).parent
    cargo = shutil.which("cargo")
    if not cargo:
        pytest.skip("An installed Rust toolchain is required for preparation identity tests")
    return Path(cargo).resolve().parent
PLAN = {"requirements": [{"source": "paper/main.tex:2", "description": "Return the maximum, or None for empty input.",
                          "consequences": "Scan every item; ties preserve the value."}],
        "interpretations": [], "unspecified": [], "departures": []}
SOURCE = '''pub fn maximum(xs: &[u64]) -> Option<u64> { xs.iter().copied().max() }
#[cfg(test)] mod tests { use super::*; #[test] fn cases() {
assert_eq!(maximum(&[]), None); assert_eq!(maximum(&[0,9,2,9]), Some(9));
assert_eq!(maximum(&[u64::MAX]), Some(u64::MAX)); }}
'''


@pytest.fixture
def root(tmp_path):
    bundle = tmp_path / "original"
    bundle.mkdir()
    (bundle / "main.tex").write_text("\\begin{theorem}\\label{thm:max}\nA scan returns a maximum, or None for an empty list.\n\\end{theorem}\n")
    contract = tmp_path / "contract.md"
    contract.write_text("Implement the paper's scan on finite lists of unsigned 64-bit integers.\n")
    paper = paper_input(bundle, "main.tex", [{"id": "thm:max", "source_path": "main.tex", "start_line": 1,
                        "end_line": 3, "selected_text": (bundle / "main.tex").read_text().strip(), "excluded_clauses": []}],
                        route="rust_aeneas", entry_points=[{"node_id": "maximum", "declaration": "maximum",
                        "source_path": "crate/src/lib.rs"}], implementation_roots={"thm:max": ["maximum"]})
    root = tmp_path / "preparation"
    initialize_rust_preparation(root, paper_bundle=bundle, paper=paper, tools={"cargo": str(rust_bin() / "cargo")},
                                model="gpt-6-astra", effort="xhigh", engineering_contract=contract)
    return root


def install_source(root):
    (root / "work/crate/src/lib.rs").write_text(SOURCE)
    (root / "work/crate/Cargo.lock").write_text('version = 4\n\n[[package]]\nname = "paper_algorithm"\nversion = "0.1.0"\n')


def kernel_result(purpose, raw):
    kind = "performance" if purpose == "performance" else "audit" if purpose.endswith("review") else "worker"
    result = run_kernel_cli({"action": f"validate_preparation_{kind}_result", "raw_payload": raw})["output"]
    assert result["ok"], result
    return result["data"]


class FixtureAgent:
    """Only local deterministic fixture authoring; no production transport claim."""
    def __init__(self):
        self.deliveries = []

    def __call__(self, root, state, task):
        purpose = task.get("purpose", "performance")
        view = root.parent / f"delivery-{state['sequence']}"
        self.deliveries.append((purpose, eng.delivery(root, state, task, view), view))
        if purpose == "source_reading":
            write_json(root / "engineering/reading.json", PLAN)
        elif purpose == "plan":
            write_json(root / "engineering/plan.json", PLAN)
        elif purpose == "implementation":
            install_source(root)
        raw = {"decision": "accept", "reason": "Paper, contract, deviations and correctness checked."} if purpose.endswith("review") else {"summary": "Completed fixture task."}
        return kernel_result(purpose, raw)


def review(root, monkeypatch, *, real=False):
    monkeypatch.setattr(driver, "commit_checkpoint", lambda *args: None)
    if not real:
        def execute(argv, *, cwd, env, stdout, stderr, timeout):
            stdout.write_text("fixture output\n")
            stderr.write_text("")
            return {"exit_code": 0, "timeout": False}
        monkeypatch.setattr(perf, "_run_bounded", execute)
    agent = FixtureAgent()
    instance = driver.Driver(root, agents=agent)
    for _ in range(5):
        instance.step()
    assert instance.state["next"] == {"role": "performance"}
    return instance, agent


def measure(root, state, *, commands=None):
    raw = {"action": "measure", "reason": "Representative fixture measurement.", "commands": commands or [
        {"name": "workload", "argv": ["/usr/bin/true"], "kind": "run", "cwd": "."}]}
    loop.apply(root, state, kernel_result("performance", raw))
    return state["performance"]["runs"][-1]


def accept(root, state, run):
    (root / "benchmarks/report.md").write_text("Fixture selection, coverage, uncertainty and stopping judgment.\n")
    loop.apply(root, state, kernel_result("performance", {"action": "accept", "reason": "Keep the best correct measured candidate.",
               "evidence": [run], "report": "benchmarks/report.md"}))


def test_driver_engineering_baseline_handoff_and_closed_provider(root, monkeypatch):
    instance, agent = review(root, monkeypatch)
    state = instance.state
    assert [x[0] for x in agent.deliveries] == ["source_reading", "plan", "plan_review", "implementation", "code_review"]
    assert set(state["audits"]) == {"engineering"}
    run = measure(root, state)
    accept(root, state, run)
    assert state["status"] == "engineering_ready"
    instance.save()
    from trellis.modeling.native import prepare
    with pytest.raises(ValueError, match="explicit formal handoff"):
        prepare(root, root.parent / "unauthorized-native", root.parent / "unauthorized-runtime")
    assert not (root.parent / "unauthorized-native").exists()
    assert not {"human_approved", "assumptions", "target_claims"} & state.keys()
    sealed = (root / eng.STORE / "baseline.json").read_bytes()
    eng.handoff(root, state)
    assert state["stage"] == "setup" and state["status"] == "active" and state["audits"] == {}
    assert (root / "upstream/src/lib.rs").read_text() == SOURCE
    assert (root / eng.STORE / "baseline.json").read_bytes() == sealed
    with pytest.raises((ValueError, FileNotFoundError), match="ENGINEERING_ADAPTATION.md|Missing"):
        eng.validate_handoff(root, state)


def test_production_delivery_blocks_before_wrapper_or_formal_checks(root, monkeypatch):
    from dataclasses import replace
    config = driver.load_config(root / "trellis.config.json")
    monkeypatch.setattr(driver, "load_config", lambda *_: replace(config, sandbox=replace(config.sandbox, enabled=False)))
    monkeypatch.setattr(driver, "execute_agent_request", lambda *args: pytest.fail("provider dispatched"))
    monkeypatch.setattr(driver, "observe", lambda *args: pytest.fail("formal check executed"))
    state = read_json(root / ".trellis/modeling/state.json")
    with pytest.raises(ValueError, match="fresh request owner"):
        driver.Agents()(root, state, state["next"])
    with pytest.raises(ValueError, match="requires bwrap"):
        driver.Driver(root).step()
    with pytest.raises(ValueError, match="closed-file delivery"):
        driver.agent_prompt(root, state, state["next"], None)
    with pytest.raises(ValueError, match="closed-file delivery"):
        loop.prompt(root, state, {"role": "performance"}, None)


def test_rendered_delivery_excludes_host_and_candidate_from_independent_read(root, monkeypatch):
    (root / "FAITHFULNESS.md").write_text("HOST_SENTINEL")
    (root / "external-diagnostics.txt").write_text("HOST_SENTINEL")
    instance, agent = review(root, monkeypatch)
    for purpose, delivery, view in agent.deliveries:
        assert delivery["fresh_context"] is True
        names = set(delivery["files"])
        assert not any(".trellis" in name or name in {"FAITHFULNESS.md", "config.json", "paper-targets.json"} for name in names)
        prompt = (view / "prompt.md").read_text().lower()
        assert not any(word in prompt for word in ("aeneas", "lean", "formal", "verification", "phase 2", "trellis"))
        assert not any("HOST_SENTINEL" in (view / name).read_text() for name in names)
        if purpose == "source_reading":
            assert not any(name.startswith("source/") or name in {"plan.json", "reading.json"} for name in names)
    assert instance.state["status"] == "active"


def test_unapproved_new_departure_restores_source_and_returns_to_planning(root, monkeypatch):
    monkeypatch.setattr(driver, "commit_checkpoint", lambda *args: None)
    agent = FixtureAgent()
    instance = driver.Driver(root, agents=agent)
    for _ in range(3):
        instance.step()
    before = (root / "work/crate/src/lib.rs").read_bytes()
    def deviating(root, state, task):
        install_source(root)
        plan = copy.deepcopy(PLAN)
        plan["departures"] = [{"source": "paper/main.tex:2", "description": "Sort first.", "consequences": "Extra storage and work."}]
        write_json(root / "engineering/plan.json", plan)
        return {"summary": "Discovered a departure."}
    instance.agents = deviating
    instance.step()
    assert instance.state["next"]["purpose"] == "plan"
    assert (root / "work/crate/src/lib.rs").read_bytes() == before
    assert (root / ".trellis/modeling/proposals/4/crate/src/lib.rs").read_text() == SOURCE
    assert "engineering" not in instance.state["audits"]


def test_rejected_plan_and_absent_verdict_do_not_advance(root, monkeypatch):
    instance = driver.Driver(root, agents=FixtureAgent())
    monkeypatch.setattr(driver, "commit_checkpoint", lambda *args: None)
    instance.step(); instance.step()
    instance.agents = lambda *args: {"decision": "revise", "reason": "Unresolved departure."}
    instance.step()
    assert instance.state["next"]["purpose"] == "plan"
    assert "plan_approval" not in instance.state["engineering"]
    instance.agents = FixtureAgent()
    instance.step()
    instance.agents = lambda *args: {}
    instance.step()
    assert instance.state["next"]["purpose"] == "plan_review"
    assert "plan_approval" not in instance.state["engineering"]


def test_prior_best_candidate_selection_and_drift(root, monkeypatch):
    instance, _ = review(root, monkeypatch)
    state = instance.state
    first = measure(root, state)
    before = (root / "work/crate/src/lib.rs").read_bytes()
    loop.apply(root, state, {"action": "improve", "reason": "Investigate an allocation.", "instruction": "Try a different loop."})
    assert state["next"]["purpose"] == "plan"
    for _ in range(4):
        instance.step()
    # A distinct reviewed generation; the fixture's algorithm is unchanged.
    (root / "work/crate/src/lib.rs").write_text(SOURCE + "// later candidate\n")
    eng.apply(root, state, {"purpose": "code_review"}, {"decision": "accept", "reason": "Reviewed the later generation."})
    measure(root, state)
    accept(root, state, first)
    assert (root / "work/crate/src/lib.rs").read_bytes() == before
    assert state["performance"]["approval"]["evidence"] == [first]
    (root / "work/crate/Cargo.lock").write_text("changed")
    with pytest.raises(ValueError, match="Current code"):
        eng.handoff(root, state)


@pytest.mark.parametrize("changed", ["contract", "paper", "plan", "source", "lock", "configuration"])
def test_changes_block_current_acceptance(root, monkeypatch, changed):
    instance, _ = review(root, monkeypatch)
    run = measure(root, instance.state)
    paths = {"contract": root / eng.STORE / "contract.md", "paper": root / "paper/main.tex",
             "plan": root / "engineering/plan.json", "source": root / "work/crate/src/lib.rs",
             "lock": root / "work/crate/Cargo.lock", "configuration": root / "trellis.config.json"}
    path = paths[changed]
    if changed == "plan":
        plan = copy.deepcopy(PLAN)
        plan["unspecified"] = list(plan["requirements"])
        write_json(path, plan)
    else:
        path.write_text(path.read_text() + "\n ")
    with pytest.raises(ValueError):
        accept(root, instance.state, run)
    assert instance.state["status"] != "engineering_ready"


def test_failed_cargo_cannot_receive_readiness_approval(root, monkeypatch):
    instance, _ = review(root, monkeypatch)
    (root / "work/crate/src/lib.rs").write_text("not valid rust")
    def fail(argv, *, cwd, env, stdout, stderr, timeout):
        stdout.write_text(""); stderr.write_text("compile failed\n")
        return {"exit_code": 1, "timeout": False}
    monkeypatch.setattr(perf, "_run_bounded", fail)
    with pytest.raises(ValueError, match="Cargo"):
        eng.apply(root, instance.state, {"purpose": "code_review"}, {"decision": "accept", "reason": "Attempted approval."})
    with pytest.raises(ValueError, match="Current code"):
        eng.current_approval(root, instance.state)


def test_seed_and_reference_intake_are_rejected_before_creation(root, tmp_path):
    config = read_json(root / "config.json")
    for extra in ({"source": root / "work/crate"}, {"paper": {**config["paper_input"], "paper_files": ["paper/TRELLIS_REFERENCES.json"]}}):
        args = dict(paper_bundle=root / "paper", paper=config["paper_input"], tools=config["tools"],
                    model="gpt-6-astra", effort="xhigh", engineering_contract=root / eng.STORE / "contract.md")
        args.update(extra)
        with pytest.raises(ValueError, match="fresh source scaffold"):
            initialize_rust_preparation(tmp_path / "rejected", **args)
        assert not (tmp_path / "rejected").exists()


def test_performance_delivery_contains_own_evidence_without_host_resources(root, monkeypatch):
    instance, _ = review(root, monkeypatch)
    run = measure(root, instance.state)
    destination = root.parent / "performance-view"
    manifest = eng.delivery(root, instance.state, {"role": "performance"}, destination)
    record = read_json(destination / "measurements" / run / "record.json")
    assert record["candidate"] and record["source_files"]["Cargo.lock"]
    assert "environment" not in record
    assert all(set(item.get("executable", {})) <= {"sha256"} for item in record["commands"])
    assert not any("basis.json" in name or name.startswith(".trellis/") for name in manifest["files"])
    text = (destination / "prompt.md").read_text().lower()
    assert "sampling" in text and "best correct measured candidate" in text
    assert all(word not in text for word in ("aeneas", "lean", "verification", "phase 2"))


def test_tampered_baseline_evidence_blocks_adaptation(root, monkeypatch):
    instance, _ = review(root, monkeypatch)
    run = measure(root, instance.state)
    accept(root, instance.state, run)
    eng.handoff(root, instance.state)
    evidence = root / perf.STORE / "runs" / run / "record.json"
    evidence.write_text(evidence.read_text() + "\n")
    with pytest.raises(ValueError, match="Baseline measurement receipt"):
        eng.validate_handoff(root, instance.state)


@pytest.mark.skipif(os.environ.get("TRELLIS_TEST_PERFORMANCE_EXECUTION") != "1", reason="Opt-in installed Cargo/bwrap test")
def test_actual_cargo_measurement_lock_freshness_and_handoff_checks(root, monkeypatch):
    instance, agent = review(root, monkeypatch, real=True)
    state = instance.state
    check = state["observation"]
    assert eng._check_passed(check), check
    (root / "benchmarks/harness/main.rs").write_text(
        '#[path="/implementation/src/lib.rs"] mod algorithm;\n'
        'fn main(){let mut sum=0u64;for n in 0..100000 {sum=sum.wrapping_add(std::hint::black_box('
        'algorithm::maximum(&[n,0,n/2]).unwrap()));}println!("iterations=100000 checksum={sum}");}\n')
    commands = [
        {"name": "compile", "argv": ["rustc", "-O", "/harness/main.rs", "-o", "/build/bench"], "kind": "build", "cwd": "build"},
        {"name": "workload", "argv": ["/build/bench"], "kind": "run", "cwd": "build"}]
    run = measure(root, state, commands=commands)
    record = perf.validate_evidence(root, state, [run])[0]
    assert record["commands"][-1]["retained_entrypoint"]
    assert "iterations=100000" in (root / Path(record["record_path"]).parent / record["commands"][-1]["stdout"]).read_text()
    accept(root, state, run)
    eng.handoff(root, state)
    assert any(Path(eng.ADAPTATION_NOTE).is_relative_to(Path(path)) for path in driver.worker_writable_paths("setup"))
    (root / eng.ADAPTATION_NOTE).write_text("Unchanged implementation; repeat the baseline workload and regressions.\n")
    # Existing formal audit remains an independent prerequisite; fixture only
    # supplies it to exercise comparison accounting, never to export a model.
    state["stage"] = "adaptation"
    state["audits"]["adaptation"] = {"decision": "accept"}
    monkeypatch.setattr("trellis.modeling.paper_choices.validate_audit", lambda *args: None)
    loop.begin(root, state)
    measure(root, state, commands=commands)
    accept(root, state, state["performance"]["runs"][-1])
    eng.validate_handoff(root, state)
    files = eng.export_evidence(root, state, root.parent / "export")
    assert "paper-implementation/engineering/baseline.json" in files
    cargo = root / "work/crate/Cargo.toml"
    cargo.write_text(cargo.read_text().replace('"0.1.0"', '"0.2.0"'))
    stale = eng.check(root, state)
    assert not eng._check_passed(stale)
    assert stale["commands"][0]["exit_code"] != 0
    with pytest.raises(ValueError, match="Cargo"):
        eng.validate_handoff(root, state)
