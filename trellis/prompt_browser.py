from __future__ import annotations

import json
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Dict, List, Mapping

from trellis.runtime.bridge_prompts import (
    PROMPT_SCHEME_REFERENCE_PATH,
    _filespec_path,
    _json_fence,
    _lane_scoped_contract_json,
    _loogle_helper_path,
    _sound_lane_scoped_contract_json,
    _validated_artifact_instructions,
    contract_previous_own_findings_by_lane,
    contract_previous_own_findings_per_node,
    contract_prompt_fragments,
    contract_request_summary,
    contract_required_mapping,
    flatten_sound_previous_findings_for_lane,
    paper_target_covering_nodes,
    render_prompt_sections,
    request_contract_block,
    request_project_invariants,
    worker_gate_acceptance,
)
from trellis.runtime.kernel_cli import KernelCliError, run_kernel_cli


@dataclass(frozen=True)
class PromptScenario:
    scenario_id: str
    role: str
    title: str
    description: str
    request_factory: Callable[[], Dict[str, Any]]


TARGET_ID = "thm:conn"
DEF_NODE = "EventDef"
HELPER_NODE = "SubcriticalMean"
TARGET_NODE = "ThmConn"
PROOF_NODE = "HasMediumComponent"
LANE_1 = "lane_1"
LANE_2 = "lane_2"


def _set(*items: str) -> List[str]:
    return list(items)


def _blocker(kind: str, *, node: str | None = None, target: str | None = None) -> Dict[str, Any]:
    if node is not None:
        obj: Dict[str, Any] = {"otype": "node", "node": node}
        suffix = node
    elif target is not None:
        obj = {"otype": "target", "target": target}
        suffix = target
    else:  # pragma: no cover - helper misuse
        raise ValueError("blocker requires node or target")
    return {
        "kind": kind,
        "object": obj,
        "fingerprint": f"{kind.lower()}::{suffix}",
    }


def _corr_lane(decision: str, *, node: str, summary: str, comments: str) -> Dict[str, Any]:
    # Post-2026-04-30: corr-node payloads use the substantiveness-shaped
    # `verdicts[]` schema rather than `issues[]`. PASS lane = empty verdicts
    # in the fixture (no per-node detail to surface); FAIL lane = a single
    # explicit Fail verdict on the named node. Comment carries the summary.
    is_pass = decision.upper() == "PASS"
    verdict_entry = {"node": node, "verdict": "Fail", "comment": summary}
    return {
        "correspondence": {
            "decision": decision,
            "verdicts": [] if is_pass else [verdict_entry],
        },
        "overall": "APPROVE" if is_pass else "REJECT",
        "summary": summary,
        "comments": comments,
    }


def _paper_lane(decision: str, *, target: str, summary: str, comments: str) -> Dict[str, Any]:
    return {
        "paper_faithfulness": {
            "decision": decision,
            "issues": [
                {
                    "node": target,
                    "description": summary,
                }
            ],
        },
        "overall": "APPROVE" if decision.upper() == "PASS" else "REJECT",
        "summary": summary,
        "comments": comments,
    }


def _sound_lane(decision: str, *, node: str, summary: str, comments: str) -> Dict[str, Any]:
    return {
        "node": node,
        "soundness": {
            "decision": decision,
            "explanation": summary,
        },
        "overall": "APPROVE" if decision.upper() == "SOUND" else "REJECT",
        "summary": summary,
        "comments": comments,
    }


def _common_request(*, kind: str, phase: str) -> Dict[str, Any]:
    return {
        "id": 1,
        "kind": kind,
        "cycle": 7,
        "phase": phase,
        "active_node": TARGET_NODE,
        "held_target": TARGET_NODE,
        "mode": "Global",
        "configured_targets": _set(TARGET_ID),
        "current_present_nodes": _set("Preamble", DEF_NODE, HELPER_NODE, TARGET_NODE, PROOF_NODE),
        "current_proof_nodes": _set(HELPER_NODE, TARGET_NODE, PROOF_NODE),
        "current_node_kinds": {
            "Preamble": "Preamble",
            DEF_NODE: "Definition",
            HELPER_NODE: "Proof",
            TARGET_NODE: "Proof",
            PROOF_NODE: "Proof",
        },
        "current_deps": {
            DEF_NODE: _set("Preamble"),
            HELPER_NODE: _set("Preamble", DEF_NODE),
            TARGET_NODE: _set("Preamble", DEF_NODE, HELPER_NODE),
            PROOF_NODE: _set("Preamble", DEF_NODE),
        },
        "current_semantic_deps": {
            DEF_NODE: _set("Preamble"),
            HELPER_NODE: _set("Preamble", DEF_NODE),
            TARGET_NODE: _set("Preamble", DEF_NODE, HELPER_NODE),
            PROOF_NODE: _set("Preamble", DEF_NODE),
        },
        "current_target_claims": {
            TARGET_NODE: _set(TARGET_ID),
        },
        "current_paper_approved_fingerprints": {
            TARGET_ID: "paper::approved::conn",
        },
    }


def _paper_request(*, revisit: bool) -> Dict[str, Any]:
    request = _common_request(kind="Paper", phase="TheoremStating")
    request.update(
        {
            "verify_lanes": _set(LANE_1, LANE_2),
            "paper_verify_targets": _set(TARGET_ID),
            "verify_targets": _set(TARGET_ID),
            "blocked_targets": _set(TARGET_ID),
        }
    )
    if revisit:
        request["previous_paper_lane_findings"] = {
            LANE_1: _paper_lane(
                "FAIL",
                target=TARGET_ID,
                summary="The current covering set omits a manuscript condition.",
                comments="Re-read the target and ensure the node set jointly states every hypothesis.",
            )
        }
    return request


def _corr_request(*, revisit: bool, with_preamble: bool) -> Dict[str, Any]:
    request = _common_request(kind="Corr", phase="TheoremStating")
    verify_nodes = [TARGET_NODE]
    if with_preamble:
        verify_nodes.append("Preamble")
    request.update(
        {
            "verify_lanes": _set(LANE_1, LANE_2),
            "corr_verify_nodes": verify_nodes,
            "verify_nodes": verify_nodes,
            "corr_verify_targets": _set(TARGET_ID),
            "blocked_targets": _set(TARGET_ID),
        }
    )
    if revisit:
        request["previous_corr_lane_findings"] = {
            LANE_1: _corr_lane(
                "FAIL",
                node=TARGET_NODE,
                summary="The Lean statement dropped a quantitative hypothesis from the TeX statement.",
                comments="Check the exact assumptions and restore any missing bound or quantifier.",
            )
        }
    return request


def _sound_request(*, phase: str, revisit: bool, node: str) -> Dict[str, Any]:
    request = _common_request(kind="Sound", phase=phase)
    request.update(
        {
            "sound_verify_node": node,
            "sound_verify_nodes": _set(node),
            "verify_nodes": _set(node),
            "verify_lanes": _set(LANE_1, LANE_2),
            "active_node": node,
            "held_target": TARGET_NODE if phase == "TheoremStating" else None,
        }
    )
    if revisit:
        # Audit Finding 3: previous_sound_lane_findings is keyed by node
        # first, then lane, mirroring the per-node accumulation in
        # apply_sound_response. The kernel-side JSON contract field is
        # `previous_own_findings` (the `_by_lane` suffix is dropped for
        # the sound contract because the outer key is no longer a lane).
        request["previous_sound_lane_findings"] = {
            node: {
                LANE_1: _sound_lane(
                    "STRUCTURAL",
                    node=node,
                    summary="The proof sketch still assumes a helper result that is not among the imported nodes.",
                    comments="Either add the missing helper node or rewrite the NL proof to use only imported statements.",
                )
            }
        }
    return request


def _worker_request(
    *,
    phase: str,
    mode: str,
    worker_profile: str,
    validation_kind: str,
    blockers: List[Dict[str, Any]] | None = None,
    held_target: str | None = TARGET_NODE,
    active_node: str | None = TARGET_NODE,
    next_context_mode: str = "resume",
    fresh_context: bool | None = None,
    reviewer_comments: str = "",
    retry_outcome_kind: str = "None",
    allow_new_obligations: bool = True,
    must_close_active: bool = False,
) -> Dict[str, Any]:
    request = _common_request(kind="Worker", phase=phase)
    authorized_nodes = _set("Preamble", DEF_NODE, HELPER_NODE, TARGET_NODE, PROOF_NODE)
    request.update(
        {
            "mode": mode,
            "active_node": active_node,
            "held_target": held_target,
            "blockers": blockers or [],
            "blocked_targets": _set(TARGET_ID) if blockers else [],
            "reviewer_comments": reviewer_comments,
            "retry_outcome_kind": retry_outcome_kind,
            "retry_attempt": 1 if retry_outcome_kind != "None" else 0,
            "deterministic_worker_rejection_reasons": (
                [
                    "Tablet/SubcriticalExpectation.lean has an application type mismatch because p is applied to n : ℝ instead of n : ℕ.",
                    "Tablet/SubcriticalExpectation.lean fails to synthesize an HPow instance for a real exponent expression.",
                ]
                if retry_outcome_kind == "Invalid"
                else []
            ),
            "fresh_context": next_context_mode == "fresh" if fresh_context is None else fresh_context,
            "worker_context": {
                "enabled": True,
                "active_difficulty": "Hard" if worker_profile != "ProofEasy" else "Easy",
                "active_easy_attempts": 0,
                "worker_profile": worker_profile,
                "validation_kind": validation_kind,
                "authorized_nodes": authorized_nodes,
                "next_context_mode": next_context_mode,
                "allow_new_obligations": allow_new_obligations,
                "must_close_active": must_close_active,
                "paper_focus_ranges": [],
                "work_style_hint": "restructure" if validation_kind in {"ProofRestructure", "ProofCoarseRestructure"} else "none",
            },
            "worker_acceptance": {
                "enabled": True,
                "validation_kind": validation_kind,
                "authorized_nodes": authorized_nodes,
                "validation_execution_plan": [],
            },
        }
    )
    if retry_outcome_kind != "None":
        request["invalid_attempt"] = retry_outcome_kind == "Invalid"
    return request


def _review_request(
    *,
    retry_outcome_kind: str = "None",
    blockers: List[Dict[str, Any]] | None = None,
    review_verifier_evidence: Dict[str, Any] | None = None,
    human_input_outstanding: bool = False,
) -> Dict[str, Any]:
    request = _common_request(kind="Review", phase="TheoremStating")
    request.update(
        {
            "blockers": blockers or [],
            "blocked_targets": _set(TARGET_ID) if blockers else [],
            "retry_outcome_kind": retry_outcome_kind,
            "retry_attempt": 1 if retry_outcome_kind != "None" else 0,
            "human_input_outstanding": human_input_outstanding,
            "allowed_decisions": _set("Continue", "NeedInput", "AdvancePhase"),
            "allowed_next_modes": _set("Global", "Targeted"),
            "kernel_hinted_next_active_nodes": _set(TARGET_NODE, HELPER_NODE),
            "targeted_next_active_nodes": _set(TARGET_NODE, HELPER_NODE),
            "allowed_resets": _set("None", "LastCommit"),
            "allowed_reset_blockers": blockers or [],
            "review_verifier_evidence": review_verifier_evidence or {"paper": {}, "corr": {}, "sound": {}},
            "deterministic_worker_rejection_reasons": (
                [
                    "Tablet/Preamble.lean imported Mathlib.Order.Filter.AtTopBot, which does not exist at the pinned mathlib revision."
                ]
                if retry_outcome_kind == "Invalid"
                else []
            ),
        }
    )
    if retry_outcome_kind != "None":
        request["invalid_attempt"] = retry_outcome_kind == "Invalid"
    return request


def _review_evidence(kind: str, *, split: bool) -> Dict[str, Any]:
    if kind == "paper":
        return {
            "paper": {
                LANE_1: _paper_lane(
                    "FAIL",
                    target=TARGET_ID,
                    summary="The covering nodes still omit a key manuscript hypothesis.",
                    comments="Focus on the exact paper target, not the broader proof branch.",
                ),
                LANE_2: _paper_lane(
                    "PASS" if split else "FAIL",
                    target=TARGET_ID,
                    summary="Lane 2 thinks the current covering set is close but still incomplete." if split else "Lane 2 agrees the current set is incomplete.",
                    comments="If you keep the same covering nodes, rewrite the TeX so the target is fully covered.",
                ),
            },
            "corr": {},
            "sound": {},
        }
    if kind == "corr":
        return {
            "paper": {},
            "corr": {
                LANE_1: _corr_lane(
                    "FAIL",
                    node=TARGET_NODE,
                    summary="The Lean statement weakened the NL statement.",
                    comments="Restore the dropped quantifier and any manuscript-side assumptions.",
                ),
                LANE_2: _corr_lane(
                    "PASS" if split else "FAIL",
                    node=TARGET_NODE,
                    summary="Lane 2 believes the translation is acceptable." if split else "Lane 2 also finds the Lean/NL mismatch.",
                    comments="Check whether the issue is just naming or a real statement mismatch.",
                ),
            },
            "sound": {},
        }
    return {
        "paper": {},
        "corr": {},
        # Audit Finding 3: review_verifier_evidence.sound is now keyed
        # by node first, then lane.
        "sound": {
            TARGET_NODE: {
                LANE_1: _sound_lane(
                    "STRUCTURAL",
                    node=TARGET_NODE,
                    summary="The NL proof depends on unstated isolated-vertex estimates.",
                    comments="Extract those estimates into helper nodes before returning to the main target.",
                ),
                LANE_2: _sound_lane(
                    "SOUND" if split else "STRUCTURAL",
                    node=TARGET_NODE,
                    summary="Lane 2 is tentatively satisfied." if split else "Lane 2 also thinks the branch needs decomposition.",
                    comments="Be explicit about which imported helpers justify each proof step.",
                ),
            },
        },
    }


SCENARIOS: List[PromptScenario] = [
    PromptScenario("paper_fresh", "paper_faithfulness", "Paper Faithfulness: Fresh", "Fresh paper-faithfulness lane on a target package.", lambda: _paper_request(revisit=False)),
    PromptScenario("paper_revisit", "paper_faithfulness", "Paper Faithfulness: Revisit", "Revisit paper-faithfulness after prior lane findings.", lambda: _paper_request(revisit=True)),
    PromptScenario("corr_fresh", "correspondence", "Correspondence: Fresh", "Fresh correspondence check on the frontier.", lambda: _corr_request(revisit=False, with_preamble=False)),
    PromptScenario("corr_fresh_with_preamble", "correspondence", "Correspondence: Fresh With Preamble", "Fresh correspondence check when Preamble is on the frontier.", lambda: _corr_request(revisit=False, with_preamble=True)),
    PromptScenario("corr_revisit", "correspondence", "Correspondence: Revisit", "Revisit correspondence with prior lane findings.", lambda: _corr_request(revisit=True, with_preamble=False)),
    PromptScenario("corr_revisit_with_preamble", "correspondence", "Correspondence: Revisit With Preamble", "Revisit correspondence including the preamble frontier.", lambda: _corr_request(revisit=True, with_preamble=True)),
    PromptScenario("sound_theorem_fresh", "soundness", "Soundness: Theorem Fresh", "Fresh theorem-stating soundness target.", lambda: _sound_request(phase="TheoremStating", revisit=False, node=TARGET_NODE)),
    PromptScenario("sound_theorem_revisit", "soundness", "Soundness: Theorem Revisit", "Revisit theorem-stating soundness target.", lambda: _sound_request(phase="TheoremStating", revisit=True, node=TARGET_NODE)),
    PromptScenario("sound_proof_fresh", "soundness", "Soundness: Proof Fresh", "Fresh proof-formalization soundness target.", lambda: _sound_request(phase="ProofFormalization", revisit=False, node=PROOF_NODE)),
    PromptScenario("sound_proof_revisit", "soundness", "Soundness: Proof Revisit", "Revisit proof-formalization soundness target.", lambda: _sound_request(phase="ProofFormalization", revisit=True, node=PROOF_NODE)),
    PromptScenario("worker_theorem_frontier", "worker", "Theorem Worker: Frontier", "Initial theorem-stating frontier work.", lambda: _worker_request(phase="TheoremStating", mode="Global", worker_profile="Theorem", validation_kind="TheoremGlobal", fresh_context=True, reviewer_comments="Build the initial paper-faithful theorem DAG.")),
    PromptScenario("worker_theorem_after_invalid_retry", "worker", "Theorem Worker: After Invalid Retry", "Theorem-stating retry after an invalid deterministic worker rejection.", lambda: _worker_request(phase="TheoremStating", mode="Global", worker_profile="Theorem", validation_kind="TheoremGlobal", held_target=None, active_node=None, retry_outcome_kind="Invalid")),
    PromptScenario("worker_theorem_after_paper_faithfulness", "worker", "Theorem Worker: After Paper-Faithfulness Review", "Targeted theorem repair after paper-faithfulness blockers.", lambda: _worker_request(phase="TheoremStating", mode="Targeted", worker_profile="Theorem", validation_kind="TheoremTargeted", held_target=None, blockers=[_blocker("PaperFaithfulness", target=TARGET_ID)], reviewer_comments="Adjust the covering nodes so the paper target is fully and faithfully covered.")),
    PromptScenario("worker_theorem_after_correspondence", "worker", "Theorem Worker: After Correspondence Review", "Targeted theorem repair after correspondence blockers.", lambda: _worker_request(phase="TheoremStating", mode="Targeted", worker_profile="Theorem", validation_kind="TheoremTargeted", held_target=None, blockers=[_blocker("NodeCorr", node=TARGET_NODE)], reviewer_comments="Repair the Lean/NL statement mismatch on the current target branch.")),
    PromptScenario("worker_theorem_after_soundness", "worker", "Theorem Worker: After Soundness Review", "Targeted theorem repair while holding a soundness target.", lambda: _worker_request(phase="TheoremStating", mode="Targeted", worker_profile="Theorem", validation_kind="TheoremTargeted", held_target=TARGET_NODE, blockers=[_blocker("Soundness", node=TARGET_NODE)], reviewer_comments="Restructure the target branch to justify the flagged NL proof steps.")),
    PromptScenario("worker_proof_local_close", "worker", "Proof Worker: Local, Must Close", "Local proof scope with no new open obligations and active closure required.", lambda: _worker_request(phase="ProofFormalization", mode="Local", worker_profile="ProofEasy", validation_kind="ProofLocal", active_node=PROOF_NODE, held_target=None, next_context_mode="fresh", allow_new_obligations=False, must_close_active=True, reviewer_comments="Close the active proof node without changing imports or TeX.")),
    PromptScenario("worker_proof_local_partial", "worker", "Proof Worker: Local, Partial Progress", "Local proof scope allowing scoped open helper obligations.", lambda: _worker_request(phase="ProofFormalization", mode="Local", worker_profile="ProofHard", validation_kind="ProofLocal", active_node=PROOF_NODE, held_target=None, allow_new_obligations=True, must_close_active=False, reviewer_comments="Repair the active proof node while keeping the surrounding support package stable.")),
    PromptScenario("worker_proof_restructure", "worker", "Proof Worker: Restructure", "Proof work with active-node-centered restructure authorization.", lambda: _worker_request(phase="ProofFormalization", mode="Restructure", worker_profile="ProofHard", validation_kind="ProofRestructure", active_node=PROOF_NODE, held_target=None, allow_new_obligations=True, must_close_active=False, reviewer_comments="Introduce or revise nearby helper nodes if that is the honest way to close the proof.")),
    PromptScenario("worker_proof_coarse_restructure", "worker", "Proof Worker: Coarse Restructure", "Proof work with coarse restructure authorization.", lambda: _worker_request(phase="ProofFormalization", mode="CoarseRestructure", worker_profile="ProofHard", validation_kind="ProofCoarseRestructure", active_node=PROOF_NODE, held_target=None, allow_new_obligations=True, must_close_active=False, reviewer_comments="Broaden the support package if the active node cannot be closed without wider proof decomposition.")),
    PromptScenario("worker_cleanup_orphan_cleanup", "worker", "Cleanup Worker: Orphan Cleanup", "Cleanup worker removing or attaching orphaned nodes.", lambda: _worker_request(phase="Cleanup", mode="Cleanup", worker_profile="Cleanup", validation_kind="Cleanup", active_node=TARGET_NODE, held_target=None, reviewer_comments="Delete or reattach orphaned nodes without perturbing the accepted live snapshot.")),
]


_REVIEW_BASES: List[tuple[str, str, Dict[str, Any]]] = [
    ("review_after_worker_invalid", "Reviewer: After Worker Invalid", _review_request(retry_outcome_kind="Invalid")),
    ("review_after_worker_stuck", "Reviewer: After Worker Stuck", _review_request(retry_outcome_kind="Stuck")),
    ("review_after_worker_needs_restructure", "Reviewer: After Worker Needs Restructure", _review_request(retry_outcome_kind="NeedsRestructure")),
    ("review_after_failed_paper_faithfulness", "Reviewer: After Failed Paper-Faithfulness", _review_request(blockers=[_blocker("PaperFaithfulness", target=TARGET_ID)], review_verifier_evidence=_review_evidence("paper", split=False))),
    ("review_after_split_paper_faithfulness", "Reviewer: After Split Paper-Faithfulness", _review_request(blockers=[_blocker("PaperFaithfulness", target=TARGET_ID)], review_verifier_evidence=_review_evidence("paper", split=True))),
    ("review_after_failed_correspondence", "Reviewer: After Failed Correspondence", _review_request(blockers=[_blocker("NodeCorr", node=TARGET_NODE)], review_verifier_evidence=_review_evidence("corr", split=False))),
    ("review_after_split_correspondence", "Reviewer: After Split Correspondence", _review_request(blockers=[_blocker("NodeCorr", node=TARGET_NODE)], review_verifier_evidence=_review_evidence("corr", split=True))),
    ("review_after_failed_soundness", "Reviewer: After Failed Soundness", _review_request(blockers=[_blocker("Soundness", node=TARGET_NODE)], review_verifier_evidence=_review_evidence("sound", split=False))),
    ("review_after_split_soundness", "Reviewer: After Split Soundness", _review_request(blockers=[_blocker("Soundness", node=TARGET_NODE)], review_verifier_evidence=_review_evidence("sound", split=True))),
    ("review_after_clean_verification", "Reviewer: After Clean Verification", _review_request()),
]

for base_id, title, request in _REVIEW_BASES:
    SCENARIOS.append(
        PromptScenario(
            base_id,
            "reviewer",
            title,
            title,
            lambda request=request: dict(request),
        )
    )
    with_human = dict(request)
    with_human["human_input_outstanding"] = True
    SCENARIOS.append(
        PromptScenario(
            f"{base_id}_with_outstanding_human_input",
            "reviewer",
            f"{title} + Outstanding Human Input",
            f"{title} with the outstanding-human-input overlay fragment.",
            lambda request=with_human: dict(request),
        )
    )


def _scenario_map() -> Dict[str, PromptScenario]:
    return {scenario.scenario_id: scenario for scenario in SCENARIOS}


def _kernel_bridge_request_payload(repo_path: Path, request: Mapping[str, Any]) -> Dict[str, Any]:
    response = run_kernel_cli(
        {
            "action": "bridge_request_payload",
            "repo_path": str(repo_path),
            "request": dict(request),
        }
    )
    if response.get("status") != "bridge_request_payload_ok":
        raise KernelCliError(
            f"unexpected bridge_request_payload status: {response.get('status')!r}"
        )
    payload = response.get("payload")
    if not isinstance(payload, dict):
        raise KernelCliError("bridge_request_payload response is missing payload")
    return payload


def _prompt_root(repo_path: Path, scenario_id: str) -> Path:
    return repo_path / ".trellis" / "prompt-browser" / scenario_id


def _bundle_from_sections(
    *,
    fragment_ids: List[str],
    context: Mapping[str, str],
) -> Dict[str, Any]:
    sections = render_prompt_sections(fragment_ids, context)
    return {
        "fragment_ids": fragment_ids,
        "sections": sections,
        "prompt": "\n\n".join(section["text"] for section in sections),
    }


def _paper_bundle(*, request: Dict[str, Any], repo_path: Path, lane_id: str, raw_output_path: Path, done_path: Path) -> Dict[str, Any]:
    contract = request_contract_block(request, "paper_contract")
    project_invariants = request_project_invariants(request)
    request_summary = contract_request_summary(contract)
    target_covering_nodes = paper_target_covering_nodes(contract)
    previous_own_findings = contract_previous_own_findings_by_lane(contract)
    lane_scoped_contract = _lane_scoped_contract_json(contract, lane_id=lane_id, previous_own_findings=previous_own_findings)
    artifact_delivery = _validated_artifact_instructions(
        repo_path=repo_path,
        raw_output_path=raw_output_path,
        done_path=done_path,
        contract=contract,
        command_context={},
    )
    rubric = contract_required_mapping(contract, "rubric")
    context = {
        "repo_path": str(repo_path),
        "lane_id": lane_id,
        "trellis_scheme_reference_path": str(PROMPT_SCHEME_REFERENCE_PATH),
        "filespec_path": str(_filespec_path(repo_path)),
        "project_invariants_json": _json_fence(project_invariants),
        "request_summary_json": _json_fence(request_summary),
        "target_covering_nodes_json": _json_fence(target_covering_nodes),
        "previous_own_findings_json": _json_fence(previous_own_findings.get(lane_id)),
        "contract_json": _json_fence(lane_scoped_contract),
        "rubric_json": _json_fence(rubric),
        "artifact_delivery_json": _json_fence(artifact_delivery),
        "raw_output_path": artifact_delivery["raw_output_path"],
        "done_path": artifact_delivery["done_path"],
        "json_check_command": artifact_delivery["json_check_command"],
        "acceptance_check_command": artifact_delivery["acceptance_check_command"],
        "acceptance_check_guidance": artifact_delivery["acceptance_check_guidance"],
        "acceptance_check_block": artifact_delivery["acceptance_check_block"],
    }
    return _bundle_from_sections(fragment_ids=contract_prompt_fragments(contract), context=context)


def _corr_bundle(*, request: Dict[str, Any], repo_path: Path, lane_id: str, raw_output_path: Path, done_path: Path) -> Dict[str, Any]:
    contract = request_contract_block(request, "corr_contract")
    project_invariants = request_project_invariants(request)
    request_summary = contract_request_summary(contract)
    previous_own_findings = contract_previous_own_findings_by_lane(contract)
    lane_scoped_contract = _lane_scoped_contract_json(contract, lane_id=lane_id, previous_own_findings=previous_own_findings)
    artifact_delivery = _validated_artifact_instructions(
        repo_path=repo_path,
        raw_output_path=raw_output_path,
        done_path=done_path,
        contract=contract,
        command_context={},
    )
    rubric = contract_required_mapping(contract, "rubric")
    context = {
        "repo_path": str(repo_path),
        "lane_id": lane_id,
        "trellis_scheme_reference_path": str(PROMPT_SCHEME_REFERENCE_PATH),
        "filespec_path": str(_filespec_path(repo_path)),
        "project_invariants_json": _json_fence(project_invariants),
        "request_summary_json": _json_fence(request_summary),
        "previous_own_findings_json": _json_fence(previous_own_findings.get(lane_id)),
        "contract_json": _json_fence(lane_scoped_contract),
        "rubric_json": _json_fence(rubric),
        "artifact_delivery_json": _json_fence(artifact_delivery),
        "raw_output_path": artifact_delivery["raw_output_path"],
        "done_path": artifact_delivery["done_path"],
        "json_check_command": artifact_delivery["json_check_command"],
        "acceptance_check_command": artifact_delivery["acceptance_check_command"],
        "acceptance_check_guidance": artifact_delivery["acceptance_check_guidance"],
        "acceptance_check_block": artifact_delivery["acceptance_check_block"],
    }
    return _bundle_from_sections(fragment_ids=contract_prompt_fragments(contract), context=context)


def _sound_bundle(*, request: Dict[str, Any], repo_path: Path, lane_id: str, node_name: str, raw_output_path: Path, done_path: Path) -> Dict[str, Any]:
    contract = request_contract_block(request, "sound_contract")
    project_invariants = request_project_invariants(request)
    request_summary = contract_request_summary(contract)
    previous_own_findings_per_node = contract_previous_own_findings_per_node(contract)
    flattened_lane_findings = flatten_sound_previous_findings_for_lane(
        previous_own_findings_per_node, lane_id=lane_id
    )
    lane_scoped_contract = _sound_lane_scoped_contract_json(
        contract,
        lane_id=lane_id,
        previous_own_findings_per_node=previous_own_findings_per_node,
        flattened_lane_findings=flattened_lane_findings,
    )
    artifact_delivery = _validated_artifact_instructions(
        repo_path=repo_path,
        raw_output_path=raw_output_path,
        done_path=done_path,
        contract=contract,
        command_context={"node_name": node_name},
    )
    rubric = contract_required_mapping(contract, "rubric")
    context = {
        "repo_path": str(repo_path),
        "lane_id": lane_id,
        "trellis_scheme_reference_path": str(PROMPT_SCHEME_REFERENCE_PATH),
        "filespec_path": str(_filespec_path(repo_path)),
        "project_invariants_json": _json_fence(project_invariants),
        "request_summary_json": _json_fence(request_summary),
        "previous_own_findings_json": _json_fence(
            flattened_lane_findings if flattened_lane_findings else None
        ),
        "contract_json": _json_fence(lane_scoped_contract),
        "rubric_json": _json_fence(rubric),
        "artifact_delivery_json": _json_fence(artifact_delivery),
        "raw_output_path": artifact_delivery["raw_output_path"],
        "done_path": artifact_delivery["done_path"],
        "json_check_command": artifact_delivery["json_check_command"],
        "acceptance_check_command": artifact_delivery["acceptance_check_command"],
        "acceptance_check_guidance": artifact_delivery["acceptance_check_guidance"],
        "acceptance_check_block": artifact_delivery["acceptance_check_block"],
    }
    return _bundle_from_sections(fragment_ids=contract_prompt_fragments(contract), context=context)


def _worker_bundle(
    *,
    request: Dict[str, Any],
    repo_path: Path,
    raw_output_path: Path,
    done_path: Path,
    acceptance_context_path: Path,
    verifier_evidence_path: Path | None,
) -> Dict[str, Any]:
    worker_gate = {"worker_acceptance": request.get("worker_acceptance", {})}
    worker_acceptance = worker_gate_acceptance(worker_gate)
    contract = request_contract_block(request, "worker_contract")
    project_invariants = request_project_invariants(request)
    request_summary = contract_request_summary(contract)
    reviewer_comments = str(contract.get("reviewer_comments", "") or "").strip()
    artifact_delivery = _validated_artifact_instructions(
        repo_path=repo_path,
        raw_output_path=raw_output_path,
        done_path=done_path,
        contract=contract,
        command_context={
            "repo_path": str(repo_path),
            "acceptance_context_path": str(acceptance_context_path),
        },
    )
    context = {
        "repo_path": str(repo_path),
        "trellis_scheme_reference_path": str(PROMPT_SCHEME_REFERENCE_PATH),
        "loogle_helper_path": str(_loogle_helper_path(repo_path)),
        "theorem_initial_dag_size_guidance": "15-50",
        "effective_fresh_context_mode": "fresh" if bool(request.get("fresh_context", False)) else "resume",
        "filespec_path": str(_filespec_path(repo_path)),
        "project_invariants_json": _json_fence(project_invariants),
        "request_summary_json": _json_fence(request_summary),
        "review_verifier_evidence_path": (
            str(verifier_evidence_path)
            if verifier_evidence_path is not None
            else "No request-local verifier evidence sidecar was provided."
        ),
        "reviewer_comments_text": reviewer_comments or "No reviewer comments.",
        "contract_json": _json_fence(contract),
        "acceptance_contract_json": _json_fence(worker_acceptance),
        "artifact_delivery_json": _json_fence(artifact_delivery),
        "raw_output_path": artifact_delivery["raw_output_path"],
        "done_path": artifact_delivery["done_path"],
        "json_check_command": artifact_delivery["json_check_command"],
        "acceptance_check_command": artifact_delivery["acceptance_check_command"],
        "acceptance_check_guidance": artifact_delivery["acceptance_check_guidance"],
        "acceptance_check_block": artifact_delivery["acceptance_check_block"],
    }
    return _bundle_from_sections(fragment_ids=contract_prompt_fragments(contract), context=context)


def _review_bundle(*, request: Dict[str, Any], repo_path: Path, raw_output_path: Path, done_path: Path, context_json_path: Path) -> Dict[str, Any]:
    contract = request_contract_block(request, "review_contract")
    project_invariants = request_project_invariants(request)
    request_summary = contract_request_summary(contract)
    blocker_partition = contract_required_mapping(contract, "blocker_partition")
    verifier_evidence = contract.get("verifier_evidence")
    if not isinstance(verifier_evidence, Mapping):
        raise ValueError("review contract verifier_evidence must be an object")
    blocker_choices = blocker_partition.get("choices")
    if not isinstance(blocker_choices, list):
        raise ValueError("review contract blocker_partition.choices must be a list")
    artifact_delivery = _validated_artifact_instructions(
        repo_path=repo_path,
        raw_output_path=raw_output_path,
        done_path=done_path,
        contract=contract,
        command_context={"context_json_path": str(context_json_path)},
    )
    context = {
        "repo_path": str(repo_path),
        "trellis_scheme_reference_path": str(PROMPT_SCHEME_REFERENCE_PATH),
        "filespec_path": str(_filespec_path(repo_path)),
        "project_invariants_json": _json_fence(project_invariants),
        "request_summary_json": _json_fence(request_summary),
        "deterministic_worker_rejection_reasons_json": _json_fence(request_summary.get("deterministic_worker_rejection_reasons", [])),
        "latest_review_rejection_reasons_json": _json_fence(request_summary.get("latest_review_rejection_reasons", [])),
        "blocker_choices_json": _json_fence(blocker_choices),
        "verifier_evidence_json": _json_fence(verifier_evidence),
        "contract_json": _json_fence(contract),
        "artifact_delivery_json": _json_fence(artifact_delivery),
        "raw_output_path": artifact_delivery["raw_output_path"],
        "done_path": artifact_delivery["done_path"],
        "json_check_command": artifact_delivery["json_check_command"],
        "acceptance_check_command": artifact_delivery["acceptance_check_command"],
        "acceptance_check_guidance": artifact_delivery["acceptance_check_guidance"],
        "acceptance_check_block": artifact_delivery["acceptance_check_block"],
    }
    # Mirror the bridge-prompt path for the source-recourse template
    # variables; the kernel only emits the fragment when both env vars
    # are populated, so the prompt browser stays in sync without
    # special-casing the fragment list.
    snapshot = os.environ.get("TRELLIS_REVIEWER_SOURCE_SNAPSHOT", "").strip()
    source_sha = os.environ.get("TRELLIS_REVIEWER_SOURCE_SHA", "").strip()
    if snapshot and source_sha:
        context["reviewer_source_snapshot_path"] = snapshot
        context["reviewer_source_sha"] = source_sha
    return _bundle_from_sections(fragment_ids=contract_prompt_fragments(contract), context=context)


def list_prompt_scenarios(_repo_path: Path) -> Dict[str, Any]:
    return {
        "roles": ["paper_faithfulness", "substantiveness", "correspondence", "soundness", "worker", "reviewer"],
        "scenarios": [
            {
                "id": scenario.scenario_id,
                "role": scenario.role,
                "title": scenario.title,
                "description": scenario.description,
            }
            for scenario in SCENARIOS
        ],
    }


def render_prompt_scenario(repo_path: Path, scenario_id: str) -> Dict[str, Any]:
    scenario = _scenario_map().get(scenario_id)
    if scenario is None:
        raise KeyError(f"unknown prompt scenario: {scenario_id}")
    request = _kernel_bridge_request_payload(repo_path, scenario.request_factory())
    root = _prompt_root(repo_path, scenario_id)
    raw_output_path = root / f"{scenario_id}.raw.json"
    done_path = root / f"{scenario_id}.done"
    bundle: Dict[str, Any]
    if scenario.role == "paper_faithfulness":
        lane_bindings = request.get("paper_verify_lane_bindings") or []
        lane_id = str((lane_bindings[0] if lane_bindings else {}).get("lane_id") or LANE_1)
        bundle = _paper_bundle(
            request=request,
            repo_path=repo_path,
            lane_id=lane_id,
            raw_output_path=raw_output_path,
            done_path=done_path,
        )
    elif scenario.role == "correspondence":
        lane_bindings = request.get("corr_verify_lane_bindings") or []
        lane_id = str((lane_bindings[0] if lane_bindings else {}).get("lane_id") or LANE_1)
        bundle = _corr_bundle(
            request=request,
            repo_path=repo_path,
            lane_id=lane_id,
            raw_output_path=raw_output_path,
            done_path=done_path,
        )
    elif scenario.role == "soundness":
        lane_bindings = request.get("sound_verify_lane_bindings") or []
        lane_id = str((lane_bindings[0] if lane_bindings else {}).get("lane_id") or LANE_1)
        node_name = str(request.get("sound_verify_node") or TARGET_NODE)
        bundle = _sound_bundle(
            request=request,
            repo_path=repo_path,
            lane_id=lane_id,
            node_name=node_name,
            raw_output_path=raw_output_path,
            done_path=done_path,
        )
    elif scenario.role == "worker":
        raw_verifier_evidence = request.get("review_verifier_evidence", {})
        verifier_evidence = (
            dict(raw_verifier_evidence)
            if isinstance(raw_verifier_evidence, Mapping)
            else {}
        )
        bundle = _worker_bundle(
            request=request,
            repo_path=repo_path,
            raw_output_path=raw_output_path,
            done_path=done_path,
            acceptance_context_path=root / f"{scenario_id}.acceptance_context.json",
            verifier_evidence_path=(
                root / f"{scenario_id}.verifier_evidence.json"
                if verifier_evidence
                else None
            ),
        )
    elif scenario.role == "reviewer":
        bundle = _review_bundle(
            request=request,
            repo_path=repo_path,
            raw_output_path=raw_output_path,
            done_path=done_path,
            context_json_path=root / f"{scenario_id}.context.json",
        )
    else:  # pragma: no cover - scenario table is authoritative
        raise ValueError(f"unsupported prompt-browser role {scenario.role!r}")
    return {
        "scenario": {
            "id": scenario.scenario_id,
            "role": scenario.role,
            "title": scenario.title,
            "description": scenario.description,
        },
        "request": request,
        **bundle,
    }
