"""Agent-owned empirical preparation review, using host-owned observations."""
from __future__ import annotations

import hashlib
from pathlib import Path
import shutil

from trellis.modeling import performance
from trellis.modeling.observations import read_json, write_json
from trellis.paper_implementation import canonical_json, evidence_path, relative_path, sha256

ACCEPTED = performance.STORE / "accepted.json"


def _audit(state: dict) -> dict:
    name = state["stage"] if state["stage"] in {"lean_implementation", "engineering"} else "adaptation"
    audit = state.get("audits", {}).get(name, {})
    if audit.get("decision") != "accept":
        raise ValueError("Performance review requires current implementation audit approval")
    return audit


def _audit_hash(state: dict) -> str:
    return hashlib.sha256(canonical_json(_audit(state))).hexdigest()


def begin(root: Path, state: dict) -> None:
    previous = state.get("performance", {})
    state["performance"] = {"basis": performance.current_basis(root), "audit_sha256": _audit_hash(state),
                            "runs": previous.get("runs", []), "history": previous.get("history", [])}
    (root / ACCEPTED).unlink(missing_ok=True)
    (root / "benchmarks/harness").mkdir(parents=True, exist_ok=True)
    state.update(status="active", next={"role": "performance"})


def reopen(root: Path, state: dict, reason: str) -> None:
    review = state.setdefault("performance", {})
    if "approval" in review:
        review.setdefault("history", []).append(review.pop("approval"))
    (root / ACCEPTED).unlink(missing_ok=True)
    if state["stage"] == "engineering":
        from trellis.modeling.engineering import reopen as reopen_engineering
        reopen_engineering(root, state, reason)
        return
    stage = "lean_implementation" if state["stage"] == "lean_implementation" else "adaptation"
    state.get("audits", {}).pop(stage, None)
    state.update(stage=stage, status="active", feedback=reason, next={"role": "reviewer", "instruction": reason})


def _current(root: Path, state: dict) -> None:
    if state["stage"] == "engineering":
        from trellis.modeling.engineering import current_approval
        current_approval(root, state)
    elif read_json(root / "config.json").get("paper_input", {}).get("route") == "rust_aeneas":
        from trellis.modeling import paper_choices
        paper_choices.validate_audit(root, state)
    review = state.get("performance", {})
    if review.get("basis") != performance.current_basis(root) or review.get("audit_sha256") != _audit_hash(state):
        raise ValueError("Implementation, build, tools or algorithm approval changed; renew implementation review")


def _report_digest(root: Path, name: str) -> str:
    path = evidence_path(root, name)
    if path.stat().st_size > 1024 * 1024 or not path.read_text(encoding="utf-8").strip():
        raise ValueError("Performance assessment must be nonempty UTF-8 text of at most 1MiB")
    return sha256(path)


def validate_acceptance(root: Path, state: dict) -> tuple[dict, list[dict]]:
    _current(root, state)
    approval = state["performance"].get("approval")
    if not approval or read_json(evidence_path(root, ACCEPTED.as_posix())) != approval:
        raise ValueError("Export requires host-recorded performance acceptance")
    if approval.get("report_sha256") != _report_digest(root, approval["report"]):
        raise ValueError("Accepted performance report changed")
    records = performance.validate_evidence(root, state, approval["evidence"])
    if (approval.get("basis") != state["performance"]["basis"]
            or approval.get("audit_sha256") != _audit_hash(state)
            or approval.get("harness_sha256") != performance.harness_fingerprint(root)):
        raise ValueError("Performance acceptance is stale")
    return approval, records


def apply(root: Path, state: dict, raw: dict) -> None:
    _current(root, state)
    action, reason = raw.get("action"), raw.get("reason")
    if not isinstance(reason, str) or not reason.strip():
        raise ValueError("Performance decisions require a reason")
    if action == "accept":
        report = relative_path(raw.get("report"))
        if not report.startswith("benchmarks/"):
            raise ValueError("Performance report must be under benchmarks/")
        _report_digest(root, report)
        if state["stage"] == "engineering":
            from trellis.modeling.engineering import select
            with select(root, state, raw.get("evidence")):
                _accept(root, state, raw, report)
        else:
            _accept(root, state, raw, report)
        return
    state.update(feedback="", next={"role": "performance"})
    if action == "measure":
        record = performance.run_measurement(root, state, raw)
        state["performance"]["runs"].append(record["id"])
        state["performance"]["latest_measurement"] = record
    elif action == "improve":
        instruction = raw.get("instruction")
        if not isinstance(instruction, str) or not instruction.strip():
            raise ValueError("Performance improvements require an implementation instruction")
        reopen(root, state, f"Performance review: {reason}\n{instruction}")
    elif action == "fail":
        state.update(status="failed", failure=reason)
    else:
        raise ValueError("Unsupported performance action")
    if action == "measure" and state["stage"] == "engineering":
        from trellis.modeling.engineering import retain
        retain(root, state, record)
    if state["stage"] == "engineering":
        state["engineering"].pop("retry_feedback", None)


def acceptance_record(root: Path, state: dict, raw: dict, report: str) -> dict:
    reason = raw["reason"]
    records = performance.validate_evidence(root, state, raw.get("evidence"))
    if any(record["id"] not in state["performance"]["runs"] for record in records):
        raise ValueError("Acceptance must cite this preparation's recorded measurements")
    return {"schema": "trellis-paper-performance-review/v1", "decision": "accept", "reason": reason,
                "basis": state["performance"]["basis"], "audit_sha256": _audit_hash(state),
                "harness_sha256": performance.harness_fingerprint(root), "evidence": raw["evidence"],
                "report": report, "report_sha256": _report_digest(root, report)}


def _accept(root: Path, state: dict, raw: dict, report: str) -> None:
    approval = acceptance_record(root, state, raw, report)
    write_json(root / ACCEPTED, approval)
    state.update(feedback="", next={"role": "performance"})
    state["performance"]["approval"] = approval
    state["status"] = "prepared"
    if state["stage"] == "engineering":
        from trellis.modeling.engineering import freeze
        freeze(root, state)
        state["engineering"].pop("retry_feedback", None)
    if state["stage"] == "adaptation":
        state["stage"] = "modeling"


def step(root: Path, state: dict, agents) -> dict:
    """Skip source check reruns for harness-only work; preserve the audited basis."""
    from trellis.modeling.driver import check_halt, runtime_root
    from trellis.agent_wrapper.executor import record_host_failure
    from trellis.agent_wrapper.protocol import EngineeringExecutionFailure
    from trellis.runtime.bridge import any_halt_marker_present
    from trellis.checker.memory_retry import ResourceOutcome
    import traceback
    check_halt(root)
    task = state["next"]
    pending = state.get('pending_performance_response')
    if pending and (pending['basis'] != performance.current_basis(root)
                    or pending['harness'] != performance.harness_fingerprint(root)):
        state.pop('pending_performance_response', None)
        pending = None
    if not pending:
        state["sequence"] += 1
    path = root / ".trellis/modeling/state.json"
    write_json(path, state)
    try:
        result = _dispatch(root, state, task, agents)
        if state.get("status") == "engineering_ready":
            from trellis.modeling.engineering import checkpoint
            from trellis.modeling.engineering_transport import publish_outcome
            state.pop("last_error", None)
            publish_outcome(root, state)
            checkpoint(root, state)
    except EngineeringExecutionFailure:
        raise  # The owning Driver records one halt using preparation configuration.
    except ResourceOutcome:
        if 'pending_performance_response' not in state:
            state['sequence'] -= 1
        write_json(path, state)
        raise
    except BaseException as error:
        if state["stage"] == "engineering":
            raise
        state["last_error"] = str(error) or type(error).__name__
        if isinstance(error, Exception) and not any_halt_marker_present(runtime_root(root)):
            evidence = root / ".trellis/modeling/requests" / f"{state['sequence']:05d}-host-error.json"
            write_json(evidence, {"error": str(error), "traceback": traceback.format_exc(), "task": task})
            state.update(resume_status=state["status"], status="halted",
                         host_failure={"operation": "performance_step", "error": str(error), "evidence": str(evidence)})
            write_json(path, state)
            record_host_failure(root, runtime_root(root), operation="performance_step", cycle=state["sequence"],
                                evidence=evidence, error=str(error))
        elif any_halt_marker_present(runtime_root(root)) and state["status"] != "halted":
            state.update(resume_status=state["status"], status="halted")
        write_json(path, state)
        raise
    state.pop("last_error", None)
    state.pop('pending_performance_response', None)
    state.pop('capacity_wait', None)
    if state["stage"] == "engineering":
        from trellis.modeling.engineering_transport import publish_outcome
        publish_outcome(root, state)
    else:
        write_json(path, state)
    return result


def _dispatch(root: Path, state: dict, task: dict, agents) -> dict:
    from trellis.modeling.driver import AgentResponseError
    from trellis.checker.memory_retry import ResourceOutcome
    raw = {}
    try:
        _current(root, state)
    except ValueError as error:
        reason = "The source or approval evidence changed. Renew review against the current inputs." if state["stage"] == "engineering" else str(error)
        reopen(root, state, reason)
    else:
        try:
            pending = state.get('pending_performance_response')
            raw = pending['response'] if pending else agents(root, state, task)
            _current(root, state)
            state['pending_performance_response'] = dict(response=raw,
                basis=performance.current_basis(root), harness=performance.harness_fingerprint(root))
            apply(root, state, raw)
        except ResourceOutcome:
            raise
        except (ValueError, KeyError, TypeError, AgentResponseError) as error:
            try:
                _current(root, state)
            except ValueError:
                reason = "The source or approval evidence changed. Renew review against the current inputs." if state["stage"] == "engineering" else str(error)
                reopen(root, state, reason)
            else:
                state.update(feedback=str(error), next={"role": "performance"})
            if state["stage"] == "engineering":
                from trellis.modeling.engineering import retry_feedback
                retry_feedback(state, task, raw)
    state["history"].append({"sequence": state["sequence"], "stage": state["stage"],
                             "task": task, "response": raw, "feedback": state.get("feedback", "")})
    return state


def export_evidence(root: Path, state: dict, destination: Path) -> list[str]:
    approval, records = validate_acceptance(root, state)
    base = Path("paper-implementation/performance")
    files = []

    def copy(source: Path, name: Path, expected: str) -> None:
        target = destination / base / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)
        if sha256(target) != expected:
            raise ValueError("Performance evidence changed during export")
        files.append((base / name).as_posix())

    copy(evidence_path(root, ACCEPTED.as_posix()), Path("review.json"), sha256(root / ACCEPTED))
    copy(evidence_path(root, approval["report"]), Path("report.md"), approval["report_sha256"])
    for record in records:
        path = evidence_path(root, record["record_path"])
        prefix = Path("runs") / record["id"]
        copy(path, prefix / "record.json", record["record_sha256"])
        for name, digest in record["evidence_files"].items():
            copy(evidence_path(path.parent, name), prefix / name, digest)
    validate_acceptance(root, state)
    return files


def prompt(root: Path, state: dict, task: dict, artifacts) -> str:
    if state["stage"] == "engineering":
        raise ValueError("Engineering performance uses the closed-file delivery contract")
    from trellis.modeling.driver import REPO
    context = root / ".trellis/modeling/requests" / f"{state['sequence']:05d}-performance.json"
    write_json(context, {"task": task, "performance": state.get("performance", {}),
                         "feedback": state.get("feedback", ""), "audits": state.get("audits", {})})
    text = (REPO / "trellis/prompt_fragments/paper_implementation/preparation_performance.md").read_text()
    references = (REPO / "trellis/prompt_fragments/paper_implementation/references.md").read_text()
    return (text + "\n" + references + f"\nRead paper-targets.json, the original paper, work/, and {context}.\n"
            + f"Write response JSON to {artifacts.raw}, validate with python3 {root / '.trellis/scripts/check.py'} "
              f"preparation-performance-result {artifacts.raw}, then create {artifacts.done}.\n")
