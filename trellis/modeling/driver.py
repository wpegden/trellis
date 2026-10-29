"""Audit-gated setup and adaptation before the native Modeling runtime."""
from __future__ import annotations

import json
import copy
import hashlib
import os
from pathlib import Path
import shlex
import shutil
import time
from contextlib import nullcontext

from trellis.agent_wrapper.executor import execute_agent_request, prepare_artifact_paths, record_host_failure
from trellis.agent_wrapper.protocol import AgentLane, ArtifactSpec, EngineeringExecutionFailure
from dataclasses import replace

from trellis.config import load_config, _parse_provider_config
from trellis.checking import write_scripts
from trellis.runtime.bridge import _single_request_common, _bridge_state_dir, any_halt_marker_present
from trellis.runtime.git_checkpoint_hook import commit_checkpoint
from trellis.modeling.environment import prepare_environment, tool_readonly_paths
from trellis.modeling.observations import fingerprint, observe, passed, read_json, write_json
from trellis.runtime.process_description import pv_process_description
from trellis.pv_toolchain import bootstrap_configuration_errors, effective_tools, setup_basis
from scripts.extract_pv_model import copy_source_inputs

REPO = Path(__file__).resolve().parents[2]
FRAGMENTS = REPO / "trellis/prompt_fragments/pv/modeling"
STAGES = ("setup", "adaptation", "modeling")
STAGE_CHECKS = {"setup": "cargo", "adaptation": "lean"}
PREPARATION_PROMPT_CHAR_BUDGET = 30000


def reopen_setup(state: dict) -> None:
    state.update(stage="setup", audits={})
    state.pop("export_check", None)


class AgentResponseError(ValueError):
    """A burst failed to return a validated response."""


def runtime_root(root: Path) -> Path:
    return root.with_name(root.name + "-runtime")


def worker_writable_paths(stage: str) -> tuple[str, ...]:
    if stage == "lean_implementation":
        return ("work/lean/Tablet",)
    return ("work",) if stage == "setup" else ("work/crate",)


def check_halt(root: Path) -> None:
    if any_halt_marker_present(runtime_root(root)):
        raise RuntimeError(f"Preparation halted for operator review: {runtime_root(root)}")
    from trellis.modeling.engineering_transport import check_unresolved_attempts
    check_unresolved_attempts(root)


def setup_fingerprint(root: Path, *, resource_identity: str | None = None) -> str:
    # The adaptation audit covers the whole candidate crate, including its
    # packaging. Setup owns the extraction environment and invocation.
    return setup_basis(root, resource_identity=resource_identity)


def source_fingerprint(root: Path, *, setup: str | None = None) -> str:
    return fingerprint(root / "work" / "crate") + (setup if setup is not None else setup_fingerprint(root))


def initialize(root: Path, source: Path, goal_file: Path | None, tools: dict, *, model: str, effort: str,
               config_template: Path | None = None, paper_input: dict | None = None,
               paper_bundle: Path | None = None) -> None:
    if root.exists():
        raise ValueError(f"Run directory already exists: {root}")
    from trellis.modeling.goals import read_goal_collection
    if paper_input is not None:
        if goal_file is not None or paper_bundle is None:
            raise ValueError("Paper preparation takes original paper evidence, not prose goals")
        goals, context, goal_raw = [], "", ""
    else:
        goal_raw, collection = read_goal_collection(goal_file)
        goals = collection["goals"]
        context = collection.get("context", "")
    root.mkdir(parents=True)
    from trellis.modeling.history import initialize_history
    initialize_history(root)
    copy_source_inputs(source, root / "upstream")
    copy_source_inputs(root / "upstream", root / "work/crate")
    if paper_input is not None:
        shutil.copytree(paper_bundle, root / "paper")
        write_json(root / "paper-targets.json", paper_input)
    else:
        (root / "goals.json").write_text(goal_raw)
        (root / "GOAL.md").write_text(context + "\n\n" + "\n\n".join(f"## {g['id']}\n\n{g['text']}" for g in goals) + "\n")
    shutil.copy2(REPO / "trellis/prompt_fragments/canonical/FAITHFULNESS.md", root / "FAITHFULNESS.md")
    (root / "work/SETUP.md").write_text("")
    write_json(root / "work/extraction.json", {"crate": "crate", "cargo_args": [], "charon_args": [], "aeneas_args": []})
    inputs = {"model": model, "effort": effort, "tools": tools}
    inputs.update({"paper_input": paper_input} if paper_input is not None else {"goals": goals})
    write_json(root / "config.json", inputs)
    state = {"stage": "setup", "status": "active", "sequence": 0,
             "next": {"role": "worker", "instruction": "Complete setup under the stage criteria using the supplied source, goals, and tools."},
             "audits": {}, "verification_protocol": "native-v1",
             "declined": {}, "history": [], "observation": {}, "feedback": ""}
    write_json(root / ".trellis/modeling/state.json", state)
    agent_config = read_json(config_template or REPO / "examples/trellis.config.json")
    if config_template is not None:
        inputs = read_json(root / "config.json")
        inputs["launch_config"] = agent_config
        for role, provider in agent_config.get("preparation", {}).items():
            _parse_provider_config(provider, f"preparation.{role}")
        write_json(root / "config.json", inputs)
    def bind(value):
        if isinstance(value, dict):
            if "provider" in value:
                value.update(provider="codex", model=model, effort=effort, label=model)
            for item in value.values():
                bind(item)
        elif isinstance(value, list):
            for item in value:
                bind(item)
    if config_template is None:
        bind(agent_config)
    from trellis.paper_implementation import preparation_launch_config
    if paper_input is not None:
        from trellis.paper_implementation import preparation_paper_digest
        preparation_paper_digest(root)
    write_json(root / "trellis.config.json", preparation_launch_config(root, agent_config, paper_input))
    prepare_environment(root, tools)
    write_scripts(root, root / ".trellis")


def agent_prompt(root: Path, state: dict, task: dict, artifacts) -> str:
    if state["stage"] == "engineering":
        raise ValueError("Engineering prompts require the closed-file delivery contract")
    from trellis.agents.codex_headless import BASE_INSTRUCTIONS_DIR
    from trellis.modeling.inspection import ContextPreviewBudgetError, context_preview, role_context
    from trellis.modeling import routes
    config = read_json(root / "config.json")
    if task["role"] == "performance":
        from trellis.modeling.performance_loop import prompt
        return prompt(root, state, task, artifacts)
    if state["stage"] == "lean_implementation":
        from trellis.paper_implementation import lean_preparation_prompt
        return lean_preparation_prompt(root, state, task, artifacts)
    from trellis.paper_implementation import preparation_targets, preparation_referent
    reference = root / ".trellis/modeling/process.md"
    if "paper_input" in config:
        process = (REPO / "trellis/prompt_fragments/paper_implementation/process.md").read_text()
        authority = (REPO / "trellis/prompt_fragments/paper_implementation/authority.md").read_text()
        references = (REPO / "trellis/prompt_fragments/paper_implementation/references.md").read_text()
        prompt = process + "\n" + authority + "\n" + references
        reference.write_text(prompt)
        prompt += "\nTarget evidence is in paper-targets.json; export creates the native subject and review package.\n"
    else:
        reference.write_text(pv_process_description("modeling", full_reference=True))
        prompt = pv_process_description("modeling")
    prompt += "\n\n" + (REPO / "trellis/prompt_fragments/canonical/FAITHFULNESS.md").read_text()
    prompt += f"\nYour assignment is source/extraction preparation. Full process reference: {reference}.\n" if "paper_input" in config else f"\nYour assignment is in Modeling and goal stating (Phase 0). Full process reference: {reference}.\n"
    prompt += "\n" + (FRAGMENTS / "files.md").read_text()
    assignment = {k: v for k, v in task.items() if k != "remaining"}
    if task["role"] == "audit" and "instruction" in assignment:
        assignment["reviewer_proposal"] = assignment.pop("instruction")
    context = {
        "task": assignment, "state": (routes.context(root, state, task["role"]) if task["role"] in routes.ROLES else role_context(state, task)),
        "stage_check": STAGE_CHECKS[state["stage"]],
        "worker_writable_paths": worker_writable_paths(state["stage"]),
        "target_ids" if "paper_input" in config else "goal_ids": [g["id"] for g in preparation_targets(config)]}
    if "paper_input" in config:
        context["paper_preparation"] = {key: config["paper_input"].get(key, [])
                                        for key in ("targets", "modeling_choices")}
        from trellis.modeling import paper_choices
        context["paper_preparation"]["discovery"] = paper_choices.context(root, state)
    reopening = root / ".trellis/modeling/reopened-from.json"
    if reopening.is_file():
        context["reopened_preparation"] = {"record": str(reopening),
            "reason": read_json(reopening)["disposition"]["reason"],
            "prior_work": "Read-only references; no prior artifact verification is inherited."}
    context_file = root / ".trellis/modeling/requests" / f"{state['sequence']:05d}-{task['role']}.json"
    write_json(context_file, context)
    prefix = prompt + "\nCurrent context:\n"
    prompt = f"\nFull role context: {context_file}. Long collections and text are truncated in this preview; read omitted task or goal content before acting.\n"
    prompt += f"\nRead {preparation_referent(root)} and relevant files in upstream/, work/, and the observation paths.\n"
    if task["role"] in {"reviewer", "audit"}:
        prompt += "Structured decisions: .history and .audits in .trellis/modeling/state.json. Transcript listing: " + shlex.join([
            "python3", ".trellis/scripts/check.py", "inspect-transcript",
            str(_bridge_state_dir(root, runtime_root(root)) / "logs")]) + "\n"
    prompt += "Check command: " + shlex.join(["python3", str(root / ".trellis/scripts/pv_modeling.py"), "check", str(root)]) + "\n"
    role = task["role"]
    if role != "worker":
        standard = (REPO / "trellis/prompt_fragments/paper_implementation/preparation_route_standard.md"
                    if "paper_input" in config else FRAGMENTS / "route_standard.md")
        prompt += "\n" + standard.read_text()
    fragment = {"planner": "native/route_planner.md", "critic": "native/route_critic.md", "route_audit": "native/route_audit.md"}.get(role, f"{role}.md")
    fragment_path = FRAGMENTS / fragment
    if "paper_input" in config:
        paper_fragment = REPO / "trellis/prompt_fragments/paper_implementation" / f"preparation_{role}.md"
        if paper_fragment.exists():
            fragment_path = paper_fragment
    role_text = fragment_path.read_text().replace("{{contract_json}}", "")
    if "paper_input" in config and role == "reviewer":
        guidance = ("During setup, assign source and extraction-configuration repairs with work; choose audit when the setup criteria and current cargo check pass."
                    if state["stage"] == "setup" else
                    "During adaptation, work may change the crate. Use reopen to request auditor-approved setup repairs while retaining the selected claims. Choose audit when extraction and the Lean/export diagnostics meet the adaptation criteria.")
        role_text = role_text.replace("{{stage_guidance}}", guidance)
    prompt += "\n" + role_text
    snapshot = os.environ.get("TRELLIS_REVIEWER_SOURCE_SNAPSHOT", "").strip()
    source_sha = os.environ.get("TRELLIS_REVIEWER_SOURCE_SHA", "").strip()
    if task["role"] in {"reviewer", "audit"} and snapshot and source_sha:
        from trellis.runtime.bridge_prompts import render_prompt_sections
        sections = render_prompt_sections(["preparation_source_recourse.md"],
                                          {"reviewer_source_snapshot_path": snapshot,
                                           "reviewer_source_sha": source_sha})
        prompt += "\n" + sections[0]["text"]
    contract = response_contract(task["role"], state["stage"])
    if "paper_input" in config and role == "route_audit":
        contract = ('Response: {"outcome":"modeling_accept", "scratchpad_replace":"assessment", '
                    '"new_tasks":[], "task_modifications":[], "modeling_resolution":{"action":"approve", '
                    '"scope":"implementation of all selected paper claims", "limitations":"implementation limitations consistent with those claims, or empty", '
                    '"tasks":[{"id":"task", "title":"implementation task", "body":"required work and source correspondence"}], '
                    '"declined_targets":[], "reopen_setup":false}}. Alternatively modeling_resolution.action is "fail" or "reopen".')
    prompt += "\n" + contract
    prompt += "\nAn optional system_feedback string reports a tooling or process defect requiring operator repair; unacknowledged feedback halts the run.\n"
    kind = ("trellis-audit-result" if role == "route_audit" else "trellis-stuck-math-audit-result") if role in routes.ROLES else f"preparation-{role if role != 'reviewer' else 'review'}-result"
    prompt += "\nValidate the response: " + shlex.join([
        "python3", str(root / ".trellis/scripts/check.py"), kind, str(artifacts.raw)]) + "\n"
    prompt += f"Write the response JSON to {artifacts.raw}, validate it, then create {artifacts.done} as your final filesystem change.\n"

    base_role = "stuck_math_audit" if role in {"planner", "critic"} else ("worker" if role == "worker" else "reviewer")
    base_chars = len((BASE_INSTRUCTIONS_DIR / f"{base_role}.md").read_text())
    context_budget = PREPARATION_PROMPT_CHAR_BUDGET - base_chars - len(prefix) - len(prompt)
    try:
        preview = context_preview(context, limit=min(16000, context_budget), preserve_keys=True)
    except ContextPreviewBudgetError:
        # Keys and truncation notices alone can exceed any fixed preview budget.
        # The complete context was saved above; use that existing evidence path
        # instead of dropping instructions, weakening the audit, or raising the cap.
        preview = ("Context preview omitted because it exceeds the prompt budget. "
                   "Read the full role context file below before acting.\n")
        if len(preview) > context_budget:
            raise ValueError("Preparation instructions and context reference exceed the prompt budget") from None
    result = prefix + preview + prompt
    if "paper_input" in config:
        result = result.replace("GOAL proof, disproof, and classification belong to argument development.",
                                "Proofs of selected paper conclusions belong to argument development.")
        result = result.replace("GOAL.md", config["paper_input"]["paper_path"]).replace("GOALs", "selected paper targets").replace("GOAL", "paper target")
    return result


def preparation_provider(config, inputs: dict, role: str):
    """Use the preparation role selected at launch, with CLI compatibility."""
    preparation = inputs.get("launch_config", {}).get("preparation", {})
    binding = "audit" if role in {"audit", "critic", "route_audit"} else (role if role in {"worker", "performance"} else "reviewer")
    if role in preparation:
        binding = role
    provider = (_parse_provider_config(preparation[binding], f"preparation.{binding}")
                if binding in preparation else (config.worker if role == "worker" else config.reviewer))
    return provider


class Agents:
    """Preparation contracts on the shared request builder and executor."""

    def __call__(self, root: Path, state: dict, task: dict) -> dict:
        check_halt(root)
        if state["stage"] == "engineering":
            from trellis.modeling.engineering_transport import dispatch
            return dispatch(root, state, task)
        config = load_config(root / "trellis.config.json")
        inputs = read_json(root / "config.json")
        role = task["role"]
        from trellis.modeling import routes
        route_role = role in routes.ROLES
        kind = "stuck_math_audit" if role in {"planner", "critic"} else ("audit" if role == "route_audit" else ("review" if role == "reviewer" else role))
        burst_role = "stuck_math_audit" if role in {"planner", "critic"} else ("worker" if role == "worker" else "reviewer")
        name = f"preparation-{state['sequence']:05d}-{role}"
        if role == "worker":
            paths = worker_writable_paths(state["stage"])
            config = replace(config, sandbox=replace(config.sandbox, extra_writable_paths=paths))
        elif role == "performance":
            config = replace(config, sandbox=replace(config.sandbox, extra_writable_paths=("benchmarks",)))
        provider = preparation_provider(config, inputs, role)
        request = _single_request_common(
            config=config, runtime_root=runtime_root(root),
            request={"kind": kind, "id": state["sequence"], "cycle": state["sequence"],
                     "phase": state["stage"], "fresh_context": role in {"audit", "planner", "critic", "route_audit", "performance"}},
            provider=provider,
            lane=AgentLane(kind="performance" if role == "performance" else burst_role),
            kind_label=role, burst_role=burst_role,
            prompt="", artifact=ArtifactSpec(canonical_name=name + ".json",
                                            kind=("trellis-audit-result" if role == "route_audit" else "trellis-stuck-math-audit-result") if route_role else f"preparation-{kind}-result", phase=state["stage"]),
        )
        artifacts = prepare_artifact_paths(request.state_dir, root, request.artifact.canonical_name)
        prompt = agent_prompt(root, state, task, artifacts)
        request = replace(request, prompt=prompt)
        log = root / ".trellis/modeling/requests" / name
        write_json(log.with_suffix(".request.json"), request.to_dict())
        log.with_suffix(".prompt.txt").write_text(prompt)
        from trellis.modeling.reopening import reference_paths
        env = {"TRELLIS_SANDBOX_EXTRA_RO_PATHS": os.pathsep.join([*tool_readonly_paths(inputs["tools"]), *reference_paths(root)]),
               "TRELLIS_KERNEL_CACHE_ROOT": str(runtime_root(root))}
        previous = {key: os.environ.get(key) for key in env}
        os.environ.update(env)
        try:
            response = execute_agent_request(request)
        finally:
            for key, value in previous.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value
        response_record = response.to_dict()
        response_record.pop("captured_output", None)
        response_record["log_dir"] = str(request.log_dir)
        write_json(log.with_suffix(".response.json"), response_record)
        check_halt(root)
        if not response.ok:
            raise AgentResponseError(f"{name}: {response.error}; logs: {request.log_dir}")
        return response.payload


def response_contract(role: str, stage: str) -> str:
    if role == "worker":
        return 'Response: {"summary": "changes and remaining preparation gaps", "modeling_review_request": "optional consequential scope/authoring tradeoff requiring review"}.'
    if role == "reviewer":
        actions = "work|audit|review_route|reopen|fail" if stage == "adaptation" else "work|audit|review_route|fail"
        return ('Response: {"action": "' + actions + '", '
                '"instruction": "task or justification"}. '
                'Setup and adaptation end with audit approval.')
    if role == "audit":
        return ('Response: {"decision": "accept|revise|review", "reason": "assessment under the phase completion criteria", "route_compliance": "not_needed|verified", '
                '"support_obligations": ["diagnosed gap in operation or observation meaning, with source evidence"]}. '
                'Omit support_obligations except when approving adaptation with diagnosed missing model support.')
    if role == "planner":
        return 'Response: {"report": "assessment (200–20000 characters)", "route_tex": "recommended route and strongest practical alternative"}.'
    if role == "critic":
        return 'Response: {"report": "independent assessment (200–20000 characters)", "gap_decision": "accept|reject", "gap_feedback": "concrete criticism"}.'
    if role == "route_audit":
        return 'Response: {"outcome":"modeling_accept", "scratchpad_replace":"assessment", "new_tasks":[], "task_modifications":[], "modeling_resolution":{"action":"approve", "scope":"exact retained program scope", "limitations":"coverage lost, or empty", "tasks":[{"id":"task", "title":"implementation task", "body":"required work"}], "declined_targets":[], "reopen_setup":false}}. Alternatively modeling_resolution.action is "fail" or "reopen". Reopen approves returning to setup with the candidate retained.'
    return 'Return the result described by the supplied artifact_contract.'


class Driver:
    def __init__(self, root: Path, agents=None, checker=None, *, terminal_scope=None):
        self.root = root.resolve()
        self.path = self.root / ".trellis/modeling/state.json"
        self.state = read_json(self.path)
        if self.state.get("verification_protocol") != "native-v1":
            raise ValueError("This preparatory run uses an older scheduler; initialize a fresh native run")
        self.config = read_json(self.root / "config.json")
        from trellis.modeling.engineering import validate_native_selection
        validate_native_selection(self.root, self.config)
        self.agents = agents or Agents()
        self.checker = checker or observe
        self.terminal_scope = terminal_scope or nullcontext

    def save(self) -> None:
        write_json(self.path, self.state)

    def reopen_setup(self) -> None:
        """Re-enter the existing audited preparation path on its retained candidate."""
        reopen_setup(self.state)

    def halt(self, operation: str, error: Exception, evidence: Path) -> None:
        self.state["resume_status"] = self.state["status"]
        self.state["status"] = "halted"
        self.state["host_failure"] = {"operation": operation, "error": str(error), "evidence": str(evidence)}
        self.save()
        record_host_failure(self.root, runtime_root(self.root), operation=operation,
                            cycle=self.state["sequence"], evidence=evidence, error=str(error))
        raise RuntimeError(f"Preparation halted during {operation}: {error}. Evidence: {evidence}") from error

    def validate_export(self) -> None:
        from trellis.modeling.native import validate_export, NativeExportCandidateError
        output = self.root / ".trellis/modeling/exports" / f"check-{time.time_ns()}"
        try:
            self.state["export_check"] = validate_export(self.state["observation"], output)
        except NativeExportCandidateError:
            self.state["export_check"] = read_json(output / "export.json")
            raise
        except (ValueError, OSError, SystemExit) as error:
            self.halt("export_validation", error, output / "export.json")

    def refresh(self) -> None:
        if self.state["stage"] == "engineering":
            from trellis.modeling.engineering import input_identity
            input_identity(self.root, self.state)
            return
        from trellis.pv_execution import execution_timeout
        from trellis.modeling.axioms import probe_identity

        basis = fingerprint(self.root / "work")
        setup = setup_fingerprint(self.root)
        previous = self.state["observation"]
        # A new budget can complete a timed-out check without invalidating a
        # successful check or either audit of the unchanged model.
        retry_timeout = False
        if any(step.get("timeout", step.get("execution", {}).get("timeout", False)) for step in previous.get("steps", [])):
            try:
                tools = effective_tools(self.root)
                budgets = {name: execution_timeout(tools, name) for name in ("cargo", "charon", "aeneas", "lean")}
                retry_timeout = previous.get("execution_timeouts") != budgets
            except (ValueError, KeyError, OSError):
                retry_timeout = True  # The check below reports malformed configuration.
        stale_probe = previous.get("preparation_checks") != probe_identity()
        if (self.state["observation"].get("work_fingerprint") != basis
                or self.state["observation"].get("setup_basis") != setup or retry_timeout or stale_probe):
            self.state.pop("export_check", None)
            output = self.root / "observations" / f"check-{time.time_ns()}"
            try:
                self.state["observation"] = self.checker(self.root, output)
            except (ValueError, KeyError) as error:
                self.state["observation"] = {"work_fingerprint": basis, "steps": [], "error": str(error)}
            self.state["observation"]["setup_basis"] = setup
            if "tool_identities" in self.state["observation"]:
                write_json(self.root / "tool-identities.json", self.state["observation"]["tool_identities"])

    def apply(self, task: dict, result: dict) -> None:
        if self.state["stage"] == "engineering":
            from trellis.modeling.engineering import apply
            apply(self.root, self.state, task, result)
            return
        state, role = self.state, task["role"]
        state["next"] = {"role": "reviewer"}
        state["feedback"] = ""
        from trellis.modeling import routes
        if role in routes.ROLES:
            routes.apply(self.root, state, role, result)
            return
        if role == "worker":
            if not isinstance(result.get("summary"), str):
                raise ValueError("Worker must supply a summary")
            if "modeling_choice_proposals" in result:
                from trellis.modeling import paper_choices
                paper_choices.propose(self.root, state, result["modeling_choice_proposals"])
            if result.get("modeling_review_request", "").strip():
                state["route_required"] = result["modeling_review_request"]
        elif role == "reviewer":
            action = result.get("action")
            instruction = result.get("instruction", "")
            if not isinstance(instruction, str) or not instruction.strip():
                raise ValueError("Reviewer must supply an instruction or justification")
            if action in {"review_route", "fail"}:
                routes.start(self.root, state, instruction)
                return
            if state.get("route_required"):
                raise ValueError("The flagged scope/authoring tradeoff requires review_route before further authoring or acceptance")
            if action == "work":
                state["next"] = {"role": "worker", "instruction": instruction}
            elif action == "audit":
                if state["stage"] == "modeling":
                    raise ValueError("Model completion uses verifier lanes; finish after they pass")
                state["next"] = {"role": "audit", "kind": "advance", "stage": state["stage"], "instruction": instruction}
            elif action == "reopen":
                if state["stage"] != "adaptation":
                    raise ValueError("Only adaptation can request reopening accepted setup")
                state["next"] = {"role": "audit", "kind": "reopen", "instruction": instruction}
            else:
                raise ValueError("Unknown reviewer action")
        elif role == "audit":
            if result.get("decision") == "review":
                routes.start(self.root, state, result["reason"])
                return
            if result.get("decision") not in {"accept", "revise"} or not result.get("reason"):
                raise ValueError("Audit needs a decision and reason")
            if result["decision"] == "revise":
                state["feedback"] = result["reason"]
                return
            if state.get("route_required"):
                raise ValueError("A flagged modeling tradeoff remains unreviewed")
            if "paper_input" in read_json(self.root / "config.json"):
                from trellis.modeling import paper_choices
                paper_choices.approved_choices(self.root, state)
            compliance = result.get("route_compliance")
            if compliance != ("verified" if state.get("route_decisions") else "not_needed"):
                raise ValueError("Stage approval requires route_compliance: verified against approved route decisions, or not_needed when no consequential tradeoff needs review")
            kind = task["kind"]
            if kind == "advance":
                stage = task["stage"]
                if stage != state["stage"]:
                    raise ValueError("Stale stage audit")
                errors = bootstrap_configuration_errors(self.root, state["observation"])
                if errors:
                    raise ValueError("; ".join(errors))
                pending = result.get("support_obligations", [])
                if not isinstance(pending, list) or any(not isinstance(item, str) or not item.strip() for item in pending):
                    raise ValueError("support_obligations must list diagnostic findings about missing model definitions")
                if pending and stage != "adaptation":
                    raise ValueError("Only adaptation approval can hand off missing model definitions")
                tool = STAGE_CHECKS[stage]
                if not passed(state["observation"], tool):
                    lean_steps = [step for step in state["observation"].get("steps", []) if step["name"] == "lean"]
                    lean_diagnosed = bool(lean_steps) and all(
                        step["exit_code"] in (0, 1) and not step.get("execution", {}).get("timeout")
                        for step in lean_steps)
                    if not (stage == "adaptation" and pending and lean_diagnosed
                            and passed(state["observation"], "aeneas")
                            and state["observation"].get("placeholders", 0) == 0):
                        raise ValueError(f"Stage audit needs a current successful {tool} observation, or an audited adaptation handoff of missing model support after extraction and a full Lean check")
                if stage == "adaptation":
                    self.validate_export()
                basis = setup_fingerprint(self.root) if stage == "setup" else source_fingerprint(self.root)
                state["audits"][stage] = dict(result, basis=basis)
                if stage == "adaptation" and "paper_input" in read_json(self.root / "config.json"):
                    from trellis.modeling import paper_choices
                    state["audits"][stage]["paper_choices_basis"] = paper_choices.basis(self.root, state)
                if stage == "adaptation":
                    state["audits"][stage]["axiom_compatibility_sha256"] = (
                        state["export_check"]["axiom_compatibility"]["sha256"])
                if stage == "adaptation" and "paper_input" in read_json(self.root / "config.json"):
                    from trellis.modeling.performance_loop import begin
                    begin(self.root, state)
                else:
                    state["stage"] = STAGES[STAGES.index(stage) + 1]
                if stage == "adaptation" and state["stage"] == "modeling":
                    state["status"] = "prepared"
            elif kind == "reopen":
                if state["stage"] != "adaptation":
                    raise ValueError("Setup reopening is only available during adaptation")
                self.reopen_setup()
                state["feedback"] = result["reason"]
            elif kind == "fail":
                state["status"] = "failed"
                state["failure"] = result["reason"]
            else:
                raise ValueError("Unknown preparatory audit disposition")
        else:
            raise ValueError("Unknown preparatory role")

    def _halt_engineering(self, evidence: Path, *, save_state: bool = True) -> None:
        """Preserve the original exception while attempting both existing halt records."""
        if self.state["status"] != "halted":
            self.state.update(resume_status=self.state["status"], status="halted",
                host_failure={"operation": "engineering_execution", "error": "Engineering execution unresolved",
                              "evidence": str(evidence)})
        try:
            if save_state:
                self.save()
        finally:
            if not any_halt_marker_present(runtime_root(self.root)):
                record_host_failure(self.root, runtime_root(self.root), operation="engineering_execution",
                    cycle=self.state["sequence"], evidence=evidence, error="Engineering execution unresolved")

    def step(self) -> dict:
        from trellis.modeling.engineering import validate_native_selection
        current_config = read_json(self.root / 'config.json')
        validate_native_selection(self.root, current_config)
        if current_config.get('engineering_protocol') != self.config.get('engineering_protocol'):
            raise ValueError('Engineering protocol selection changed')
        if self.config.get('engineering_protocol') == 'native-v1':
            return self._step_native_engineering()
        if self.state["stage"] == "engineering" and self.state["status"] == "active":
            from trellis.modeling.engineering_transport import request_owner
            with request_owner(self.root, self.state, self._halt_engineering):
                return self._step()
        return self._step()

    def _step_native_engineering(self) -> dict:
        from trellis.agent_wrapper import serial_worker
        from trellis.modeling import engineering, engineering_transport as transport
        from trellis.runtime.kernel_cli import run_kernel_cli
        check_halt(self.root)
        current = engineering.native_load(self.root)
        if engineering.native_handoff_complete(self.root, current):
            return self._step()
        if self.state.get('engineering', {}).get('handed_off'):
            raise RuntimeError('Native engineering handoff needs explicit completion')
        projected = engineering.native_project(self.root, self.state, current)
        if projected != self.state:
            self.state = projected
            self.save()
        if self.state['status'] != 'active':
            return self.state
        with transport.request_owner(self.root, self.state, self._halt_engineering) as owner:
            owner.native = dict(state=current)
            transport.native_event(owner, dict(event='start_cycle'))
            current = owner.native['state']
            projected = engineering.native_project(self.root, self.state, current)
            self.state.clear()
            self.state.update(projected)
            self.save()
            request = current['state']['in_flight_request']
            owner.sequence = request['id']
            owner.task = copy.deepcopy(self.state['next'])
            owner.native['request'] = request
            _, _, delivered, expected = transport.prepare_delivery(self.root, self.state, owner.task,
                                                                    native_request=request)
            owner.native.update(delivered=delivered, basis=expected)
            config = load_config(self.root / 'trellis.config.json')
            while owner.committed is None:
                check_halt(self.root)
                if (self.root / '.trellis/modeling/pause').exists():
                    raise RuntimeError('Engineering request paused before consumption')
                protocol = owner.native['state']['state']
                serial = protocol.get('worker_attempt')
                result = serial_worker.next_step(config, runtime_root(self.root), request,
                    serial['attempt'] if serial else None, contract_sha256=engineering.digest(request))
                if result['kind'] == 'worker_operation':
                    transport.native_event(owner, result['event'])
                    if owner.native['state']['state']['engineering'].get('halt'):
                        raise RuntimeError('Native engineering execution requires operator review')
                    continue
                if result['kind'] != 'engineering_checked':
                    raise RuntimeError('Native engineering returned an unexpected operation')
                event = run_kernel_cli(dict(action='engineering_checked_event', state=protocol,
                                             checked=result['checked']))['event']
                if event['event'] == 'engineering_checked':
                    transport.native_event(owner, event)
                elif event['event'] == 'engineering_outcome':
                    transport.native_consume(owner, event, terminal_scope=self.terminal_scope)
                else:
                    raise RuntimeError('Native engineering returned an unexpected event')
            return self.state

    def _step(self) -> dict:
        from trellis.checker.memory_retry import ResourceOutcome
        if self.state["status"] != "active":
            return self.state
        try:
            check_halt(self.root)
        except EngineeringExecutionFailure as error:
            if self.state['stage'] == 'engineering':
                raise
            self.halt('engineering_execution', error, error.evidence)
        if self.state["next"]["role"] == "performance":
            from trellis.modeling.performance_loop import step
            try:
                return step(self.root, self.state, self.agents)
            except EngineeringExecutionFailure as error:
                if self.state['stage'] == 'engineering':
                    raise
                self.halt('performance_execution', error, error.evidence)
            except ResourceOutcome as error:
                return self.defer_compute(error)
        pending = self.state.get("pending_compute_response")
        if pending and pending['work_fingerprint'] != fingerprint(self.root / 'work'):
            # A changed candidate cannot inherit an old audit/worker response.
            self.state.pop('pending_compute_response', None)
            pending = None
        try:
            self.refresh()
        except ResourceOutcome as error:
            return self.defer_compute(error)
        task = pending['task'] if pending else self.state["next"]
        stage_started = pending['stage'] if pending else self.state["stage"]
        if not pending:
            self.state["sequence"] += 1
        self.save()
        print(f"{self.state['sequence']}: {self.state['stage']} / {task['role']}", flush=True)
        guard = setup_fingerprint if self.state["stage"] == "adaptation" else source_fingerprint
        if stage_started == "engineering":
            from trellis.modeling import engineering, performance
            guard = engineering.input_guard if task["role"] == "worker" else performance.current_basis
        before = guard(self.root)
        snapshot = None
        if not pending and task["role"] == "worker" and self.state["stage"] != "setup":
            snapshot = self.root / ".trellis/modeling" / f"before-{self.state['sequence']}"
            copy_source_inputs(self.root / "work", snapshot)
        try:
            from trellis.modeling.native import NativeExportCandidateError
            try:
                if (not pending and task["role"] == "audit" and task.get("kind") == "advance"
                        and stage_started == "adaptation" and passed(self.state["observation"], "aeneas")
                        and not self.state["observation"].get("placeholders")):
                    self.validate_export()
            except NativeExportCandidateError as error:
                result = {"export_validation_error": str(error)}
            else:
                try:
                    result = pending['response'] if pending else self.agents(self.root, self.state, task)
                except AgentResponseError as error:
                    result = {"response_error": str(error)}
            try:
                if stage_started == "engineering" and task["role"] != "worker" and guard(self.root) != before:
                    raise RuntimeError("Engineering review changed the implementation or its build inputs")
                if snapshot and guard(self.root) != before:
                    # Only restore this burst's candidate; the reviewer can
                    # request a scoped earlier-stage repair through audit.
                    proposal = self.root / ".trellis/modeling/proposals" / str(self.state["sequence"])
                    copy_source_inputs(self.root / "work", proposal)
                    shutil.rmtree(self.root / "work")
                    shutil.copytree(snapshot, self.root / "work")
                    boundary = "extraction.json" if stage_started == "adaptation" else "crate or extraction.json"
                    raise ValueError(f"Closed-stage changes to {boundary} were restored; request audit-authorized reopening. Proposed work: {proposal}")
                self.refresh()
                if "response_error" in result:
                    raise ValueError(result["response_error"])
                if "export_validation_error" in result:
                    raise ValueError(result["export_validation_error"])
                self.apply(task, result)
            except (ValueError, KeyError, TypeError) as error:
                from trellis.modeling.routes import ROLES
                if stage_started == "engineering":
                    engineering.retry_feedback(self.state, task, result)
                    if task["role"] == "worker" and snapshot:
                        proposal = self.root / ".trellis/modeling/proposals" / str(self.state["sequence"])
                        if not proposal.exists():
                            copy_source_inputs(self.root / "work", proposal)
                        if (self.root / "engineering/plan.json").is_file() and not self.state["engineering"].get("proposal"):
                            proposed_plan = read_json(self.root / "engineering/plan.json")
                            write_json(proposal / "plan.json", proposed_plan)
                            self.state["engineering"]["proposal"] = proposed_plan
                        shutil.rmtree(self.root / "work")
                        shutil.copytree(snapshot, self.root / "work")
                        engineering.reopen(self.root, self.state, "Review the discovered candidate changes in proposal.json before adoption.")
                    else:
                        self.state["next"] = task
                else:
                    self.state["next"] = task if task["role"] in ROLES else {"role": "reviewer"}
                self.state["feedback"] = str(error)
            self.state["history"].append({"sequence": self.state["sequence"], "stage": stage_started,
                                          "task": {k: v for k, v in task.items() if k != "remaining"},
                                          "response": result, "feedback": self.state["feedback"]})
        except EngineeringExecutionFailure as error:
            if self.state['stage'] == 'engineering':
                raise
            self.halt('engineering_execution', error, error.evidence)
        except ResourceOutcome as error:
            if 'result' in locals():
                self.state['pending_compute_response'] = dict(task=task, response=result,
                    stage=stage_started, work_fingerprint=fingerprint(self.root / 'work'))
            elif not pending:
                self.state['sequence'] -= 1
            return self.defer_compute(error)
        except BaseException as error:
            if snapshot and guard(self.root) != before:
                shutil.rmtree(self.root / "work")
                shutil.copytree(snapshot, self.root / "work")
            if stage_started == "engineering":
                raise
            self.state["last_error"] = str(error) or type(error).__name__
            if isinstance(error, Exception) and self.state["status"] != "halted" and not any_halt_marker_present(runtime_root(self.root)):
                import traceback
                evidence = self.root / ".trellis/modeling/requests" / f"{self.state['sequence']:05d}-host-error.json"
                write_json(evidence, {"error": str(error), "traceback": traceback.format_exc(), "task": task})
                self.halt("preparation_step", error, evidence)
            if any_halt_marker_present(runtime_root(self.root)) and self.state["status"] != "halted":
                self.state["resume_status"] = self.state["status"]
                self.state["status"] = "halted"
            self.save()
            raise
        finally:
            if snapshot:
                shutil.rmtree(snapshot)
        self.state.pop("last_error", None)
        self.state.pop('pending_compute_response', None)
        self.state.pop('capacity_wait', None)
        if stage_started == "engineering":
            from trellis.modeling.engineering_transport import publish_outcome
            publish_outcome(self.root, self.state)
        else:
            self.save()
        commit_checkpoint({
            "root": str(runtime_root(self.root)), "event_count": self.state["sequence"],
            "metadata": {"repo_path": str(self.root), "config_path": str(self.root / "trellis.config.json")},
            "checkpoint": {"cycle": self.state["sequence"], "phase": "modeling"},
            "state": self.state,
        })
        print(f"  {self.state['status']}: {result.get('summary', result.get('reason', result.get('action', '')))}", flush=True)
        return self.state

    def defer_compute(self, error) -> dict:
        """Keep the candidate and any completed response outside provider retries."""
        self.state['capacity_wait'] = dict(error.detail, observed_at=time.time())
        self.save()
        # The owning preparation loop can retry or stop between bounded calls.
        # Do not turn host pressure into a host halt or an invalid-model result.
        for _ in range(10):
            check_halt(self.root)
            time.sleep(.1)
        return self.state
