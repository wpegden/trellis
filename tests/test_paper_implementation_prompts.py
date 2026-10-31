"""Rendered paper-mode lane regressions, including the retained PV fragments.

The fragment combinations below mirror the kernel's Modeling/TS lane selections.
They exercise the production renderer, not searches of its implementation. Kernel
tests separately check which fragments each typed request selects.
"""
from __future__ import annotations

import json
import re
from pathlib import Path

import pytest

from trellis.runtime.bridge_prompts import (
    PROMPT_FRAGMENT_ROOT,
    _render_prompt_fragment,
    build_audit_prompt,
    build_correspondence_prompt,
    build_paper_faithfulness_prompt,
    build_review_prompt,
    build_soundness_prompt,
    build_stuck_math_audit_prompt,
    build_worker_prompt,
    render_prompt_sections,
)


MODELING_BASE = (
    "pv/modeling/native/goal_meaning.md",
    "pv/modeling/native/process.md",
    "pv/modeling/native/reference.md",
)

# These include both same-path PV/math substitutions and PV-only preservation
# fragments. In particular the latter must not restore the old Decide policy.
LANE_FRAGMENTS = {
    "worker_modeling": MODELING_BASE + (
        "pv/modeling/native/worker.md",
        "pv/modeling/native/worker_scope.md",
        "pv/modeling/native/worker_self_checks.md",
        "pv/modeling/correspondence.md",
        "pv/modeling/substantiveness.md",
        "pv/modeling/soundness.md",
        "canonical/FAITHFULNESS.md",
    ),
    "worker_theorem_stating": (
        "pv/common/TRELLIS_FORMALIZATION_SCHEME.md",
        "pv/common/18_source_of_truth.md",
        "pv/worker/theorem_stating/05_frontier_work.md",
        "pv/worker/theorem_stating/12_first_request_decomposition.md",
        "pv/worker/theorem_stating/18_targets.md",
        "pv/worker/theorem_stating/20_common_failure_modes.md",
        "pv/modeling/substantiveness.md",
        "pv/modeling/faithfulness.md",
    ),
    "review_modeling": MODELING_BASE + (
        "pv/modeling/native/reviewer.md",
        "pv/modeling/native/clean_review.md",
        "pv/modeling/route_standard.md",
        "pv/modeling/substantiveness.md",
        "pv/modeling/soundness.md",
        "canonical/FAITHFULNESS.md",
    ),
    "review_theorem_stating": (
        "pv/common/TRELLIS_FORMALIZATION_SCHEME.md",
        "pv/review/27_source_of_truth.md",
        "pv/review/25_verifier_reasoning.md",
        "pv/review/33b_theorem_target_orientation.md",
        "pv/review/30a_blocker_actions.md",
        "pv/modeling/substantiveness.md",
        "pv/modeling/faithfulness.md",
    ),
    "audit_modeling": MODELING_BASE + (
        "pv/modeling/native/audit.md",
        "pv/modeling/route_standard.md",
        "canonical/FAITHFULNESS.md",
    ),
    "audit_route": MODELING_BASE + (
        "pv/modeling/native/route_audit.md",
        "pv/modeling/route_standard.md",
    ),
    "route_planner": MODELING_BASE + (
        "pv/modeling/native/route_planner.md",
        "pv/modeling/native/route_planner_output.md",
        "pv/modeling/route_standard.md",
    ),
    "route_critic": MODELING_BASE + (
        "pv/modeling/native/route_critic.md",
        "pv/modeling/native/route_critic_output.md",
        "pv/modeling/route_standard.md",
    ),
    # Paper's contract is rebuilt with original math-mode target authority.
    "paper_targets": (
        "verifier/paper_faithfulness/20_targets.md",
        "verifier/paper_faithfulness/30_contract.md",
        "verifier/paper_faithfulness/40_rubric.md",
        "verifier/paper_faithfulness/50_authority.md",
        "canonical/FAITHFULNESS.md",
    ),
    "paper_node_substantiveness": (
        "verifier/substantiveness/05_fresh_node_frontier.md",
        "verifier/substantiveness/30_contract.md",
        "verifier/substantiveness/40_rubric.md",
        "verifier/substantiveness/50_authority.md",
        "canonical/SUBSTANTIVENESS.md",
    ),
    "corr": MODELING_BASE + (
        "pv/verifier/correspondence/05_frontier.md",
        "pv/verifier/correspondence/09_pv_correspondence.md",
        "verifier/correspondence/30_contract.md",
        "verifier/correspondence/40_rubric.md",
        "pv/modeling/correspondence.md",
    ),
    "sound": MODELING_BASE + (
        "verifier/soundness/05_theorem_target.md",
        "pv/verifier/soundness/05_pv_floor.md",
        "verifier/soundness/30_contract.md",
        "verifier/soundness/40_rubric.md",
        "verifier/soundness/50_authority.md",
        "pv/modeling/soundness.md",
    ),
}


def _context(tmp_path: Path, *, paper: bool) -> dict[str, str]:
    # The test is about actual rendered instructions. Give every optional lane
    # payload a neutral, identifiable value without mocking fragment loading or
    # adaptation. Builder/context propagation has its own tests in this module.
    context = {
        key: f"fixture:{key}"
        for path in PROMPT_FRAGMENT_ROOT.rglob("*.md")
        for key in re.findall(r"\{\{\s*([a-zA-Z_][a-zA-Z0-9_]*)\s*\}\}", path.read_text())
    }
    context.update(
        repo_path=str(tmp_path),
        paper_tex_path="paper/original.tex",
        paper_implementation_mode="true" if paper else "false",
        process_phase="modeling",
        pv_process_description="Ordinary PV process fixture.",
        contract_json='{"paper_implementation":{"route":"rust_aeneas"}}',
        rubric_json="{}",
        project_invariants_json="{}",
        request_summary_json="{}",
    )
    return context


def _render(lane: str, tmp_path: Path, *, paper: bool = True):
    return render_prompt_sections(
        LANE_FRAGMENTS[lane], _context(tmp_path, paper=paper), backend="lean"
    )


@pytest.mark.parametrize("lane", LANE_FRAGMENTS)
def test_paper_lanes_share_original_paper_and_actual_implementation_authority(lane, tmp_path):
    sections = _render(lane, tmp_path)
    text = "\n\n".join(section["text"] for section in sections)
    ids = [section["fragment_id"] for section in sections]
    assert ids.count("paper_implementation/authority.md") == 1
    assert ids.count("paper_implementation/process.md") == 1
    assert "configured LaTeX paper supplies the selected claims" in text
    assert "`target_claims` for collective coverage" in text
    assert "actual implementation" in text
    assert "checked refinement and composition" in text
    assert "paper_implementation_review" in text
    assert "Formal completion requires checked transitive closure" in text
    assert "GOAL.md" not in text
    assert "selected/excluded clauses" in text
    assert "excludes cost proofs" in text


@pytest.mark.parametrize("lane", LANE_FRAGMENTS)
def test_paper_lanes_have_one_late_human_gate_and_no_inherited_decide_route(lane, tmp_path):
    text = "\n\n".join(section["text"] for section in _render(lane, tmp_path))
    assert "exactly one human checkpoint" in text
    assert "after theorem-stating arguments" in text
    assert "before proof formalization" in text
    assert "algorithm fidelity, Rust modeling exceptions, and paper/formal correspondence" in text
    assert "reviewed semantics are fixed" in text
    for stale in (
        "outside-expert review before argument development",
        "When disproof is active",
        "active disproof direction",
        "system-derived exact negation",
        "proof or disproof",
        "selected_polarity",
    ):
        assert stale not in text, f"{lane} retained obsolete instruction: {stale}"


@pytest.mark.parametrize("lane", LANE_FRAGMENTS)
def test_paper_lanes_keep_native_per_choice_agent_approval_and_existing_trust(lane, tmp_path):
    text = "\n\n".join(section["text"] for section in _render(lane, tmp_path))
    assert "existing planner/critic route and final agent approval" in text
    assert "Authored semantics supplying an extraction boundary require inventoried choices" in text
    assert "final agent approval before use" in text
    assert "paper-model:<NodeId>" in text
    assert "Both rounds and approved tasks identify each consequential choice" in text
    assert "Routine bindings use existing preparation and artifact reviews" in text
    assert "Mathematical definitions, refinement relations and theorem proofs use ordinary proof nodes" in text
    assert "Required Modeling support and refinement arguments" in text
    assert "unchanged trust basis" in text
    assert "implementation definitions use safe, total core Lean" in text
    assert "Proof dependencies follow the pinned proof project" in text
    assert "Compiler/runtime correctness remains trusted" in text


def test_modeling_success_continues_to_argument_development_without_human_gate(tmp_path):
    text = "\n\n".join(section["text"] for section in _render("review_modeling", tmp_path))
    assert "acceptance enters TheoremStating" in text
    assert "Required Modeling support and refinement arguments precede theorem stating" in text


def test_corr_and_sound_retain_their_distinct_preservation_subjects(tmp_path):
    corr = "\n\n".join(section["text"] for section in _render("corr", tmp_path))
    sound = "\n\n".join(section["text"] for section in _render("sound", tmp_path))
    assert "Judge each node's own Lean statement against its own `.tex` statement" in corr
    assert "Body extraction is pinned by provenance" in corr
    assert "rather than the pinned declaration itself" in corr
    assert "For a definition or Preamble, the arguments are its preservation sections" in sound
    assert "Reports of earlier agent judgments do not supply that evidence" in sound
    assert "Missing arguments and `SKETCH:` are incomplete" in sound


def test_paper_verifier_keeps_collective_math_mode_target_check(tmp_path):
    text = "\n\n".join(section["text"] for section in _render("paper_targets", tmp_path))
    assert "covering `.tex` statements collectively capture the configured paper target" in text
    assert "Read the configured paper file directly from disk at `paper/original.tex`" in text
    assert "manifest's original locators" in text


@pytest.mark.parametrize("lane", LANE_FRAGMENTS)
def test_ordinary_pv_fragment_rendering_is_unchanged(lane, tmp_path):
    context = _context(tmp_path, paper=False)
    sections = render_prompt_sections(LANE_FRAGMENTS[lane], context, backend="lean")
    expected = [
        (fragment, _render_prompt_fragment(fragment, context, backend="lean"))
        for fragment in LANE_FRAGMENTS[lane]
    ]
    assert [(section["fragment_id"], section["text"]) for section in sections] == [
        (fragment, rendered) for fragment, rendered in expected if rendered
    ]
    assert not any(section["fragment_id"].startswith("paper_implementation/") for section in sections)


@pytest.mark.parametrize("fragments", [
    ("worker/theorem_stating/00_intro.md", "worker/common/18_reference_paper.md"),
    ("review/common/27_reference_paper.md", "review/common/33b_theorem_target_orientation.md"),
    ("verifier/correspondence/05_frontier.md", "canonical/CORRESPONDENCE.md"),
    ("verifier/soundness/05_theorem_target.md", "canonical/SOUNDNESS.md"),
    LANE_FRAGMENTS["paper_targets"],
])
def test_ordinary_math_fragment_rendering_is_unchanged(fragments, tmp_path):
    context = _context(tmp_path, paper=False)
    sections = render_prompt_sections(fragments, context, backend="lean")
    assert [section["fragment_id"] for section in sections] == list(fragments)
    assert [section["text"] for section in sections] == [
        _render_prompt_fragment(fragment, context, backend="lean") for fragment in fragments
    ]


@pytest.mark.parametrize("route", ("rust_aeneas", "lean_native"))
@pytest.mark.parametrize("role,lane,builder,contract_fragment", [
    ("worker", "worker_modeling", build_worker_prompt, "worker/common/40_contract.md"),
    ("review", "review_modeling", build_review_prompt, "review/common/30_contract.md"),
    ("audit", "audit_modeling", build_audit_prompt, None),
    ("stuck_math_audit", "route_planner", build_stuck_math_audit_prompt, None),
    ("paper", "paper_targets", build_paper_faithfulness_prompt, None),
    ("corr", "corr", build_correspondence_prompt, None),
    ("sound", "sound", build_soundness_prompt, None),
])
def test_real_lane_builders_propagate_both_routes_and_pinned_evidence(
    route, role, lane, builder, contract_fragment, tmp_path
):
    """Exercise actual builder context creation, with no patched render path."""
    (tmp_path / "trellis.config.json").write_text(json.dumps({
        "workflow": {"paper_tex_path": "paper/original.tex"},
    }))
    subject = {
        "version": 1,
        "route": route,
        "manifest_path": "paper-implementation/manifest.json",
        "implementation_roots": {"paper-result": ["ActualImplementation"]},
    }
    evidence = {
        "paper": "paper/original.tex",
        "configuration_sha256": "a" * 64,
        "modeling_choices": [{"node": "ManualBinding", "task": "paper-model:ManualBinding"}],
    }
    fragments = list(LANE_FRAGMENTS[lane])
    if contract_fragment:
        fragments.append(contract_fragment)
    # Just the ordinary fields each builder consumes; no fabricated agent or
    # human approvals. The contract's subject/evidence mirror kernel projection.
    contract = {
        "paper_implementation": subject,
        "paper_implementation_review": evidence,
        "request_summary": {"phase": "modeling", "nodes": ["ActualImplementation"]},
        "prompt_fragments": fragments,
        "prompt_schema_example": {"decision": "PASS"},
        "artifact_prompt_view": {"json_check_command_template": ["python3", "check.py"]},
        "rubric": {},
        "target_covering_nodes": {"paper-result": ["ImplementationCorrect"]},
        "blocker_partition": {"choices": []},
        "blocker_choices_block": {"md": "No blockers.", "sidecar_payload": None},
        "verifier_evidence": {},
    }
    request = {
        "phase": "modeling",
        "paper_implementation": subject,
        "project_invariants": {},
        f"{role}_contract": contract,
    }
    kwargs = {
        "request": request,
        "repo_path": tmp_path,
        "raw_output_path": tmp_path / "result.raw.json",
        "done_path": tmp_path / "result.done",
    }
    if role == "worker":
        kwargs.update(worker_gate={"worker_acceptance": {}},
                      acceptance_context_path=tmp_path / "acceptance.json")
    elif role in {"review", "audit", "stuck_math_audit"}:
        kwargs.update(runtime_root=tmp_path, context_json_path=tmp_path / "context.json")
    else:
        kwargs["lane_id"] = "reviewer-a"
        if role == "sound":
            kwargs["node_name"] = "ActualImplementation"
    text = builder(**kwargs)
    assert "exactly one human checkpoint" in text
    assert "configured LaTeX paper supplies the selected claims" in text
    assert f'"route": "{route}"' in text
    assert "ActualImplementation" in text
    assert "paper-model:ManualBinding" in text
    assert "GOAL.md" not in text
    assert "outside-expert review before argument development" not in text
    assert "system-derived exact negation" not in text
