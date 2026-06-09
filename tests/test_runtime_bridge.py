from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import pytest

from trellis.adapters import ProviderConfig
from trellis.agent_wrapper.protocol import PanelExecutionResponse, SingleAgentResponse
from trellis.runtime import bridge as bridge_module
from trellis.runtime.bridge import BridgeError, handle_bridge_request
from trellis.runtime.bridge_prompts import (
    build_paper_faithfulness_prompt,
    build_correspondence_prompt,
    build_review_prompt,
    build_soundness_prompt,
    build_stuck_math_audit_prompt,
    build_worker_prompt,
)
from trellis.runtime.bridge_protocol import BridgeCliRequest


def _project_invariants() -> dict[str, object]:
    return {
        "node_pair_contract": "every_present_node_has_lean_and_nl_statement",
        "proof_bearing_contract": "proof_nodes_need_closed_lean_or_rigorous_nl",
        "progress_modes": ["close_proof", "paper_faithful_dag_improvement"],
        "role_authority": {
            "worker": "writes_repository_content_only",
            "reviewer": "chooses_next_step_and_guidance",
            "verifier": "checks_invariants_without_choosing_work",
        },
    }


def _artifact_prompt_view(
    json_command_template: tuple[str, ...] | list[str],
    acceptance_command_template: tuple[str, ...] | list[str] = (),
) -> dict[str, object]:
    return {
        "raw_output_format": "json_only",
        "escape_json_backslashes": True,
        "done_marker_contract": "write_done_after_json_check_passes",
        "checker_authority": "exact_command_is_authoritative",
        "json_check_command_template": list(json_command_template),
        "acceptance_check_command_template": list(acceptance_command_template),
        "failure_recovery": "json_check_required_acceptance_check_best_effort",
        "stdout_policy": "do_not_print_json_to_stdout",
    }


def _blocker_choice_id(blocker: dict[str, object]) -> str:
    kind = str(blocker.get("kind", "") or "").strip().lower()
    obj = blocker.get("object")
    fingerprint = str(blocker.get("fingerprint", "") or "").strip()
    if not isinstance(obj, dict):
        raise AssertionError("test blocker is missing object")
    otype = str(obj.get("otype", "") or "").strip().lower()
    if otype == "node":
        target = str(obj.get("node", "") or "").strip()
    elif otype == "target":
        target = str(obj.get("target", "") or "").strip()
    else:
        raise AssertionError("test blocker has unsupported object type")
    return f"{kind}:{otype}:{target}:{fingerprint}"


def _fake_config(repo_path: Path) -> SimpleNamespace:
    return SimpleNamespace(
        repo_path=repo_path,
        state_dir=repo_path / ".trellis",
        worker=ProviderConfig(provider="codex", model="worker-model"),
        easy_worker=ProviderConfig(provider="gemini", model="easy-worker-model"),
        hard_worker=ProviderConfig(provider="codex", model="hard-worker-model"),
        reviewer=ProviderConfig(provider="codex", model="reviewer-model"),
        verification=SimpleNamespace(
            correspondence_agents=[
                SimpleNamespace(provider="claude", model="corr-a", effort="max", extra_args=[], fallback_models=[], label="claude-a"),
                SimpleNamespace(provider="gemini", model="corr-b", effort=None, extra_args=[], fallback_models=[], label="gemini-b"),
            ],
            soundness_agents=[
                SimpleNamespace(provider="claude", model="snd-a", effort="max", extra_args=[], fallback_models=[], label="claude-a"),
                SimpleNamespace(provider="gemini", model="snd-b", effort=None, extra_args=[], fallback_models=[], label="gemini-b"),
            ],
        ),
        workflow=SimpleNamespace(
            allowed_import_prefixes=["Mathlib"],
            forbidden_keyword_allowlist=[],
            approved_axioms_path=None,
            paper_tex_path=None,
        ),
        tmux=SimpleNamespace(burst_home=None),
        sandbox=None,
        startup_timeout_seconds=30.0,
    )


def test_worker_session_scope_tracks_phase_not_worker_profile() -> None:
    provider = ProviderConfig(provider="codex", model="gpt-5.4", effort="high")
    theorem_scope = bridge_module._session_scope(
        {
            "phase": "theorem_stating",
            "worker_context": {"worker_profile": "theorem"},
        },
        provider,
        "worker",
    )
    proof_scope = bridge_module._session_scope(
        {
            "phase": "proof_formalization",
            "worker_context": {"worker_profile": "proof_hard"},
        },
        provider,
        "worker",
    )
    proof_easy_scope = bridge_module._session_scope(
        {
            "phase": "proof_formalization",
            "worker_context": {"worker_profile": "proof_easy"},
        },
        provider,
        "worker",
    )

    assert theorem_scope != proof_scope
    assert proof_scope == proof_easy_scope
    assert theorem_scope == "theorem_stating:worker:codex:gpt-5.4:high"
    assert proof_scope == "proof_formalization:worker:codex:gpt-5.4:high"


def _fake_policy(
    *,
    corr_selectors: tuple[str, ...] = (),
    sound_selectors: tuple[str, ...] = (),
) -> SimpleNamespace:
    return SimpleNamespace(
        timing=SimpleNamespace(burst_timeout_seconds=120.0),
        prompt_notes=SimpleNamespace(
            initial_theorem_dag_size_min=15,
            initial_theorem_dag_size_max=50,
        ),
        verification=SimpleNamespace(
            correspondence_agent_selectors=corr_selectors,
            soundness_agent_selectors=sound_selectors,
        ),
    )


def _lane_bindings(lanes: list[str], agents: list[SimpleNamespace]) -> list[dict[str, object]]:
    bindings: list[dict[str, object]] = []
    for lane_id, agent in zip(sorted(str(item) for item in lanes), agents):
        bindings.append(
            {
                "lane_id": lane_id,
                "provider": str(agent.provider),
                "model": getattr(agent, "model", None),
                "effort": getattr(agent, "effort", None),
                "extra_args": list(getattr(agent, "extra_args", []) or []),
                "fallback_models": list(getattr(agent, "fallback_models", []) or []),
                "label": str(getattr(agent, "label", "") or ""),
            }
        )
    return bindings


def _provider_binding(config: ProviderConfig | None) -> dict[str, object]:
    if config is None:
        return {
            "provider": "",
            "model": None,
            "effort": None,
            "extra_args": [],
            "fallback_models": [],
            "label": "",
        }
    return {
        "provider": str(config.provider),
        "model": config.model,
        "effort": config.effort,
        "extra_args": list(config.extra_args),
        "fallback_models": list(config.fallback_models),
        "label": "",
    }


def _kernel_bound_request(
    config: SimpleNamespace,
    request: dict[str, object],
) -> dict[str, object]:
    bound = dict(request)
    kind = str(bound.get("kind", "") or "").strip().lower()
    bound.setdefault("project_invariants", _project_invariants())
    bound.setdefault("runtime_support_required", kind in {"worker", "paper", "corr", "sound", "review"})
    bound.setdefault("worker_binding", _provider_binding(None))
    bound.setdefault("reviewer_binding", _provider_binding(None))
    if kind == "worker":
        worker_context = bound.get("worker_context")
        profile = ""
        if isinstance(worker_context, dict):
            profile = str(worker_context.get("worker_profile", "") or "").strip().lower()
        if profile == "proof_easy" and getattr(config, "easy_worker", None) is not None:
            binding = config.easy_worker
        elif profile in {"proof_hard", "cleanup"} and getattr(config, "hard_worker", None) is not None:
            binding = config.hard_worker
        else:
            binding = config.worker
        bound["worker_binding"] = _provider_binding(binding)
    elif kind == "review":
        bound["reviewer_binding"] = _provider_binding(config.reviewer)
    return bound


def _fake_kernel_cli_for_verifier_normalization(payload: dict[str, object]) -> dict[str, object]:
    action = payload.get("action")
    input_data = payload.get("input")
    assert isinstance(input_data, dict)
    if action == "normalize_paper":
        verify_targets = [str(item) for item in input_data.get("verify_targets", [])]
        target_lane_updates: dict[str, dict[str, dict[str, str]]] = {}
        for member in input_data.get("members", []):
            assert isinstance(member, dict)
            lane_id = str(member.get("lane_id", ""))
            if not member.get("ok", False):
                raise AssertionError(f"unexpected failed paper lane in test stub: {lane_id}")
            lane_payload = member.get("payload")
            assert isinstance(lane_payload, dict)
            faith = lane_payload.get("paper_faithfulness") or {}
            target_issues = {
                str(item.get("node", "") or "").strip()
                for item in faith.get("issues", [])
                if isinstance(item, dict)
            }
            target_lane_updates[lane_id] = {
                target: {"Set": "Fail" if target in target_issues else "Pass"}
                for target in verify_targets
            }
        return {
            "status": "normalize_paper_ok",
            "output": {
                "response": {
                    "request_id": input_data.get("request_id"),
                    "cycle": input_data.get("cycle"),
                    "status": "Ok",
                    "target_lane_updates": target_lane_updates,
                }
            },
        }
    if action == "normalize_corr":
        verify_nodes = [str(item) for item in input_data.get("verify_nodes", [])]
        node_lane_updates: dict[str, dict[str, dict[str, str]]] = {}
        for member in input_data.get("members", []):
            assert isinstance(member, dict)
            lane_id = str(member.get("lane_id", ""))
            if not member.get("ok", False):
                raise AssertionError(f"unexpected failed corr lane in test stub: {lane_id}")
            lane_payload = member.get("payload")
            assert isinstance(lane_payload, dict)
            corr = lane_payload.get("correspondence") or {}
            corr_issues = {
                str(item.get("node", "") or "").strip()
                for item in corr.get("issues", [])
                if isinstance(item, dict)
            }
            node_lane_updates[lane_id] = {
                node: {
                    "Set": (
                        "Fail"
                        if node in corr_issues
                        or (node == "Preamble" and any(issue.startswith("Preamble[") for issue in corr_issues))
                        else "Pass"
                    )
                }
                for node in verify_nodes
            }
        return {
            "status": "normalize_corr_ok",
            "output": {
                "response": {
                    "request_id": input_data.get("request_id"),
                    "cycle": input_data.get("cycle"),
                    "status": "Ok",
                    "node_lane_updates": node_lane_updates,
                    "target_lane_updates": {},
                }
            },
        }
    if action == "normalize_sound":
        verify_nodes = [str(item) for item in input_data.get("verify_nodes", [])]
        lane_updates: dict[str, dict[str, dict[str, str]]] = {}
        if not verify_nodes:
            for lane_id in [str(item) for item in input_data.get("verify_lanes", [])]:
                lane_updates[lane_id] = {}
            return {
                "status": "normalize_sound_ok",
                "output": {
                    "response": {
                        "request_id": input_data.get("request_id"),
                        "cycle": input_data.get("cycle"),
                        "status": "Ok",
                        "lane_updates": lane_updates,
                    }
                },
            }
        assert len(verify_nodes) == 1
        node_name = verify_nodes[0]
        for member in input_data.get("members", []):
            assert isinstance(member, dict)
            lane_id = str(member.get("lane_id", ""))
            if not member.get("ok", False):
                raise AssertionError(f"unexpected failed sound lane in test stub: {lane_id}")
            lane_payload = member.get("payload")
            assert isinstance(lane_payload, dict)
            soundness = lane_payload.get("soundness") or {}
            decision = str(soundness.get("decision", "") or "").strip().upper()
            status = "Pass" if decision == "SOUND" else "Structural" if decision == "STRUCTURAL" else "Fail"
            lane_updates[lane_id] = {node_name: {"Set": status}}
        return {
            "status": "normalize_sound_ok",
            "output": {
                "response": {
                    "request_id": input_data.get("request_id"),
                    "cycle": input_data.get("cycle"),
                    "status": "Ok",
                    "lane_updates": lane_updates,
                }
            },
        }
    raise AssertionError(f"unexpected test kernel action: {action!r}")


def _worker_acceptance(
    validation_kind: str,
    authorized_nodes: list[str] | tuple[str, ...],
) -> dict[str, object]:
    active_placeholder = list(authorized_nodes)[0] if authorized_nodes else "n1"
    if validation_kind == "theorem_targeted":
        validation_execution_plan = [
            {
                "kind": "theorem_target_edit_scope",
                "target": active_placeholder,
                "initial_scope": list(authorized_nodes),
            },
            {
                "kind": "scoped_tablet",
                "allowed_nodes_mode": "previous_or_explicit",
                "explicit_nodes": list(authorized_nodes),
            },
        ]
        observation_plan = {
            "capture_before_snapshot": True,
            "capture_scoped_tablet_baseline_errors": True,
            "scoped_tablet_baseline_scope": "authorized_nodes",
            "capture_imports_before": False,
            "capture_expected_active_hash": False,
            "capture_baseline_declaration_hashes": False,
            "capture_baseline_correspondence_hashes": False,
        }
    elif validation_kind == "theorem_global":
        validation_execution_plan = [
            {
                "kind": "scoped_tablet",
                "allowed_nodes_mode": "all_present",
                "explicit_nodes": [],
            }
        ]
        observation_plan = {
            "capture_before_snapshot": True,
            "capture_scoped_tablet_baseline_errors": True,
            "scoped_tablet_baseline_scope": "all_present",
            "capture_imports_before": False,
            "capture_expected_active_hash": False,
            "capture_baseline_declaration_hashes": False,
            "capture_baseline_correspondence_hashes": False,
        }
    elif validation_kind == "proof_easy":
        validation_execution_plan = [
            {"kind": "proof_easy_scope", "active_node": active_placeholder}
        ]
        observation_plan = {
            "capture_before_snapshot": True,
            "capture_scoped_tablet_baseline_errors": False,
            "scoped_tablet_baseline_scope": "none",
            "capture_imports_before": True,
            "capture_expected_active_hash": False,
            "capture_baseline_declaration_hashes": False,
            "capture_baseline_correspondence_hashes": False,
        }
    else:
        validation_execution_plan = (
            [{"kind": "cleanup_preserving"}]
            if validation_kind == "cleanup"
            else [{"kind": "final_cleanup_preserving"}]
            if validation_kind == "final_cleanup"
            else [
                {
                    "kind": "proof_worker_delta",
                    "active_node": active_placeholder,
                    "mode": (
                        "coarse_restructure"
                        if validation_kind == "proof_coarse_restructure"
                        else validation_kind.removeprefix("proof_")
                    ),
                    "authorized_nodes": list(authorized_nodes),
                }
            ]
        )
        observation_plan = {
            "capture_before_snapshot": True,
            "capture_scoped_tablet_baseline_errors": False,
            "scoped_tablet_baseline_scope": "none",
            "capture_imports_before": False,
            "capture_expected_active_hash": validation_kind in {
                "proof_local",
                "proof_restructure",
                "proof_coarse_restructure",
            },
            "capture_baseline_declaration_hashes": validation_kind in {"cleanup", "final_cleanup"},
            "capture_baseline_correspondence_hashes": validation_kind in {"cleanup", "final_cleanup"},
        }
    return {
        "validation_kind": validation_kind,
        "authorized_nodes": list(authorized_nodes),
        "validation_execution_plan": validation_execution_plan,
        "require_explicit_semantic_deps_for_new_nodes": True,
        "require_explicit_semantic_deps_for_changed_direct_deps": True,
        "require_explicit_target_claims_for_new_nodes": True,
        "forbid_tablet_changes_when_stuck": True,
        "observation_plan": observation_plan,
    }


def _corr_contract(
    *,
    verify_nodes: list[str] | tuple[str, ...],
    verify_targets: list[str] | tuple[str, ...],
    phase: str = "theorem_stating",
    blocked_targets: list[str] | tuple[str, ...] = (),
    preamble_item_ids: list[str] | tuple[str, ...] = (),
    preamble_items: list[dict[str, object]] | None = None,
) -> dict[str, object]:
    return {
        "prompt_fragments": [
            "common/TRELLIS_FORMALIZATION_SCHEME.md",
            "verifier/common/00_intro.md",
            "shared/10_repository_root.md",
            "verifier/common/10_lane_id.md",
            "verifier/common/15_previous_findings.md",
            "shared/20_read_files.md",
            "shared/25_filespec.md",
            "shared/30_project_invariants.md",
            "verifier/correspondence/07_scratchpad.md",
            "verifier/correspondence/20_frontier.md",
            "verifier/correspondence/30_contract.md",
            "verifier/correspondence/40_rubric.md",
            "verifier/correspondence/50_authority.md",
            "shared/90_artifact_delivery.md",
        ],
        "request_summary": {
            "phase": phase,
            "nodes": list(verify_nodes),
            "blocked_targets": list(blocked_targets),
        },
        "previous_own_findings_by_lane": {},
        "issue_reporting_policy": "current_failures_only",
        "fixed_item_reporting_policy": "summary_only",
        "node_issue_scope": list(verify_nodes),
        "rubric": {
            "statement_alignment_checks": [
                "quantifier_scope",
                "type_constraints",
                "implicit_assumptions",
                "domain_context",
            ],
            "project_definition_policy": "expand_project_definitions_but_trust_mathlib",
            "definition_hygiene": [
                "reject_opaque",
                "reject_axiom",
                "reject_constant",
                "reject_sorry_in_definition",
            ],
            "duplicate_mathlib_definition_policy": "reject_project_duplicates",
            "preamble_item_issue_policy": "use_exact_item_id",
        },
        "artifact_contract": {
            "result_type": "correspondence_result_v1",
            "overall_rule": "approve_iff_pass",
            "prompt_schema_example": {
                "correspondence": {
                    "decision": "PASS or FAIL",
                    "issues": [{"node": "node_id", "description": "..."}],
                },
                "overall": "APPROVE or REJECT",
                "summary": "brief overall summary",
                "comments": "optional short note",
            },
            "phase_blocks": {
                "correspondence": {
                    "decision_values": ["PASS", "FAIL"],
                    "issue_subject_kind": "node",
                },
            },
        },
        "artifact_prompt_view": _artifact_prompt_view((
            "python3",
            "{{check_script_path}}",
            "correspondence-result",
            "{{raw_output_path}}",
        )),
        "preamble_contract": {
            "mode": "one_way_support" if "Preamble" in verify_nodes else "none",
            "item_ids": list(preamble_item_ids),
            "items": list(preamble_items or []),
            "empty_items_vacuously_supported": True,
        },
    }


def _paper_contract(
    *,
    verify_targets: list[str] | tuple[str, ...],
    phase: str = "theorem_stating",
    blocked_targets: list[str] | tuple[str, ...] = (),
    target_covering_nodes: dict[str, list[str] | tuple[str, ...]] | None = None,
) -> dict[str, object]:
    covering_nodes = (
        {str(target): [f"{target}_cover"] for target in verify_targets}
        if target_covering_nodes is None
        else {str(target): list(nodes) for target, nodes in target_covering_nodes.items()}
    )
    return {
        "prompt_fragments": [
            "common/TRELLIS_FORMALIZATION_SCHEME.md",
            "verifier/common/00_intro.md",
            "shared/10_repository_root.md",
            "verifier/common/10_lane_id.md",
            "verifier/common/15_previous_findings.md",
            "shared/20_read_files.md",
            "shared/25_filespec.md",
            "shared/30_project_invariants.md",
            "verifier/paper_faithfulness/20_targets.md",
            "verifier/paper_faithfulness/30_contract.md",
            "verifier/paper_faithfulness/40_rubric.md",
            "verifier/paper_faithfulness/50_authority.md",
            "shared/90_artifact_delivery.md",
        ],
        "request_summary": {
            "phase": phase,
            "targets": list(verify_targets),
            "blocked_targets": list(blocked_targets),
        },
        "target_covering_nodes": covering_nodes,
        "previous_own_findings_by_lane": {},
        "issue_reporting_policy": "current_failures_only",
        "fixed_item_reporting_policy": "summary_only",
        "rubric": {
            "paper_statement_authority": "configured_target_ids_label_first",
            "covering_set_authority": "covering_nodes_for_target_claim",
            "definition_dependency_authority": "statement_level_definition_hashes_only",
            "faithfulness_standard": "covering_tex_statements_collectively_capture_target_statement",
        },
        "artifact_contract": {
            "result_type": "paper_faithfulness_result_v1",
            "overall_rule": "approve_iff_pass",
            "prompt_schema_example": {
                "paper_faithfulness": {
                    "decision": "PASS or FAIL",
                    "issues": [{"node": "target_id", "description": "..."}],
                },
                "overall": "APPROVE or REJECT",
                "summary": "brief overall summary",
                "comments": "optional short note",
            },
            "phase_blocks": {
                "paper_faithfulness": {
                    "decision_values": ["PASS", "FAIL"],
                    "issue_subject_kind": "target",
                },
            },
        },
        "artifact_prompt_view": _artifact_prompt_view((
            "python3",
            "{{check_script_path}}",
            "paper-faithfulness-result",
            "{{raw_output_path}}",
        )),
    }


# Test-local mirrors of the kernel's per-blocker formatting helpers. The
# bridge used to host these before the 2026-06-04 bridge-to-kernel migration
# batch 2 deleted them; tests still need a Python implementation to synthesize
# kernel-shaped `blocker_status` / `blocker_choices_block` payloads without
# spinning up the kernel binary. Keep these byte-equivalent to the kernel
# helpers in `kernel/src/request_contracts.rs` (`worker_blocker_*` family).
_TEST_BLOCKER_INLINE_LIMIT_DEFAULT = 8
_TEST_BLOCKER_INLINE_LIMIT_ENV = "TRELLIS_BLOCKER_INLINE_LIMIT"
_TEST_BLOCKER_ACTIONABLE_FALLBACK_K = 5


def _test_blocker_inline_limit() -> int:
    raw = os.environ.get(_TEST_BLOCKER_INLINE_LIMIT_ENV, "").strip()
    if raw:
        try:
            return max(0, int(raw))
        except ValueError:
            pass
    return _TEST_BLOCKER_INLINE_LIMIT_DEFAULT


def _test_blocker_object_label(blocker: dict[str, object]) -> str:
    obj = blocker.get("object") if isinstance(blocker.get("object"), dict) else {}
    otype = str(obj.get("otype") or "?")
    body = (
        obj.get("node")
        or obj.get("target")
        or obj.get("id")
        or obj.get("name")
        or "?"
    )
    return f"{otype}:{body}"


def _test_blocker_kind_counts_line(blocker_choices: list[dict[str, object]]) -> str:
    from collections import Counter

    kind_counts: Counter[str] = Counter()
    for choice in blocker_choices:
        if not isinstance(choice, dict):
            kind_counts["?"] += 1
            continue
        blocker = choice.get("blocker") if isinstance(choice.get("blocker"), dict) else {}
        kind_counts[str(blocker.get("kind") or "?")] += 1
    if not kind_counts:
        return "(none)"
    return ", ".join(f"{k}={v}" for k, v in sorted(kind_counts.items()))


def _test_blocker_format_row(
    index: int, choice: dict[str, object], *, include_id: bool = False
) -> str:
    blocker = choice.get("blocker") if isinstance(choice.get("blocker"), dict) else {}
    kind = str(blocker.get("kind") or "?")
    label = _test_blocker_object_label(blocker)
    if include_id:
        bid = choice.get("id") if isinstance(choice.get("id"), str) else "?"
        return f"{index:5d} | {kind:16s} | {label:32s} | id={bid}"
    return f"{index:5d} | {kind:16s} | {label}"


def _test_blocker_select_actionable(
    blocker_choices: list[dict[str, object]],
    *,
    active_node: str | None,
    held_target: str | None,
    deps_neighborhood: list[str] | None,
) -> tuple[list[int], str]:
    neighborhood: set[str] = set()
    if active_node:
        neighborhood.add(active_node)
    if deps_neighborhood:
        neighborhood.update(str(n) for n in deps_neighborhood if isinstance(n, str))
    target_focus: set[str] = set()
    if held_target:
        target_focus.add(held_target)

    matched: list[int] = []
    for index, choice in enumerate(blocker_choices):
        if not isinstance(choice, dict):
            continue
        blocker = choice.get("blocker")
        if not isinstance(blocker, dict):
            continue
        obj = blocker.get("object") if isinstance(blocker.get("object"), dict) else {}
        otype = obj.get("otype")
        if otype == "node":
            node = obj.get("node")
            if isinstance(node, str) and node in neighborhood:
                matched.append(index)
        elif otype == "target":
            target = obj.get("target")
            if isinstance(target, str) and target in target_focus:
                matched.append(index)

    if matched:
        return matched, "active node + direct-dep neighborhood"

    fallback_count = min(_TEST_BLOCKER_ACTIONABLE_FALLBACK_K, len(blocker_choices))
    if fallback_count == 0:
        return [], "no live blockers"
    sortable: list[tuple[str, int]] = []
    for index, choice in enumerate(blocker_choices):
        if not isinstance(choice, dict):
            sortable.append(("?", index))
            continue
        blocker = choice.get("blocker") if isinstance(choice.get("blocker"), dict) else {}
        sortable.append((_test_blocker_object_label(blocker), index))
    sortable.sort()
    return [idx for _, idx in sortable[:fallback_count]], (
        f"fallback: no blockers touch active_node/held_target; showing "
        f"first {fallback_count} of {len(blocker_choices)} by label order"
    )


def _test_blocker_format_actionable_table(
    indices: list[int],
    blocker_choices: list[dict[str, object]],
    *,
    note: str,
    include_id: bool = True,
) -> str:
    if not indices:
        return f"(none) -- {note}"
    rows: list[str] = []
    for index in indices:
        if 0 <= index < len(blocker_choices):
            choice = blocker_choices[index]
            if isinstance(choice, dict):
                rows.append(_test_blocker_format_row(index, choice, include_id=include_id))
    if include_id:
        header_lines = [
            "Index | Kind             | otype:body                       | id",
            "------|------------------|----------------------------------|" + "-" * 40,
        ]
    else:
        header_lines = [
            "Index | Kind             | Object",
            "------|------------------|" + "-" * 32,
        ]
    return "\n".join(
        [
            f"Actionable subset ({len(rows)} of {len(blocker_choices)}): {note}",
            "",
            *header_lines,
            *rows,
        ]
    )


def _synth_worker_blocker_status_for_test(
    blockers: list[dict[str, object]],
    *,
    active_node: str | None,
    held_target: str | None,
    current_deps: dict[str, object],
) -> dict[str, object]:
    """Synthesize a `worker_contract["blocker_status"]` for tests.

    The kernel emits this field via `worker_blocker_status_block` in
    production (see `kernel/src/request_contracts.rs`). Tests construct a
    `worker_contract` dict directly without going through the kernel, so we
    reproduce the kernel's logic in Python here using the test-local
    `_test_blocker_*` helpers above. The output shape matches
    `WorkerBlockerStatusBlock`: `{"md": str, "sidecar_payload": dict | None}`.
    The bridge substitutes `{sidecar_path}` in `md` before splicing.
    """
    if not blockers:
        return {"md": "No live blockers.", "sidecar_payload": None}
    synth_choices: list[dict[str, object]] = [
        {
            "id": "(worker view: id only emitted in review contract)",
            "blocker": dict(b),
        }
        for b in blockers
        if isinstance(b, dict)
    ]
    # Compute deps_neighborhood the same way the kernel does: direct
    # out-edges of active_node + reverse-edges (consumers of active_node).
    deps_neighborhood: list[str] = []
    if isinstance(active_node, str) and isinstance(current_deps, dict):
        direct = current_deps.get(active_node)
        if isinstance(direct, list):
            deps_neighborhood.extend(str(n) for n in direct if isinstance(n, str))
        for node, deps in current_deps.items():
            if isinstance(deps, list) and active_node in deps and isinstance(node, str):
                deps_neighborhood.append(node)
    total = len(synth_choices)
    counts_line = _test_blocker_kind_counts_line(synth_choices)
    indices, note = _test_blocker_select_actionable(
        synth_choices,
        active_node=active_node,
        held_target=held_target,
        deps_neighborhood=deps_neighborhood or None,
    )
    actionable_table = _test_blocker_format_actionable_table(
        indices, synth_choices, note=note, include_id=False
    )
    header = (
        f"{total} live blocker(s). Counts by kind: {counts_line}. "
        "Reviewer comments above describe what to repair; this list shows "
        "the live verifier blockers for situational awareness."
    )
    inline_limit = _test_blocker_inline_limit()
    if total <= inline_limit:
        rows = [
            _test_blocker_format_row(index, choice)
            for index, choice in enumerate(synth_choices)
        ]
        md = "\n".join(
            [
                header,
                "",
                "Index | Kind             | Object",
                "------|------------------|" + "-" * 32,
                *rows,
                "",
                actionable_table,
            ]
        )
        return {"md": md, "sidecar_payload": None}
    md = "\n".join(
        [
            header,
            "",
            (
                f"Live blocker count ({total}) exceeds the inline limit "
                f"({inline_limit}); full structured blocker list is on disk so "
                "the actionable subset stays visible inline."
            ),
            "",
            actionable_table,
            "",
            "Full blocker list sidecar: `{sidecar_path}`",
            "",
            (
                "Sidecar shape: `{\"blocker_choices\": [{\"id\": ..., "
                "\"blocker\": ...}, ...]}`. Workers do not echo blocker `id`s "
                "back; this file exists so you can inspect any blocker beyond "
                "the inline actionable subset if the reviewer's comments "
                "reference it."
            ),
        ]
    )
    return {
        "md": md,
        "sidecar_payload": {"blocker_choices": synth_choices},
    }


def _synth_review_blocker_choices_block_for_test(
    blocker_choices: list[dict[str, object]],
    *,
    active_node: str | None,
    held_target: str | None,
) -> dict[str, object]:
    """Synthesize a `review_contract["blocker_choices_block"]` for tests.

    Mirrors `_synth_worker_blocker_status_for_test` for the reviewer-side
    block. The kernel emits this field via `review_blocker_choices_block`
    in production (see `kernel/src/request_contracts.rs`). Tests construct
    a `review_contract` dict directly without going through the kernel, so
    we reproduce the kernel's logic in Python here using the test-local
    `_test_blocker_*` helpers above. The bridge substitutes `{sidecar_path}`
    and `{context_json_path}` in `md` before splicing.
    """
    total = len(blocker_choices)
    counts_line = _test_blocker_kind_counts_line(blocker_choices)
    # Reviewer side has no deps_neighborhood (the review request_summary does
    # not carry the DAG `current_deps` map). The kernel handles this the same
    # way, passing `None` for the neighborhood.
    indices, note = _test_blocker_select_actionable(
        blocker_choices,
        active_node=active_node,
        held_target=held_target,
        deps_neighborhood=None,
    )
    actionable_table = _test_blocker_format_actionable_table(
        indices, blocker_choices, note=note, include_id=True
    )
    header = f"{total} blocker choices total. Counts by kind: {counts_line}"
    inline_limit = _test_blocker_inline_limit()

    if total <= inline_limit:
        rows = [
            _test_blocker_format_row(index, choice)
            for index, choice in enumerate(blocker_choices)
            if isinstance(choice, dict)
        ]
        lines: list[str] = [
            header,
            "",
            "Index | Kind             | Object",
            "------|------------------|" + "-" * 32,
            *rows,
        ]
        if total > 0:
            lines.extend(["", actionable_table])
        lines.extend(
            [
                "",
                "Full structured blocker data (with the fingerprint-encoded `id`",
                "field that you must echo back verbatim if you select a blocker)",
                "lives at: {context_json_path}",
                "",
                "List every blocker `id`:",
                "",
                "  jq -r '.review_blocker_choices[].id' {context_json_path}",
                "",
                "Read one full blocker by index:",
                "",
                "  jq '.review_blocker_choices[N]' {context_json_path}",
            ]
        )
        return {"md": "\n".join(lines), "sidecar_payload": None}

    # Overflow path: the bridge writes the sidecar from `sidecar_payload`;
    # the kernel-rendered md substitutes `{sidecar_path}` at splice time.
    lines = [
        header,
        "",
        (
            f"Live blocker count ({total}) exceeds the inline limit "
            f"({inline_limit}); full structured blocker list is moved to a "
            "sidecar so the actionable subset stays visible inline."
        ),
        "",
        actionable_table,
        "",
        "Full blocker list sidecar: `{sidecar_path}`",
        "",
        (
            "The sidecar JSON has the shape "
            "`{\"blocker_choices\": [{\"id\": ..., \"blocker\": ...}, ...]}`. "
            "Use blocker `id`s for task/override/reset lists; use node ids for "
            "`request_sound_verifier_node_ids`."
        ),
        "",
        "List every blocker `id` from the sidecar:",
        "",
        "  jq -r '.blocker_choices[].id' {sidecar_path}",
        "",
        "Read one full blocker by index:",
        "",
        "  jq '.blocker_choices[N]' {sidecar_path}",
        "",
        "The original kernel context.json also has the same data under "
        "`.review_blocker_choices`, mirrored at {context_json_path}.",
    ]
    return {
        "md": "\n".join(lines),
        "sidecar_payload": {"blocker_choices": blocker_choices},
    }


def _worker_contract(
    *,
    authorized_nodes: list[str] | tuple[str, ...],
    phase: str = "theorem_stating",
    mode: str = "targeted",
    active_node: str | None = None,
    held_target: str | None = None,
    worker_context: dict[str, object] | None = None,
    blockers: list[dict[str, object]] | None = None,
    protected_nodes: list[str] | tuple[str, ...] = (),
    current_present_nodes: list[str] | tuple[str, ...] = (),
    current_proof_nodes: list[str] | tuple[str, ...] = (),
    current_deps: dict[str, object] | None = None,
    current_semantic_deps: dict[str, object] | None = None,
    current_target_claims: dict[str, object] | None = None,
    configured_targets: list[str] | tuple[str, ...] = (),
    blocked_targets: list[str] | tuple[str, ...] = (),
    forbid_tablet_changes_when_stuck: bool = True,
    reviewer_comments: str = "",
    deterministic_worker_rejection_reasons: list[str] | tuple[str, ...] = (),
    invalid_attempt: bool = False,
) -> dict[str, object]:
    # B7: include 33_routing_hints only when at least one hint is non-default.
    has_routing_hints = bool(worker_context) and (
        worker_context.get("next_context_mode") not in (None, "resume")
        or worker_context.get("paper_focus_ranges")
        or worker_context.get("work_style_hint") not in (None, "none")
    )
    prompt_fragments = [
        "common/TRELLIS_FORMALIZATION_SCHEME.md",
        "worker/theorem_stating/00_intro.md",
        "shared/10_repository_root.md",
        "shared/20_read_files.md",
        "worker/common/15_loogle.md",
        "shared/25_filespec.md",
        "shared/30_project_invariants.md",
        "worker/common/20_authority.md",
        "worker/common/30_request.md",
        "worker/common/31_scratchpad.md",
        "worker/common/34_verifier_evidence.md",
    ]
    if has_routing_hints:
        prompt_fragments.append("worker/common/33_routing_hints.md")
    prompt_fragments.extend([
        "worker/theorem_stating/10_mode_guidance.md",
        "worker/theorem_stating/15_initial_dag_size.md",
        "worker/theorem_stating/20_common_failure_modes.md",
        "worker/common/35_reviewer_comments.md",
        "worker/common/37_field_guidance.md",
        "worker/common/40_contract.md",
        "worker/common/45_outcomes.md",
        "worker/common/50_acceptance.md",
        "shared/90_artifact_delivery.md",
        "worker/common/95_gate_authority.md",
    ])
    if invalid_attempt:
        prompt_fragments.insert(10, "worker/common/31_last_invalid.md")
    if deterministic_worker_rejection_reasons:
        prompt_fragments.insert(10, "worker/common/32_deterministic_worker_rejection.md")
    # A2: pending_targets is omitted when blocked_targets is empty.
    scope_contract: dict[str, object] = {
        "existing_node_scope_mode": "authorized_existing_nodes",
        "authorized_existing_nodes": list(authorized_nodes),
        "configured_targets": list(configured_targets),
        "new_nodes_allowed": True,
    }
    if blocked_targets:
        scope_contract["pending_targets"] = list(blocked_targets)
        scope_contract["pending_targets_meaning"] = (
            "targets_lacking_current_approved_support"
        )
    return {
        "prompt_fragments": prompt_fragments,
        "request_summary": {
            "phase": phase,
            "mode": mode,
            "active_node": active_node,
            "held_target": held_target,
            "fresh_context": False,
            "deterministic_worker_rejection_reasons": list(
                deterministic_worker_rejection_reasons
            ),
            "worker_context": dict(worker_context or {}),
            "blockers": list(blockers or []),
            "protected_nodes": list(protected_nodes),
            "current_present_nodes": list(current_present_nodes),
            "current_proof_nodes": list(current_proof_nodes),
            "current_deps": dict(current_deps or {}),
            "current_semantic_deps": dict(current_semantic_deps or {}),
            "current_target_claims": dict(current_target_claims or {}),
        },
        # Migration 2026-06-02: the worker prompt's blocker-status block is
        # rendered by the kernel and exposed via this field. In production
        # `worker_contract_payload` populates it; tests construct the
        # contract by hand, so we replicate that synthesis here using the
        # test-local `_test_blocker_*` helpers (mirrors of the kernel's
        # `worker_blocker_*` family). The bridge no longer hosts these.
        "blocker_status": _synth_worker_blocker_status_for_test(
            list(blockers or []),
            active_node=active_node,
            held_target=held_target,
            current_deps=dict(current_deps or {}),
        ),
        "reviewer_comments": reviewer_comments,
        "result_type": "worker_result_v1",
        "kernel_derives_structural_snapshot": True,
        "allowed_outcomes": ["valid", "invalid", "stuck", "needs_restructure"],
        # B3: semantic_dep_updates is dead protocol; A6: forbidden_legacy_fields
        # was a 2-year-old migration relic.
        "reported_delta_fields": [
            "target_claim_updates",
            "difficulty_updates",
        ],
        "prompt_schema_example": {
            "outcome": "valid / invalid / stuck / needs_restructure",
            "summary": "brief summary",
            "comments": "optional short note",
            "target_claim_updates": {"node_id": ["target_id"]},
            "difficulty_updates": {"node_id": "easy or hard"},
        },
        "scope_contract": scope_contract,
        "stuck_contract": {
            "allowed": True,
            "forbid_tablet_changes_when_stuck": forbid_tablet_changes_when_stuck,
            "meaning": "cannot_make_progress_on_pending_work_under_current_scope",
        },
        "needs_restructure_contract": {
            "allowed": True,
            "forbid_tablet_changes_when_needs_restructure": False,
            "meaning": "worker_can_name_broader_restructure_needed_but_current_scope_does_not_authorize_it",
        },
        "artifact_prompt_view": _artifact_prompt_view(
            (
                "python3",
                "{{check_script_path}}",
                "trellis-worker-result",
                "{{raw_output_path}}",
            ),
            (
            "python3",
            "{{check_script_path}}",
            "trellis-worker-result",
            "{{raw_output_path}}",
            "--repo",
            "{{repo_path}}",
            "--context-json",
            "{{acceptance_context_path}}",
            ),
        ),
    }


def _review_contract(
    *,
    blocker_choices: list[dict[str, object]],
    phase: str = "theorem_stating",
    mode: str = "global",
    active_node: str | None = None,
    held_target: str | None = None,
    invalid_attempt: bool = False,
    retry_outcome_kind: str = "None",
    retry_attempt: int = 0,
    blocked_targets: list[str] | tuple[str, ...] = (),
    protected_nodes: list[str] | tuple[str, ...] = (),
    deterministic_worker_rejection_reasons: list[str] | tuple[str, ...] = (),
    latest_review_rejection_reasons: list[str] | tuple[str, ...] = (),
    latest_worker_summary: str = "",
    latest_worker_comments: str = "",
    allowed_decisions: list[str] | tuple[str, ...] = ("continue", "need_input"),
    allowed_reset_ids: list[str] | tuple[str, ...] = (),
    # Option C (2026-06-04): `allowed_override_ids` retained as a kwarg
    # default so old call sites compile, but the synthesized contract no
    # longer surfaces the field. The kernel's real `review_contract_payload`
    # also no longer emits it.
    allowed_override_ids: list[str] | tuple[str, ...] = (),  # noqa: ARG001 (kept for callsite compat)
    allowed_next_modes: list[str] | tuple[str, ...] = ("global", "targeted"),
    allowed_resets: list[str] | tuple[str, ...] = ("none",),
    allowed_next_nodes: list[str] | tuple[str, ...] = (),
    targeted_allowed_nodes: list[str] | tuple[str, ...] = (),
    allow_targeted_without_next_active: bool = False,
    allowed_difficulty_update_nodes: list[str] | tuple[str, ...] = (),
    human_input_outstanding: bool = False,
    verifier_evidence: dict[str, object] | None = None,
) -> dict[str, object]:
    prompt_fragments = [
        "common/TRELLIS_FORMALIZATION_SCHEME.md",
        "review/common/00_intro.md",
        "shared/10_repository_root.md",
        "shared/20_read_files.md",
        "shared/25_filespec.md",
        "shared/30_project_invariants.md",
        "review/common/10_request.md",
        "review/common/12_deterministic_worker_rejection.md",
        "review/common/20_blocker_choices.md",
        "review/common/25_verifier_reasoning.md",
        "review/common/30_contract.md",
        "review/common/31_need_input.md",
        "review/common/32_revert.md",
        "review/common/33_routing_hints.md",
        "review/common/34_worker_context_strategy.md",
        "review/common/35_comments.md",
    ]
    if phase == "proof_formalization":
        prompt_fragments.append("review/common/37_restructure_strategy.md")
    prompt_fragments.extend(
        [
            "review/common/38_paper_focus_strategy.md",
            "review/common/39_revert_strategy.md",
            "review/common/40_authority.md",
            "shared/90_artifact_delivery.md",
        ]
    )
    return {
        "prompt_fragments": prompt_fragments,
        "request_summary": {
            "phase": phase,
            "mode": mode,
            "active_node": active_node,
            "held_target": held_target,
            "invalid_attempt": invalid_attempt,
            "retry_outcome_kind": retry_outcome_kind,
            "retry_attempt": retry_attempt,
            "human_input_outstanding": human_input_outstanding,
            "blocked_targets": list(blocked_targets),
            "protected_nodes": list(protected_nodes),
            "latest_worker_rationale": {
                "summary": latest_worker_summary,
                "comments": latest_worker_comments,
            },
            "deterministic_worker_rejection_reasons": list(
                deterministic_worker_rejection_reasons
            ),
            "latest_review_rejection_reasons": list(latest_review_rejection_reasons),
        },
        "artifact_contract": {
            "result_type": "review_result_v1",
            "required_fields": [
                # Option C (2026-06-04): override_blocker_ids removed.
                "decision",
                "reason",
                "comments",
                "task_blocker_ids",
                "reset_blocker_ids",
                "next_active",
                "next_mode",
                "reset",
                "difficulty_updates",
            ],
            "optional_fields": [
                "clear_human_input",
                "next_worker_context_mode",
                "paper_focus_ranges",
                "work_style_hint",
            ],
            "prompt_schema_example": {
                "decision": list(allowed_decisions),
                "reason": "brief rationale for the decision",
                "comments": "optional non-authoritative comments to the next worker",
                "task_blocker_ids": ["subset of listed ids"],
                # Option C (2026-06-04): override_blocker_ids removed.
                "reset_blocker_ids": ["subset of allowed reset ids"],
                "next_active": "node id or empty string",
                "next_mode": list(allowed_next_modes),
                "reset": list(allowed_resets),
                "difficulty_updates": {"node_id from allowed_difficulty_update_nodes": "easy or hard"},
                "clear_human_input": (
                    True if human_input_outstanding else "omit unless clearing human input"
                ),
                "next_worker_context_mode": "resume or fresh",
                "paper_focus_ranges": [
                    {"start_line": 1, "end_line": 5, "reason": "optional source-paper focus"}
                ],
                "work_style_hint": "none or restructure",
            },
        },
        "verifier_evidence": dict(verifier_evidence or {"corr": {}, "sound": {}}),
        "blocker_partition": {
            "required": True,
            "partition_fields": [
                # Option C (2026-06-04): override_blocker_ids removed.
                "task_blocker_ids",
                "reset_blocker_ids",
            ],
            "choices": blocker_choices,
            "allowed_reset_ids": list(allowed_reset_ids),
            # Option C (2026-06-04): allowed_override_ids removed from
            # emitted contracts. The keyword arg above is retained as a
            # no-op for callsite compatibility.
            "reset_semantics": "clear_current_fail_to_unknown",
        },
        # Migration 2026-06-04 (batch 2): the review prompt's blocker-choices
        # block is rendered by the kernel and exposed via this field. In
        # production `review_contract_payload` populates it; tests construct
        # the contract by hand, so we replicate that synthesis via the test-
        # local `_test_blocker_*` helpers (mirrors of the kernel's
        # `review_blocker_*` family).
        "blocker_choices_block": _synth_review_blocker_choices_block_for_test(
            list(blocker_choices),
            active_node=active_node,
            held_target=held_target,
        ),
        "need_input_contract": {
            "meaning": "escalate_to_human_before_blocker_adjudication",
            "blocker_partition_required": False,
            # Option C (2026-06-04): override_blocker_ids removed.
            "task_blocker_ids": [],
            "reset_blocker_ids": ["subset of allowed reset ids"],
            "next_active": "",
            "next_mode": mode,
            "next_worker_context_mode": "resume",
            "paper_focus_ranges": [],
            "work_style_hint": "none",
        },
        "next_active_contract": {
            "kernel_hinted_nodes": list(allowed_next_nodes),
            "targeted_allowed_nodes": list(targeted_allowed_nodes),
            "allow_targeted_without_next_active": allow_targeted_without_next_active,
        },
        "difficulty_update_contract": {
            "allowed_nodes": list(allowed_difficulty_update_nodes),
        },
        "clear_human_input_contract": {
            "allowed_when_outstanding": human_input_outstanding,
            "omit_when_not_allowed": True,
        },
        "comments_contract": {
            "field": "comments",
            "semantics": "non_authoritative_guidance_forwarded_to_future_workers",
            "empty_string_means_no_comments": True,
        },
        "routing_hints_contract": {
            "next_worker_context_mode_values": ["resume", "fresh"],
            "paper_focus_ranges_shape": {
                "start_line": ">= 1",
                "end_line": ">= start_line",
                "reason": "optional short reason",
            },
            "work_style_hint_values": ["none", "restructure"],
            "continue_only": True,
            "advisory_only": True,
            "semantics": "non_authoritative_hints_forwarded_to_future_workers_without_expanding_kernel_authority",
        },
        "reset_contract": {
            "allowed_resets": list(allowed_resets),
            "last_commit_semantics": "discard_unaccepted_live_changes_and_resume_from_last_accepted_checkpoint",
        },
        "artifact_prompt_view": _artifact_prompt_view(
            (
                "python3",
                "{{check_script_path}}",
                "trellis-reviewer-result",
                "{{raw_output_path}}",
            ),
            (
            "python3",
            "{{check_script_path}}",
            "trellis-reviewer-result",
            "{{raw_output_path}}",
            "--context-json",
            "{{context_json_path}}",
            ),
        ),
    }


def _sound_contract(
    *,
    target_nodes: list[str] | tuple[str, ...],
    phase: str = "theorem_stating",
    node: str | None = None,
    active_node: str | None = None,
    held_target: str | None = None,
) -> dict[str, object]:
    return {
        "prompt_fragments": [
            "common/TRELLIS_FORMALIZATION_SCHEME.md",
            "verifier/common/00_intro.md",
            "shared/10_repository_root.md",
            "verifier/common/10_lane_id.md",
            "verifier/common/15_previous_findings.md",
            "shared/20_read_files.md",
            "shared/25_filespec.md",
            "shared/30_project_invariants.md",
            "verifier/soundness/20_target.md",
            "verifier/soundness/30_contract.md",
            "verifier/soundness/40_rubric.md",
            "verifier/soundness/50_authority.md",
            "shared/90_artifact_delivery.md",
        ],
        "request_summary": {
            "phase": phase,
            "node": node,
            "active_node": active_node,
            "held_target": held_target,
        },
        "previous_own_findings": {},
        "target_nodes": list(target_nodes),
        "evaluation_basis": "nl_only",
        "detail_floor": "paper_floor",
        "rubric": {
            "proof_standard": "line_by_line_rigorous",
            "reject_sketches": True,
            "detail_floor": "paper_floor",
            "lean_code_relevance": "ignore_lean_check_nl_only",
        },
        "artifact_contract": {
            "result_type": "soundness_result_v1",
            "decision_values": ["SOUND", "UNSOUND", "STRUCTURAL"],
            "overall_rule": "approve_iff_sound",
            "prompt_schema_example": {
                "node": "target_node",
                "soundness": {
                    "decision": "SOUND, UNSOUND, or STRUCTURAL",
                    "explanation": "brief explanation",
                },
                "overall": "APPROVE or REJECT",
                "summary": "brief overall summary",
                "comments": "optional short note",
            },
        },
        "artifact_prompt_view": _artifact_prompt_view((
            "python3",
            "{{check_script_path}}",
            "soundness-result",
            "{{raw_output_path}}",
            "--node",
            "{{node_name}}",
        )),
    }

def test_corr_bridge_maps_raw_lane_outputs() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    config = _fake_config(tmpdir)
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=tmpdir / "runtime",
        request={
            "project_invariants": _project_invariants(),
            "kind": "corr",
            "runtime_support_required": True,
            "id": 7,
            "cycle": 3,
            "phase": "theorem_stating",
            "verify_nodes": ["n2", "n1"],
            "verify_targets": ["target_a"],
            "corr_contract": _corr_contract(
                verify_nodes=["n2", "n1"],
                verify_targets=["target_a"],
            ),
            "verify_lanes": ["v2", "v1"],
            "corr_verify_lane_bindings": _lane_bindings(
                ["v2", "v1"], config.verification.correspondence_agents
            ),
            "sound_verify_lane_bindings": [],
            "blocked_targets": ["target_a"],
        },
    )

    captured_panel = {}

    def _fake_panel(panel, *, port_resolver=None):
        captured_panel["panel"] = panel
        return PanelExecutionResponse(
            request_id=panel.request_id,
            cycle=panel.cycle,
            kind=panel.kind,
            member_responses=[
                SingleAgentResponse(
                    request_id=panel.members[0].request_id,
                    cycle=panel.cycle,
                    kind="corr",
                    burst_role="reviewer",
                    ok=True,
                    payload={
                        "correspondence": {
                            "decision": "FAIL",
                            "issues": [{"node": "n2", "description": "wrong statement"}],
                        },
                        "overall": "REJECT",
                        "summary": "lane 1 rejects",
                        "comments": "",
                    },
                ),
                SingleAgentResponse(
                    request_id=panel.members[1].request_id,
                    cycle=panel.cycle,
                    kind="corr",
                    burst_role="reviewer",
                    ok=True,
                    payload={
                        "correspondence": {"decision": "PASS", "verdicts": []},
                        "overall": "APPROVE",
                        "summary": "lane 2 approves",
                        "comments": "",
                    },
                ),
            ],
        )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.execute_panel_raw",
        side_effect=_fake_panel,
    ), patch(
        "trellis.runtime.bridge.run_kernel_cli",
        side_effect=_fake_kernel_cli_for_verifier_normalization,
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    panel = captured_panel["panel"]
    assert [member.lane.node_name for member in panel.members] == ["v1", "v2"]
    assert [member.provider.provider for member in panel.members] == ["claude", "gemini"]
    # Regression guard for B1: multi-lane panel members MUST have distinct
    # tmux session_names. Otherwise `tmux new-session` in the second lane
    # kills the first lane's session (observed live on Corr/96 v1 wedging
    # the supervisor). Lane discriminator must be part of session_name.
    session_names = [m.session_name for m in panel.members]
    assert len(set(session_names)) == len(session_names), f"lane session_name collision: {session_names}"
    assert all("-v1" in session_names[0] or session_names[0].endswith("-v1") for _ in [0]), session_names[0]
    assert session_names[0].endswith("-v1") and session_names[1].endswith("-v2"), session_names
    assert response["kind"] == "corr"
    assert response["node_lane_updates"]["v1"]["n1"] == {"Set": "Pass"}
    assert response["node_lane_updates"]["v1"]["n2"] == {"Set": "Fail"}
    assert response["target_lane_updates"] == {}
    assert response["node_lane_updates"]["v2"]["n1"] == {"Set": "Pass"}
    assert response["node_lane_updates"]["v2"]["n2"] == {"Set": "Pass"}


def test_sound_bridge_maps_raw_lane_outputs() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    config = _fake_config(tmpdir)
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=tmpdir / "runtime",
        request={
            "project_invariants": _project_invariants(),
            "kind": "sound",
            "runtime_support_required": True,
            "id": 8,
            "cycle": 4,
            "phase": "proof_formalization",
            "verify_nodes": ["node_a"],
            "sound_verify_node": "node_a",
            "sound_contract": _sound_contract(target_nodes=["node_a"]),
            "verify_lanes": ["v1", "v2"],
            "corr_verify_lane_bindings": [],
            "sound_verify_lane_bindings": _lane_bindings(
                ["v1", "v2"], config.verification.soundness_agents
            ),
        },
    )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.execute_panel_raw",
        return_value=PanelExecutionResponse(
            request_id="sound-8",
            cycle=4,
            kind="sound",
            member_responses=[
                SingleAgentResponse(
                    request_id="sound-8-v1",
                    cycle=4,
                    kind="sound",
                    burst_role="reviewer",
                    ok=True,
                    payload={
                        "node": "node_a",
                        "soundness": {"decision": "SOUND", "explanation": "fine"},
                        "overall": "APPROVE",
                        "summary": "ok",
                        "comments": "",
                    },
                ),
                SingleAgentResponse(
                    request_id="sound-8-v2",
                    cycle=4,
                    kind="sound",
                    burst_role="reviewer",
                    ok=True,
                    payload={
                        "node": "node_a",
                        "soundness": {"decision": "STRUCTURAL", "explanation": "needs new helper"},
                        "overall": "REJECT",
                        "summary": "structural",
                        "comments": "",
                    },
                ),
            ],
        ),
    ), patch(
        "trellis.runtime.bridge.run_kernel_cli",
        side_effect=_fake_kernel_cli_for_verifier_normalization,
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert response["kind"] == "sound"
    assert response["lane_updates"]["v1"]["node_a"] == {"Set": "Pass"}
    assert response["lane_updates"]["v2"]["node_a"] == {"Set": "Structural"}


def test_corr_bridge_maps_preamble_item_failures_back_to_preamble_node() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    config = _fake_config(tmpdir)
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=tmpdir / "runtime",
        request={
            "project_invariants": _project_invariants(),
            "kind": "corr",
            "runtime_support_required": True,
            "id": 17,
            "cycle": 3,
            "phase": "theorem_stating",
            "verify_nodes": ["Preamble"],
            "verify_targets": [],
            "corr_contract": _corr_contract(
                verify_nodes=["Preamble"],
                verify_targets=[],
                preamble_item_ids=["Preamble[1]"],
            ),
            "verify_lanes": ["v1"],
            "corr_verify_lane_bindings": _lane_bindings(
                ["v1"], config.verification.correspondence_agents
            ),
            "sound_verify_lane_bindings": [],
            "blocked_targets": [],
        },
    )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.execute_panel_raw",
        return_value=PanelExecutionResponse(
            request_id="corr-17",
            cycle=3,
            kind="corr",
            member_responses=[
                SingleAgentResponse(
                    request_id="corr-17-v1",
                    cycle=3,
                    kind="corr",
                    burst_role="reviewer",
                    ok=True,
                    payload={
                        "correspondence": {
                            "decision": "FAIL",
                            "issues": [{"node": "Preamble[1]", "description": "unsupported"}],
                        },
                        "paper_faithfulness": {"decision": "PASS", "issues": []},
                        "overall": "REJECT",
                        "summary": "preamble item unsupported",
                        "comments": "",
                    },
                ),
            ],
        ),
    ), patch(
        "trellis.runtime.bridge.run_kernel_cli",
        side_effect=_fake_kernel_cli_for_verifier_normalization,
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert response["node_lane_updates"]["v1"]["Preamble"] == {"Set": "Fail"}


def test_corr_bridge_rejects_incomplete_lane_bindings() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    config = _fake_config(tmpdir)
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=tmpdir / "runtime",
        request={
            "kind": "corr",
            "runtime_support_required": True,
            "id": 22,
            "cycle": 4,
            "phase": "theorem_stating",
            "verify_nodes": ["n1"],
            "verify_targets": [],
            "verify_lanes": ["v2", "v1"],
            "corr_verify_lane_bindings": _lane_bindings(
                ["v1"], config.verification.correspondence_agents[:1]
            ),
            "sound_verify_lane_bindings": [],
            "blocked_targets": [],
        },
    )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager:
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        with pytest.raises(BridgeError, match="corr_verify_lane_bindings must cover exactly verify_lanes"):
            handle_bridge_request(request)


def test_review_bridge_validates_blocker_ids_and_maps_to_protocol_shape() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    config = _fake_config(tmpdir)
    blocker = {
        "kind": "TargetCorr",
        "object": {"otype": "target", "target": "main_result"},
        "fingerprint": "fp-main",
    }
    runtime_root = tmpdir / "extremal-live-clean" / "runtime"
    runtime_root.mkdir(parents=True, exist_ok=True)
    raw_path = config.repo_path / ".trellis" / "runtime" / runtime_root.name / "staging" / "trellis_review_9_decision.raw.json"
    raw_path.parent.mkdir(parents=True, exist_ok=True)
    raw_payload = {
        "decision": "continue",
        "reason": "Keep working with the current blocker overridden for this cycle.",
        "comments": "Keep the blocker focus narrow and preserve the current structure.",
        "task_blocker_ids": [],
        "override_blocker_ids": [_blocker_choice_id(blocker)],
        "next_active": "main_node",
        "next_mode": "targeted",
        "reset": "none",
        "difficulty_updates": {"main_node": "hard"},
        "allow_new_obligations": True,
        "must_close_active": False,
        "clear_human_input": True,
    }
    raw_path.write_text(json.dumps(raw_payload), encoding="utf-8")
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=runtime_root,
        request=_kernel_bound_request(config, {
            "kind": "review",
            "id": 9,
            "cycle": 5,
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "main_node",
            "blockers": [blocker],
            "review_blocker_choices": [{"id": _blocker_choice_id(blocker), "blocker": blocker}],
            "invalid_attempt": False,
            "human_input_outstanding": True,
            "blocked_targets": ["main_result"],
            "protected_nodes": ["main_node"],
            "allowed_decisions": ["continue", "advance_phase", "need_input"],
            "allowed_next_modes": ["global", "targeted"],
            "kernel_hinted_next_active_nodes": ["main_node"],
            "targeted_next_active_nodes": ["main_node"],
            "allow_targeted_without_next_active": False,
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none"],
            "allowed_difficulty_update_nodes": ["main_node"],
            "review_contract": _review_contract(
                blocker_choices=[{"id": _blocker_choice_id(blocker), "blocker": blocker}],
                allowed_next_nodes=["main_node"],
                targeted_allowed_nodes=["main_node"],
                allowed_difficulty_update_nodes=["main_node"],
                human_input_outstanding=True,
            ),
        }),
    )
    seen: dict[str, str] = {}

    def _capture_single(single: SingleAgentRequest, **_: object) -> SingleAgentResponse:
        seen["session_name"] = single.session_name
        return SingleAgentResponse(
            request_id="review-9-reviewer",
            cycle=5,
            kind="review",
            burst_role="reviewer",
            ok=True,
            raw_path=raw_path,
        )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.execute_agent_request",
        side_effect=_capture_single,
    ), patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "check_trellis_reviewer_result_ok",
            "output": {
                "ok": True,
                "errors": [],
                "data": {
                    "decision": "continue",
                    "reason": "Need to keep working.",
                    "comments": "Keep the blocker focus narrow and preserve the current structure.",
                },
                "response": {
                    "kind": "review",
                    "request_id": 9,
                    "cycle": 5,
                    "status": "Ok",
                    "decision": "Continue",
                    "comments": "Keep the blocker focus narrow and preserve the current structure.",
                    "task_blockers": [blocker],
                    "override_blockers": [blocker],
                    "reset_blockers": [],
                    "next_active": "main_node",
                    "reset": "None",
                    "next_mode": "Targeted",
                    "difficulty_updates": {"main_node": {"Set": "Hard"}},
                    "clear_human_input": True,
                }
            },
        },
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert response["kind"] == "review"
    assert response["decision"] == "Continue"
    assert response["override_blockers"] == [blocker]
    assert response["difficulty_updates"]["main_node"] == {"Set": "Hard"}
    assert response["clear_human_input"] is True
    # session_name must include the kind_label suffix so multi-lane verifier
    # panels get distinct tmux sessions per lane (see bridge._single_request_common).
    assert seen["session_name"] == "trellis-extremal-live-clean-review-9-reviewer"


def test_human_gate_bridge_reads_file_response() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / "runtime"
    runtime_root.mkdir(parents=True, exist_ok=True)
    (runtime_root / "human_gate_response.json").write_text(
        json.dumps({"choice": "feedback"}),
        encoding="utf-8",
    )
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=runtime_root,
        request={
            "kind": "human_gate",
            "runtime_support_required": False,
            "id": 10,
            "cycle": 6,
        },
    )

    with (
        patch("trellis.runtime.bridge.load_config", return_value=_fake_config(tmpdir)),
        patch(
            "trellis.runtime.bridge.run_kernel_cli",
            return_value={
                "status": "normalize_human_gate_ok",
                "output": {
                    "kind": "human_gate",
                    "request_id": 10,
                    "cycle": 6,
                    "status": "Ok",
                    "choice": "Feedback",
                },
            },
        ) as mock_kernel_cli,
    ):
        response = handle_bridge_request(request)

    assert response == {
        "kind": "human_gate",
        "request_id": 10,
        "cycle": 6,
        "status": "Ok",
        "choice": "Feedback",
    }
    call_payload = mock_kernel_cli.call_args.args[0]
    assert call_payload["action"] == "normalize_human_gate"
    assert call_payload["request_id"] == 10
    assert call_payload["cycle"] == 6
    assert json.loads(call_payload["raw_payload_text"]) == {"choice": "feedback"}


def test_human_gate_bridge_returns_kernel_malformed_response() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / "runtime"
    runtime_root.mkdir(parents=True, exist_ok=True)
    (runtime_root / "human_gate_response.json").write_text("not json", encoding="utf-8")
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=runtime_root,
        request={
            "kind": "human_gate",
            "runtime_support_required": False,
            "id": 11,
            "cycle": 7,
        },
    )

    with (
        patch("trellis.runtime.bridge.load_config", return_value=_fake_config(tmpdir)),
        patch(
            "trellis.runtime.bridge.run_kernel_cli",
            return_value={
                "status": "normalize_human_gate_ok",
                "output": {
                    "kind": "human_gate",
                    "request_id": 11,
                    "cycle": 7,
                    "status": "Malformed",
                    "choice": "Approve",
                },
            },
        ),
    ):
        response = handle_bridge_request(request)

    assert response["status"] == "Malformed"
    assert response["request_id"] == 11


def test_human_gate_bridge_skips_runtime_support_materialization() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / "runtime"
    runtime_root.mkdir(parents=True, exist_ok=True)
    (runtime_root / "human_gate_response.json").write_text(
        json.dumps({"choice": "approve"}),
        encoding="utf-8",
    )
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=runtime_root,
        request={
            "kind": "human_gate",
            "runtime_support_required": False,
            "id": 12,
            "cycle": 8,
        },
    )

    with (
        patch("trellis.runtime.bridge.load_config", return_value=_fake_config(tmpdir)),
        patch("trellis.runtime.bridge.write_scripts") as mock_install,
        patch(
            "trellis.runtime.bridge.run_kernel_cli",
            return_value={
                "status": "normalize_human_gate_ok",
                "output": {
                    "kind": "human_gate",
                    "request_id": 12,
                    "cycle": 8,
                    "status": "Ok",
                    "choice": "Approve",
                },
            },
        ),
    ):
        response = handle_bridge_request(request)

    assert response["status"] == "Ok"
    mock_install.assert_not_called()


def test_review_bridge_dry_run_writes_prompt_preview() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    config = _fake_config(tmpdir)
    runtime_root = tmpdir / "runtime"
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=runtime_root,
        request=_kernel_bound_request(config, {
            "kind": "review",
            "id": 10,
            "cycle": 6,
            "phase": "theorem_stating",
            "mode": "global",
            "blockers": [],
            "review_blocker_choices": [],
            "invalid_attempt": False,
            "human_input_outstanding": False,
            "blocked_targets": [],
            "protected_nodes": [],
            "allowed_decisions": ["continue", "need_input"],
            "allowed_next_modes": ["global"],
            "kernel_hinted_next_active_nodes": [],
            "targeted_next_active_nodes": [],
            "allow_targeted_without_next_active": False,
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none"],
            "allowed_difficulty_update_nodes": [],
            "review_contract": _review_contract(blocker_choices=[]),
        }),
    )

    old = os.environ.get("TRELLIS_TRELLIS_BRIDGE_DRY_RUN")
    os.environ["TRELLIS_TRELLIS_BRIDGE_DRY_RUN"] = "1"
    try:
        with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
            "trellis.runtime.bridge.PolicyManager"
        ) as mock_policy_manager:
            mock_policy_manager.return_value.current.return_value = _fake_policy()
            response = handle_bridge_request(request)
    finally:
        if old is None:
            os.environ.pop("TRELLIS_TRELLIS_BRIDGE_DRY_RUN", None)
        else:
            os.environ["TRELLIS_TRELLIS_BRIDGE_DRY_RUN"] = old

    assert response["dry_run"] is True
    prompt_path = Path(response["prompt_path"])
    assert prompt_path.exists()
    prompt_text = prompt_path.read_text(encoding="utf-8")
    assert "trellis" in prompt_text.lower()
    assert '"result_type": "review_result_v1"' in prompt_text
    assert '"reset_semantics": "clear_current_fail_to_unknown"' in prompt_text
    assert '"clear_human_input": true' not in prompt_text
    assert response["single_request"]["kind"] == "review"


def test_worker_bridge_dry_run_writes_prompt_preview() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    tablet_dir = tmpdir / "Tablet"
    tablet_dir.mkdir(parents=True, exist_ok=True)
    (tablet_dir / "ideal_bound_corollary.lean").write_text(
        "theorem ideal_bound_corollary : True := by sorry\n",
        encoding="utf-8",
    )
    (tablet_dir / "ideal_bound_corollary.tex").write_text(
        "\\begin{theorem}Test\\end{theorem}\n",
        encoding="utf-8",
    )
    config = _fake_config(tmpdir)
    runtime_root = tmpdir / "runtime"
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=runtime_root,
        request=_kernel_bound_request(config, {
            "kind": "worker",
            "id": 12,
            "cycle": 7,
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "ideal_bound_corollary",
            "held_target": None,
            "configured_targets": ["c.idealbd"],
                "worker_contract": _worker_contract(
                    authorized_nodes=["ideal_bound_corollary"],
                    configured_targets=["c.idealbd"],
                    blocked_targets=["c.idealbd"],
                    invalid_attempt=True,
                    deterministic_worker_rejection_reasons=["bad import"],
                ),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["ideal_bound_corollary"],
            },
            "worker_acceptance": _worker_acceptance(
                "theorem_targeted",
                ["ideal_bound_corollary"],
            ),
            "blockers": [],
            "blocked_targets": ["c.idealbd"],
            "protected_nodes": [],
            "current_present_nodes": ["ideal_bound_corollary"],
            "current_proof_nodes": ["ideal_bound_corollary"],
            "current_deps": {"ideal_bound_corollary": []},
            "current_semantic_deps": {"ideal_bound_corollary": []},
            "current_target_claims": {"ideal_bound_corollary": ["c.idealbd"]},
            "review_verifier_evidence": {
                "paper": {
                    "p1": {
                        "paper_faithfulness": {
                            "decision": "FAIL",
                            "issues": [{"subject_kind": "target", "subject": "c.idealbd"}],
                        },
                        "overall": "REJECT",
                        "summary": "Coverage misses the final manuscript quantifier.",
                        "comments": "Restore the exact paper conclusion.",
                    }
                },
                "corr": {},
                "sound": {},
            },
        }),
    )

    old = os.environ.get("TRELLIS_TRELLIS_BRIDGE_DRY_RUN")
    os.environ["TRELLIS_TRELLIS_BRIDGE_DRY_RUN"] = "1"
    try:
        with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
            "trellis.runtime.bridge.PolicyManager"
        ) as mock_policy_manager:
            mock_policy_manager.return_value.current.return_value = _fake_policy()
            response = handle_bridge_request(request)
    finally:
        if old is None:
            os.environ.pop("TRELLIS_TRELLIS_BRIDGE_DRY_RUN", None)
        else:
            os.environ["TRELLIS_TRELLIS_BRIDGE_DRY_RUN"] = old

    assert response["dry_run"] is True
    prompt_path = Path(response["prompt_path"])
    assert prompt_path.exists()
    prompt_text = prompt_path.read_text(encoding="utf-8")
    assert "authorized_nodes" in prompt_text
    assert '"result_type": "worker_result_v1"' in prompt_text
    assert '"allowed_outcomes": [' in prompt_text
    # A6: forbidden_legacy_fields was a 2-year-old migration relic; removed.
    assert '"forbidden_legacy_fields"' not in prompt_text
    verifier_evidence_path = (
        tmpdir
        / ".trellis"
        / "runtime"
        / runtime_root.name
        / "staging"
        / "trellis_worker_12_result.verifier_evidence.json"
    )
    assert verifier_evidence_path.exists()
    assert str(verifier_evidence_path) in prompt_text
    assert (
        json.loads(verifier_evidence_path.read_text(encoding="utf-8"))["paper"]["p1"][
            "summary"
        ]
        == "Coverage misses the final manuscript quantifier."
    )
    assert response["single_request"]["kind"] == "worker"


def test_paper_bridge_dry_run_writes_panel_prompt_previews() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    config = _fake_config(tmpdir)
    runtime_root = tmpdir / "runtime"
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=runtime_root,
        request={
            "project_invariants": _project_invariants(),
            "kind": "paper",
            "runtime_support_required": True,
            "id": 14,
            "cycle": 10,
            "phase": "theorem_stating",
            "paper_contract": _paper_contract(verify_targets=["target_a"]),
            "verify_lanes": ["v2", "v1"],
            "paper_verify_lane_bindings": _lane_bindings(
                ["v2", "v1"], config.verification.soundness_agents
            ),
            "paper_verify_targets": ["target_a"],
        },
    )

    old = os.environ.get("TRELLIS_TRELLIS_BRIDGE_DRY_RUN")
    os.environ["TRELLIS_TRELLIS_BRIDGE_DRY_RUN"] = "1"
    try:
        with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
            "trellis.runtime.bridge.PolicyManager"
        ) as mock_policy_manager, patch(
            "trellis.runtime.bridge.execute_panel_raw",
            side_effect=AssertionError("paper dry-run must not execute panel"),
        ) as mock_panel:
            mock_policy_manager.return_value.current.return_value = _fake_policy()
            response = handle_bridge_request(request)
    finally:
        if old is None:
            os.environ.pop("TRELLIS_TRELLIS_BRIDGE_DRY_RUN", None)
        else:
            os.environ["TRELLIS_TRELLIS_BRIDGE_DRY_RUN"] = old

    assert response["dry_run"] is True
    assert response["kind"] == "paper"
    assert len(response["prompt_paths"]) == 2
    assert len(response["single_requests"]) == 2
    assert response["single_requests"][0]["lane"]["node_name"] == "v1"
    prompt_path = Path(response["prompt_paths"][0])
    assert prompt_path.exists()
    prompt_text = prompt_path.read_text(encoding="utf-8")
    assert '"result_type": "paper_faithfulness_result_v1"' in prompt_text
    assert "target_a" in prompt_text
    mock_panel.assert_not_called()


def test_worker_bridge_targeted_dry_run_avoids_full_tablet_baseline() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    tablet_dir = tmpdir / "Tablet"
    tablet_dir.mkdir(parents=True, exist_ok=True)
    (tablet_dir / "ideal_bound_corollary.lean").write_text(
        "theorem ideal_bound_corollary : True := by sorry\n",
        encoding="utf-8",
    )
    (tablet_dir / "ideal_bound_corollary.tex").write_text(
        "\\begin{theorem}Test\\end{theorem}\n",
        encoding="utf-8",
    )
    config = _fake_config(tmpdir)
    runtime_root = tmpdir / "runtime"
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=runtime_root,
        request=_kernel_bound_request(config, {
            "kind": "worker",
            "id": 13,
            "cycle": 8,
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "ideal_bound_corollary",
            "held_target": None,
            "configured_targets": ["c.idealbd"],
            "worker_contract": _worker_contract(
                authorized_nodes=["ideal_bound_corollary"],
                configured_targets=["c.idealbd"],
                blocked_targets=["c.idealbd"],
                invalid_attempt=True,
                deterministic_worker_rejection_reasons=["bad import"],
            ),
            "worker_context": {
                "enabled": True,
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["ideal_bound_corollary"],
            },
            "worker_acceptance": _worker_acceptance(
                "theorem_targeted",
                ["ideal_bound_corollary"],
            ),
            "blockers": [],
            "blocked_targets": ["c.idealbd"],
            "protected_nodes": [],
            "current_present_nodes": ["ideal_bound_corollary"],
            "current_proof_nodes": ["ideal_bound_corollary"],
            "current_deps": {"ideal_bound_corollary": []},
            "current_semantic_deps": {"ideal_bound_corollary": []},
            "current_target_claims": {"ideal_bound_corollary": ["c.idealbd"]},
        }),
    )

    old = os.environ.get("TRELLIS_TRELLIS_BRIDGE_DRY_RUN")
    os.environ["TRELLIS_TRELLIS_BRIDGE_DRY_RUN"] = "1"
    try:
        with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
            "trellis.runtime.bridge.PolicyManager"
        ) as mock_policy_manager:
            mock_policy_manager.return_value.current.return_value = _fake_policy()
            response = handle_bridge_request(request)
    finally:
        if old is None:
            os.environ.pop("TRELLIS_TRELLIS_BRIDGE_DRY_RUN", None)
        else:
            os.environ["TRELLIS_TRELLIS_BRIDGE_DRY_RUN"] = old

    assert response["dry_run"] is True


def test_worker_bridge_dry_run_resets_scratch_workspace_for_fresh_context() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    tablet_dir = tmpdir / "Tablet"
    tablet_dir.mkdir(parents=True, exist_ok=True)
    (tablet_dir / "ideal_bound_corollary.lean").write_text(
        "theorem ideal_bound_corollary : True := by sorry\n",
        encoding="utf-8",
    )
    (tablet_dir / "ideal_bound_corollary.tex").write_text(
        "\\begin{theorem}Test\\end{theorem}\n",
        encoding="utf-8",
    )
    scratch_dir = tmpdir / ".trellis" / "scratch"
    scratch_dir.mkdir(parents=True, exist_ok=True)
    notes_path = scratch_dir / "notes.md"
    notes_path.write_text("carry this over\n", encoding="utf-8")
    config = _fake_config(tmpdir)
    runtime_root = tmpdir / "runtime"
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=runtime_root,
        request=_kernel_bound_request(config, {
            "kind": "worker",
            "id": 14,
            "cycle": 8,
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "ideal_bound_corollary",
            "held_target": None,
            "fresh_context": True,
            "configured_targets": ["c.idealbd"],
            "worker_contract": _worker_contract(
                authorized_nodes=["ideal_bound_corollary"],
                configured_targets=["c.idealbd"],
                blocked_targets=["c.idealbd"],
                invalid_attempt=True,
                deterministic_worker_rejection_reasons=["bad import"],
            ),
            "worker_context": {
                "enabled": True,
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["ideal_bound_corollary"],
            },
            "worker_acceptance": _worker_acceptance(
                "theorem_targeted",
                ["ideal_bound_corollary"],
            ),
            "blockers": [],
            "blocked_targets": ["c.idealbd"],
            "protected_nodes": [],
            "current_present_nodes": ["ideal_bound_corollary"],
            "current_proof_nodes": ["ideal_bound_corollary"],
            "current_deps": {"ideal_bound_corollary": []},
            "current_semantic_deps": {"ideal_bound_corollary": []},
            "current_target_claims": {"ideal_bound_corollary": ["c.idealbd"]},
        }),
    )

    old = os.environ.get("TRELLIS_TRELLIS_BRIDGE_DRY_RUN")
    os.environ["TRELLIS_TRELLIS_BRIDGE_DRY_RUN"] = "1"
    try:
        with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
            "trellis.runtime.bridge.PolicyManager"
        ) as mock_policy_manager:
            mock_policy_manager.return_value.current.return_value = _fake_policy()
            response = handle_bridge_request(request)
    finally:
        if old is None:
            os.environ.pop("TRELLIS_TRELLIS_BRIDGE_DRY_RUN", None)
        else:
            os.environ["TRELLIS_TRELLIS_BRIDGE_DRY_RUN"] = old

    prompt_text = Path(response["prompt_path"]).read_text(encoding="utf-8")
    assert scratch_dir.exists()
    assert notes_path.exists()
    assert "carry this over" not in notes_path.read_text(encoding="utf-8")
    assert str(scratch_dir) in prompt_text
    assert str(scratch_dir / "example.lean") in prompt_text
    assert str(notes_path) in prompt_text
    assert "reset to the baseline scratch workspace scaffold because the worker context is fresh" in prompt_text


def test_worker_bridge_dry_run_surfaces_last_invalid_snapshot_on_invalid_retry() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    tablet_dir = tmpdir / "Tablet"
    tablet_dir.mkdir(parents=True, exist_ok=True)
    (tablet_dir / "ideal_bound_corollary.lean").write_text(
        "theorem ideal_bound_corollary : True := by sorry\n",
        encoding="utf-8",
    )
    (tablet_dir / "ideal_bound_corollary.tex").write_text(
        "\\begin{theorem}Test\\end{theorem}\n",
        encoding="utf-8",
    )
    last_invalid_dir = tmpdir / ".trellis-history" / "worker_state" / "last_invalid"
    (last_invalid_dir / "Tablet").mkdir(parents=True, exist_ok=True)
    (last_invalid_dir / "Tablet" / "Old.lean").write_text(
        "theorem Old : True := by sorry\n",
        encoding="utf-8",
    )
    (last_invalid_dir / "metadata.json").write_text(
        json.dumps({"deterministic_rejection_reasons": ["bad import"]}),
        encoding="utf-8",
    )
    config = _fake_config(tmpdir)
    runtime_root = tmpdir / "runtime"
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=runtime_root,
        request=_kernel_bound_request(config, {
            "kind": "worker",
            "id": 15,
            "cycle": 9,
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "ideal_bound_corollary",
            "held_target": None,
            "invalid_attempt": True,
            "retry_outcome_kind": "Invalid",
            "configured_targets": ["c.idealbd"],
            "worker_contract": _worker_contract(
                authorized_nodes=["ideal_bound_corollary"],
                configured_targets=["c.idealbd"],
                blocked_targets=["c.idealbd"],
                invalid_attempt=True,
                deterministic_worker_rejection_reasons=["bad import"],
            ),
            "worker_context": {
                "enabled": True,
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["ideal_bound_corollary"],
            },
            "worker_acceptance": _worker_acceptance(
                "theorem_targeted",
                ["ideal_bound_corollary"],
            ),
            "blockers": [],
            "blocked_targets": ["c.idealbd"],
            "protected_nodes": [],
            "current_present_nodes": ["ideal_bound_corollary"],
            "current_proof_nodes": ["ideal_bound_corollary"],
            "current_deps": {"ideal_bound_corollary": []},
            "current_semantic_deps": {"ideal_bound_corollary": []},
            "current_target_claims": {"ideal_bound_corollary": ["c.idealbd"]},
            "deterministic_worker_rejection_reasons": ["bad import"],
            }),
        )

    old = os.environ.get("TRELLIS_TRELLIS_BRIDGE_DRY_RUN")
    os.environ["TRELLIS_TRELLIS_BRIDGE_DRY_RUN"] = "1"
    try:
        with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
            "trellis.runtime.bridge.PolicyManager"
        ) as mock_policy_manager:
            mock_policy_manager.return_value.current.return_value = _fake_policy()
            response = handle_bridge_request(request)
    finally:
        if old is None:
            os.environ.pop("TRELLIS_TRELLIS_BRIDGE_DRY_RUN", None)
        else:
            os.environ["TRELLIS_TRELLIS_BRIDGE_DRY_RUN"] = old

    prompt_text = Path(response["prompt_path"]).read_text(encoding="utf-8")
    assert str(last_invalid_dir) in prompt_text
    assert str(last_invalid_dir / "metadata.json") in prompt_text


def test_review_bridge_rejects_theorem_local_mode_to_match_kernel() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    config = _fake_config(tmpdir)
    runtime_root = tmpdir / "runtime"
    raw_path = (
        config.repo_path
        / ".trellis"
        / "runtime"
        / runtime_root.name
        / "staging"
        / "trellis_review_11_decision.raw.json"
    )
    raw_path.parent.mkdir(parents=True, exist_ok=True)
    raw_path.write_text(
        json.dumps(
            {
                "decision": "continue",
                "reason": "keep going",
                "comments": "Keep comments short.",
                "task_blocker_ids": [],
                "override_blocker_ids": [],
                "next_active": "",
                "next_mode": "local",
                "reset": "none",
                "difficulty_updates": {},
                "allow_new_obligations": True,
                "must_close_active": False,
                "comments": "",
            }
        ),
        encoding="utf-8",
    )
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=runtime_root,
        request=_kernel_bound_request(config, {
            "kind": "review",
            "id": 11,
            "cycle": 7,
            "phase": "theorem_stating",
            "mode": "global",
            "blockers": [],
            "review_blocker_choices": [],
            "invalid_attempt": False,
            "human_input_outstanding": False,
            "blocked_targets": [],
            "protected_nodes": [],
            "allowed_decisions": ["continue", "need_input"],
            "allowed_next_modes": ["global"],
            "kernel_hinted_next_active_nodes": [],
            "targeted_next_active_nodes": [],
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none"],
            "allowed_difficulty_update_nodes": [],
            "review_contract": _review_contract(blocker_choices=[]),
        }),
    )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.execute_agent_request",
        return_value=SingleAgentResponse(
            request_id="review-11-reviewer",
            cycle=7,
            kind="review",
            burst_role="reviewer",
            ok=True,
            raw_path=raw_path,
        ),
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert response["kind"] == "review"
    assert response["status"] == "Malformed"
    latest = json.loads(
        (tmpdir / "runtime" / "bridge" / "latest_review.json").read_text(encoding="utf-8")
    )
    assert any("reviewer next_mode must be one of" in error for error in latest["errors"])


def test_review_bridge_rejects_next_active_outside_kernel_affordance() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    config = _fake_config(tmpdir)
    runtime_root = tmpdir / "runtime"
    raw_path = (
        config.repo_path
        / ".trellis"
        / "runtime"
        / runtime_root.name
        / "staging"
        / "trellis_review_12_decision.raw.json"
    )
    raw_path.parent.mkdir(parents=True, exist_ok=True)
    raw_path.write_text(
        json.dumps(
            {
                "decision": "continue",
                "reason": "focus the blocked target cone",
                "comments": "Focus only the blocked target cone.",
                "task_blocker_ids": [],
                "override_blocker_ids": [],
                "next_active": "shifted_downset_corollary",
                "next_mode": "global",
                "reset": "none",
                "difficulty_updates": {},
                "allow_new_obligations": True,
                "must_close_active": False,
                "comments": "",
            }
        ),
        encoding="utf-8",
    )
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=runtime_root,
        request=_kernel_bound_request(config, {
            "kind": "review",
            "id": 12,
            "cycle": 7,
            "phase": "theorem_stating",
            "mode": "global",
            "blockers": [],
            "review_blocker_choices": [],
            "invalid_attempt": False,
            "human_input_outstanding": False,
            "blocked_targets": ["c.idealbd"],
            "protected_nodes": [],
            "allowed_decisions": ["continue", "advance_phase", "need_input"],
            "allowed_next_modes": ["global", "targeted"],
            "kernel_hinted_next_active_nodes": ["ideal_bound_corollary"],
            "targeted_next_active_nodes": ["ideal_bound_corollary"],
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none"],
            "allowed_difficulty_update_nodes": [],
            "review_contract": _review_contract(
                blocker_choices=[],
                allowed_next_nodes=["ideal_bound_corollary"],
                targeted_allowed_nodes=["ideal_bound_corollary"],
            ),
        }),
    )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.execute_agent_request",
        return_value=SingleAgentResponse(
            request_id="review-12-reviewer",
            cycle=7,
            kind="review",
            burst_role="reviewer",
            ok=True,
            raw_path=raw_path,
        ),
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert response["kind"] == "review"
    assert response["status"] == "Malformed"
    latest = json.loads(
        (tmpdir / "runtime" / "bridge" / "latest_review.json").read_text(encoding="utf-8")
    )
    assert any("reviewer next_active must be one of" in error for error in latest["errors"])


def test_review_bridge_allows_invalid_targeted_mode_without_next_active_when_kernel_says_so() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    config = _fake_config(tmpdir)
    runtime_root = tmpdir / "runtime"
    raw_path = (
        config.repo_path
        / ".trellis"
        / "runtime"
        / runtime_root.name
        / "staging"
        / "trellis_review_13_decision.raw.json"
    )
    raw_path.parent.mkdir(parents=True, exist_ok=True)
    raw_path.write_text(
        json.dumps(
            {
                "decision": "continue",
                "reason": "retry the same targeted task after invalid output",
                "comments": "Retry the same targeted task carefully.",
                "task_blocker_ids": [],
                "override_blocker_ids": [],
                "next_active": "",
                "next_mode": "targeted",
                "reset": "last_commit",
                "difficulty_updates": {},
                "allow_new_obligations": True,
                "must_close_active": False,
                "comments": "",
            }
        ),
        encoding="utf-8",
    )
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=runtime_root,
        request=_kernel_bound_request(config, {
            "kind": "review",
            "id": 13,
            "cycle": 8,
            "phase": "theorem_stating",
            "mode": "targeted",
            "blockers": [],
            "review_blocker_choices": [],
            "invalid_attempt": True,
            "human_input_outstanding": False,
            "blocked_targets": [],
            "protected_nodes": [],
            "allowed_decisions": ["continue", "need_input"],
            "allowed_next_modes": ["targeted"],
            "kernel_hinted_next_active_nodes": [],
            "targeted_next_active_nodes": [],
            "allow_targeted_without_next_active": True,
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none", "last_commit"],
            "allowed_difficulty_update_nodes": [],
            "review_contract": _review_contract(
                blocker_choices=[],
                allow_targeted_without_next_active=True,
            ),
        }),
    )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.execute_agent_request",
        return_value=SingleAgentResponse(
            request_id="review-13-reviewer",
            cycle=8,
            kind="review",
            burst_role="reviewer",
            ok=True,
            raw_path=raw_path,
        ),
    ), patch(
        "trellis.checking.run_kernel_cli",
        return_value={
            "status": "check_trellis_reviewer_result_ok",
            "output": {
                "ok": True,
                "errors": [],
                "data": {
                    "decision": "continue",
                    "reason": "Keep going.",
                    "comments": "Retry the same targeted task carefully.",
                },
                "response": {
                    "kind": "review",
                    "request_id": 13,
                    "cycle": 8,
                    "status": "Ok",
                    "decision": "Continue",
                    "comments": "Retry the same targeted task carefully.",
                    "task_blockers": [],
                    "override_blockers": [],
                    "reset_blockers": [],
                    "next_active": None,
                    "reset": "LastCommit",
                    "next_mode": "Targeted",
                    "difficulty_updates": {},
                    "clear_human_input": False,
                }
            },
        },
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert response["next_mode"] == "Targeted"
    assert response["next_active"] is None
    assert response["reset"] == "LastCommit"


def test_review_prompt_limits_next_mode_by_request_affordances() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / "runtime"
    theorem_prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "blockers": [],
            "review_blocker_choices": [],
            "review_contract": _review_contract(
                blocker_choices=[],
                phase="theorem_stating",
                allowed_decisions=["continue", "need_input"],
                allowed_next_modes=["global"],
                allowed_resets=["none"],
                allowed_difficulty_update_nodes=["main_node"],
            ),
            "invalid_attempt": False,
            "human_input_outstanding": False,
            "blocked_targets": [],
            "protected_nodes": [],
            "allowed_decisions": ["continue", "need_input"],
            "allowed_next_modes": ["global"],
            "kernel_hinted_next_active_nodes": [],
            "targeted_next_active_nodes": [],
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none"],
            "allowed_difficulty_update_nodes": ["main_node"],
        },
        repo_path=tmpdir,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )
    proof_prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "proof_formalization",
            "mode": "local",
            "blockers": [],
            "review_blocker_choices": [],
            "review_contract": _review_contract(
                blocker_choices=[],
                phase="proof_formalization",
                allowed_decisions=["continue", "need_input"],
                allowed_next_modes=["local", "coarse_restructure"],
                allowed_resets=["none"],
                allowed_next_nodes=["main_node"],
                allowed_difficulty_update_nodes=["main_node"],
            ),
            "invalid_attempt": False,
            "human_input_outstanding": False,
            "blocked_targets": [],
            "protected_nodes": [],
            "allowed_decisions": ["continue", "need_input"],
            "allowed_next_modes": ["local", "coarse_restructure"],
            "kernel_hinted_next_active_nodes": ["main_node"],
            "targeted_next_active_nodes": [],
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none"],
            "allowed_difficulty_update_nodes": ["main_node"],
        },
        repo_path=tmpdir,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )
    cleanup_prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "cleanup",
            "mode": "cleanup",
            "blockers": [],
            "review_blocker_choices": [],
            "review_contract": _review_contract(
                blocker_choices=[],
                phase="cleanup",
                allowed_decisions=["continue", "done"],
                allowed_next_modes=["cleanup"],
                allowed_resets=["none"],
                allowed_next_nodes=["main_node"],
                allowed_difficulty_update_nodes=["main_node"],
            ),
            "invalid_attempt": False,
            "human_input_outstanding": False,
            "blocked_targets": [],
            "protected_nodes": [],
            "allowed_decisions": ["continue", "done"],
            "allowed_next_modes": ["cleanup"],
            "kernel_hinted_next_active_nodes": ["main_node"],
            "targeted_next_active_nodes": [],
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none"],
            "allowed_difficulty_update_nodes": ["main_node"],
        },
        repo_path=tmpdir,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )

    assert '"next_mode": [' in theorem_prompt
    assert '"global"' in theorem_prompt
    assert '"reason": "brief rationale for the decision"' in theorem_prompt
    assert '"next_mode": [' in proof_prompt
    assert '"coarse_restructure"' in proof_prompt
    assert '"local"' in proof_prompt
    assert '"next_mode": [' in cleanup_prompt
    assert '"cleanup"' in cleanup_prompt
    assert "trellis-reviewer-result" in theorem_prompt
    assert "Choosing proof scope" not in theorem_prompt
    assert "Choosing proof scope" in proof_prompt
    assert "Choosing proof scope" not in cleanup_prompt


def test_review_prompt_accepts_rust_style_phase_names() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / "runtime"

    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "TheoremStating",
            "mode": "Global",
            "blockers": [],
            "review_blocker_choices": [],
            "review_contract": _review_contract(
                blocker_choices=[],
                allowed_next_nodes=["main_node"],
                targeted_allowed_nodes=["main_node"],
            ),
            "invalid_attempt": False,
            "human_input_outstanding": False,
            "blocked_targets": [],
            "protected_nodes": [],
            "allowed_decisions": ["continue", "need_input"],
            "allowed_next_modes": ["global", "targeted"],
            "kernel_hinted_next_active_nodes": ["main_node"],
            "targeted_next_active_nodes": ["main_node"],
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none"],
            "allowed_difficulty_update_nodes": ["main_node"],
        },
        repo_path=tmpdir,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )

    assert '"next_mode": [' in prompt
    assert '"global"' in prompt
    assert '"targeted"' in prompt
    assert "trellis-reviewer-result" in prompt
    assert "--context-json" in prompt


def test_review_prompt_uses_kernel_request_summary_not_top_level_fields() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / "runtime"

    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "proof_formalization",
            "mode": "local",
            "review_contract": _review_contract(
                blocker_choices=[],
                phase="theorem_stating",
                mode="global",
                active_node="main_node",
                blocked_targets=["main_result"],
                allowed_decisions=["continue", "need_input"],
                allowed_next_modes=["global", "targeted"],
                allowed_next_nodes=["main_node"],
                targeted_allowed_nodes=["main_node"],
            ),
        },
        repo_path=tmpdir,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )

    assert '"phase": "theorem_stating"' in prompt
    assert '"mode": "global"' in prompt
    assert '"blocked_targets": [' in prompt
    assert '"main_result"' in prompt
    assert '"phase": "proof_formalization"' not in prompt


def test_worker_prompt_renders_reviewer_comments_as_non_authoritative_guidance() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "n1",
            "held_target": None,
            "configured_targets": [],
            "worker_contract": _worker_contract(
                authorized_nodes=["n1"],
                phase="theorem_stating",
                mode="targeted",
                active_node="n1",
                worker_context={
                    "active_difficulty": "Hard",
                    "worker_profile": "Theorem",
                    "validation_kind": "theorem_targeted",
                    "authorized_nodes": ["n1"],
                },
                blockers=[],
                current_present_nodes=["n1"],
                current_proof_nodes=["n1"],
                current_deps={"n1": []},
                current_semantic_deps={"n1": []},
                current_target_claims={},
                reviewer_comments="Split this into helper lemmas before refining the main node.",
            ),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["n1"],
            },
            "worker_acceptance": _worker_acceptance("theorem_targeted", ["n1"]),
        },
        worker_gate={
            "worker_acceptance": _worker_acceptance("theorem_targeted", ["n1"]),
        },
        repo_path=tmpdir,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
    )

    assert "Split this into helper lemmas before refining the main node." in prompt
    assert "kernel-authored request, contract, and checker still control what is legal and accepted" in prompt


def test_worker_prompt_renders_reviewer_lean_product_handoff() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / ".trellis"
    runtime_root.mkdir(parents=True, exist_ok=True)
    product = {"kind": "sufficient_statement", "statement": "thread invariant H"}
    worker_contract = _worker_contract(
        authorized_nodes=["n1"],
        phase="proof_formalization",
        mode="local",
        active_node="n1",
        worker_context={
            "active_difficulty": "Hard",
            "worker_profile": "ProofHard",
            "validation_kind": "proof_local",
            "authorized_nodes": [],
        },
        blockers=[],
        current_present_nodes=["n1"],
        current_proof_nodes=["n1"],
        current_deps={"n1": []},
        current_target_claims={},
    )
    worker_contract["prompt_fragments"].insert(
        10, "worker/common/34b_stuck_math_reviewer_lean_product.md"
    )
    worker_contract["reviewer_lean_product"] = product

    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "proof_formalization",
            "mode": "local",
            "active_node": "n1",
            "held_target": None,
            "configured_targets": [],
            "worker_contract": worker_contract,
            "worker_context": worker_contract["request_summary"]["worker_context"],
            "worker_acceptance": {"validation_kind": "proof_local"},
        },
        worker_gate={"worker_acceptance": {"validation_kind": "proof_local"}},
        repo_path=tmpdir,
        raw_output_path=runtime_root / "worker.raw.json",
        done_path=runtime_root / "worker.done",
        acceptance_context_path=runtime_root / "worker.acceptance.json",
        runtime_root=runtime_root,
    )

    assert "## StuckMathAudit reviewer product" in prompt
    assert "verify its mathematical and Lean" in prompt
    assert "claims against the paper" in prompt
    assert '"kind": "sufficient_statement"' in prompt
    assert "{{reviewer_lean_product_json}}" not in prompt


def test_review_prompt_renders_stuck_math_audit_context_and_scratch_path() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / ".trellis"
    runtime_root.mkdir(parents=True, exist_ok=True)
    review_contract = _review_contract(
        blocker_choices=[],
        phase="proof_formalization",
        mode="local",
        active_node="n1",
        allowed_next_modes=["local"],
        allowed_next_nodes=["n1"],
    )
    review_contract["prompt_fragments"].insert(
        review_contract["prompt_fragments"].index("review/common/30_contract.md"),
        "review/common/29_stuck_math_audit.md",
    )
    review_contract["request_summary"]["stuck_math_audit"] = {
        "active": True,
        "trigger": "test",
        "active_since_cycle": 7,
        "trigger_blockers": [],
        "last_reviewer_lean_product": None,
    }

    prompt = build_review_prompt(
        request={
            "id": 42,
            "cycle": 7,
            "project_invariants": _project_invariants(),
            "phase": "proof_formalization",
            "mode": "local",
            "blockers": [],
            "review_blocker_choices": [],
            "review_contract": review_contract,
            "invalid_attempt": False,
            "human_input_outstanding": False,
            "blocked_targets": [],
            "protected_nodes": [],
            "allowed_decisions": ["continue", "need_input"],
            "allowed_next_modes": ["local"],
            "kernel_hinted_next_active_nodes": ["n1"],
            "targeted_next_active_nodes": [],
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none"],
            "allowed_difficulty_update_nodes": ["n1"],
        },
        repo_path=tmpdir,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )

    scratch_path = runtime_root / "stuck-math-audit" / "cycle-7-request-42"
    assert "## StuckMathAudit mode" in prompt
    assert str(scratch_path) in prompt
    assert scratch_path.is_dir()
    assert '"trigger": "test"' in prompt
    assert "{{stuck_math_audit_json}}" not in prompt


def test_stuck_math_audit_prompt_renders_cone_clean_impact_command() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / ".trellis"
    runtime_root.mkdir(parents=True, exist_ok=True)
    context_json_path = runtime_root / "stuck.context.json"
    contract = {
        "prompt_fragments": ["stuck_math_audit/common/04b_cone_clean.md"],
        "request_summary": {
            "phase": "proof_formalization",
            "scenario": "stuck_math_audit",
            "resettable_theorem_stating_nodes": ["N"],
        },
        "audit_latch": {"active": True, "trigger": "test"},
        "artifact_contract": {
            "result_type": "stuck_math_audit_result_v1",
            "prompt_schema_example": {
                "report": "audit report",
                "cone_clean_node": "N",
                "tasks": [],
                "probe_paths": [],
            },
        },
        "artifact_prompt_view": _artifact_prompt_view(
            (
                "python3",
                "{{check_script_path}}",
                "trellis-stuck-math-audit-result",
                "{{raw_output_path}}",
                "--context-json",
                "{{context_json_path}}",
            )
        ),
    }

    prompt = build_stuck_math_audit_prompt(
        request={
            "id": 99,
            "cycle": 7,
            "project_invariants": _project_invariants(),
            "stuck_math_audit_contract": contract,
        },
        repo_path=tmpdir,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "stuck.raw.json",
        done_path=runtime_root / "stuck.done",
        context_json_path=context_json_path,
    )

    assert "cone_clean_impact.py" in prompt
    assert f"--context-json {context_json_path} --node N" in prompt
    assert str(context_json_path) in prompt
    assert "{{context_json_path}}" not in prompt


def test_worker_prompt_points_to_request_local_verifier_evidence_sidecar() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    verifier_evidence_path = tmpdir / "worker.verifier_evidence.json"
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "n1",
            "held_target": None,
            "configured_targets": [],
            "worker_contract": _worker_contract(
                authorized_nodes=["n1"],
                phase="theorem_stating",
                mode="targeted",
                active_node="n1",
                worker_context={
                    "active_difficulty": "Hard",
                    "worker_profile": "Theorem",
                    "validation_kind": "theorem_targeted",
                    "authorized_nodes": ["n1"],
                },
                blockers=[],
                current_present_nodes=["n1"],
                current_proof_nodes=["n1"],
                current_deps={"n1": []},
                current_semantic_deps={"n1": []},
                current_target_claims={},
            ),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["n1"],
            },
            "worker_acceptance": _worker_acceptance("theorem_targeted", ["n1"]),
        },
        worker_gate={
            "worker_acceptance": _worker_acceptance("theorem_targeted", ["n1"]),
        },
        repo_path=tmpdir,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
        verifier_evidence_path=verifier_evidence_path,
    )

    assert str(verifier_evidence_path) in prompt
    assert "full lane-level verifier evidence" in prompt
    assert "Do not rely only on reviewer paraphrase" in prompt


def test_worker_prompt_renders_structured_routing_hints() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "n1",
            "held_target": None,
            "configured_targets": [],
            "fresh_context": True,
            "worker_contract": _worker_contract(
                authorized_nodes=["n1"],
                phase="theorem_stating",
                mode="targeted",
                active_node="n1",
                worker_context={
                    "active_difficulty": "Hard",
                    "worker_profile": "Theorem",
                    "validation_kind": "theorem_targeted",
                    "authorized_nodes": ["n1"],
                    "next_context_mode": "fresh",
                    "paper_focus_ranges": [
                        {"start_line": 41, "end_line": 48, "reason": "threshold statement"}
                    ],
                    "work_style_hint": "restructure",
                },
                blockers=[],
                current_present_nodes=["n1"],
                current_proof_nodes=["n1"],
                current_deps={"n1": []},
                current_semantic_deps={"n1": []},
                current_target_claims={},
            ),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["n1"],
                "next_context_mode": "fresh",
                "paper_focus_ranges": [
                    {"start_line": 41, "end_line": 48, "reason": "threshold statement"}
                ],
                "work_style_hint": "restructure",
            },
            "worker_acceptance": _worker_acceptance("theorem_targeted", ["n1"]),
        },
        worker_gate={
            "worker_acceptance": _worker_acceptance("theorem_targeted", ["n1"]),
        },
        repo_path=tmpdir,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
    )

    assert "Current session context mode: `fresh`." in prompt
    assert '"next_context_mode": "fresh"' in prompt
    assert '"paper_focus_ranges": [' in prompt
    assert '"work_style_hint": "restructure"' in prompt


def _write_paper_grounding_repo(
    tmpdir: Path, *, line_count: int = 60
) -> Path:
    """Set up a synthetic repo with trellis.config.json + paper/reference.tex."""
    (tmpdir / "trellis.config.json").write_text(
        json.dumps({"workflow": {"paper_tex_path": "paper/reference.tex"}}),
        encoding="utf-8",
    )
    paper_dir = tmpdir / "paper"
    paper_dir.mkdir(parents=True, exist_ok=True)
    lines = [f"line {i:04d}: lorem ipsum sample content" for i in range(1, line_count + 1)]
    # Plant a recognizable marker on lines 41-48 so tests can assert it.
    for marker_line in range(41, 49):
        lines[marker_line - 1] = f"line {marker_line:04d}: THRESHOLD_MARKER_{marker_line}"
    (paper_dir / "reference.tex").write_text("\n".join(lines) + "\n", encoding="utf-8")
    return tmpdir


def _worker_request_with_paper_focus(
    *, ranges: list[dict[str, object]], tmpdir: Path
) -> dict[str, object]:
    contract = _worker_contract(
        authorized_nodes=["n1"],
        phase="proof_formalization",
        mode="local",
        active_node="n1",
        worker_context={
            "active_difficulty": "Hard",
            "worker_profile": "ProofHard",
            "validation_kind": "proof_local",
            "authorized_nodes": ["n1"],
            "paper_focus_ranges": ranges,
        },
        blockers=[],
        current_present_nodes=["n1"],
        current_proof_nodes=["n1"],
        current_deps={"n1": []},
        current_semantic_deps={"n1": []},
        current_target_claims={},
    )
    # Mirror the eventual kernel-side change: include the new fragment
    # when the worker_context carries paper_focus_ranges. This is what
    # `worker_prompt_fragments` will do in Rust; for now the test helper
    # injects it directly so the Python-side rendering can be exercised.
    fragments = list(contract["prompt_fragments"])
    fragments.append("worker/common/19_paper_focus_fragments.md")
    contract["prompt_fragments"] = fragments
    return {
        "project_invariants": _project_invariants(),
        "phase": "proof_formalization",
        "mode": "local",
        "active_node": "n1",
        "held_target": None,
        "configured_targets": [],
        "worker_contract": contract,
        "worker_context": {
            "active_difficulty": "Hard",
            "worker_profile": "ProofHard",
            "validation_kind": "proof_local",
            "authorized_nodes": ["n1"],
            "paper_focus_ranges": ranges,
        },
        "worker_acceptance": _worker_acceptance("proof_local", ["n1"]),
    }


def test_worker_prompt_inlines_reviewer_selected_paper_fragments() -> None:
    tmpdir = _write_paper_grounding_repo(Path(tempfile.mkdtemp()))
    raw_output_path = tmpdir / "worker.raw.json"
    prompt = build_worker_prompt(
        request=_worker_request_with_paper_focus(
            ranges=[
                {"start_line": 41, "end_line": 48, "reason": "threshold statement"}
            ],
            tmpdir=tmpdir,
        ),
        worker_gate={"worker_acceptance": _worker_acceptance("proof_local", ["n1"])},
        repo_path=tmpdir,
        raw_output_path=raw_output_path,
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
    )

    assert "Reviewer-selected paper fragments" in prompt
    assert "paper/reference.tex:41-48" in prompt
    assert "Reason: threshold statement" in prompt
    assert "THRESHOLD_MARKER_41" in prompt
    assert "THRESHOLD_MARKER_48" in prompt
    assert "Complete cited text is inline above." in prompt
    sidecar = tmpdir / "worker.raw.paper_focus_fragments.md"
    assert sidecar.exists()
    sidecar_text = sidecar.read_text(encoding="utf-8")
    assert "THRESHOLD_MARKER_45" in sidecar_text
    assert str(sidecar) in prompt


def test_worker_prompt_omits_paper_fragments_block_when_no_ranges() -> None:
    tmpdir = _write_paper_grounding_repo(Path(tempfile.mkdtemp()))
    prompt = build_worker_prompt(
        request=_worker_request_with_paper_focus(ranges=[], tmpdir=tmpdir),
        worker_gate={"worker_acceptance": _worker_acceptance("proof_local", ["n1"])},
        repo_path=tmpdir,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
    )

    assert "Reviewer-selected paper fragments" not in prompt
    assert not (tmpdir / "worker.raw.paper_focus_fragments.md").exists()


def test_worker_prompt_paper_fragments_truncates_long_inline(monkeypatch) -> None:
    tmpdir = _write_paper_grounding_repo(
        Path(tempfile.mkdtemp()), line_count=200
    )
    monkeypatch.setattr(
        "trellis.runtime.bridge_prompts.PAPER_FOCUS_INLINE_MAX_LINES", 10
    )
    prompt = build_worker_prompt(
        request=_worker_request_with_paper_focus(
            ranges=[{"start_line": 41, "end_line": 90, "reason": "wide span"}],
            tmpdir=tmpdir,
        ),
        worker_gate={"worker_acceptance": _worker_acceptance("proof_local", ["n1"])},
        repo_path=tmpdir,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
    )

    assert "TRUNCATED" in prompt
    # The truncation marker must point at the sidecar.
    sidecar = tmpdir / "worker.raw.paper_focus_fragments.md"
    assert sidecar.exists()
    assert str(sidecar) in prompt
    # The full range survives in the sidecar even though the inline excerpt was capped.
    sidecar_text = sidecar.read_text(encoding="utf-8")
    assert "THRESHOLD_MARKER_48" in sidecar_text
    # And the inline excerpt should NOT contain a line past the line cap.
    # We capped at 10 lines of full_text (which includes headers); confirm a
    # late-fragment line marker is absent from the inline body.
    inline_segment = prompt.split("Reviewer-selected paper fragments", 1)[1].split(
        "[TRUNCATED", 1
    )[0]
    assert "line 0090" not in inline_segment


def test_worker_prompt_paper_fragments_raises_on_out_of_range() -> None:
    tmpdir = _write_paper_grounding_repo(Path(tempfile.mkdtemp()), line_count=60)
    with pytest.raises(ValueError, match="ends at line 999"):
        build_worker_prompt(
            request=_worker_request_with_paper_focus(
                ranges=[{"start_line": 1, "end_line": 999, "reason": "too wide"}],
                tmpdir=tmpdir,
            ),
            worker_gate={"worker_acceptance": _worker_acceptance("proof_local", ["n1"])},
            repo_path=tmpdir,
            raw_output_path=tmpdir / "worker.raw.json",
            done_path=tmpdir / "worker.done",
            acceptance_context_path=tmpdir / "worker.acceptance.json",
        )


def test_worker_prompt_paper_fragments_raises_on_missing_paper_config() -> None:
    tmpdir = Path(tempfile.mkdtemp())  # no trellis.config.json
    with pytest.raises(ValueError, match="paper_tex_path not"):
        build_worker_prompt(
            request=_worker_request_with_paper_focus(
                ranges=[{"start_line": 1, "end_line": 5, "reason": "anything"}],
                tmpdir=tmpdir,
            ),
            worker_gate={"worker_acceptance": _worker_acceptance("proof_local", ["n1"])},
            repo_path=tmpdir,
            raw_output_path=tmpdir / "worker.raw.json",
            done_path=tmpdir / "worker.done",
            acceptance_context_path=tmpdir / "worker.acceptance.json",
        )


def test_worker_prompt_mentions_private_system_feedback_channel() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "active_node": None,
            "held_target": None,
            "configured_targets": ["thm:conn"],
            "worker_contract": _worker_contract(
                authorized_nodes=["Preamble"],
                phase="theorem_stating",
                mode="global",
                active_node=None,
                worker_context={
                    "active_difficulty": "Hard",
                    "worker_profile": "Theorem",
                    "validation_kind": "theorem_global",
                    "authorized_nodes": ["Preamble"],
                },
                blockers=[],
                current_present_nodes=["Preamble"],
                current_proof_nodes=[],
                current_deps={"Preamble": []},
                current_semantic_deps={"Preamble": []},
                current_target_claims={},
                configured_targets=["thm:conn"],
                blocked_targets=["thm:conn"],
            ),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_global",
                "authorized_nodes": ["Preamble"],
            },
            "worker_acceptance": _worker_acceptance("theorem_global", ["Preamble"]),
        },
        worker_gate={
            "worker_acceptance": _worker_acceptance("theorem_global", ["Preamble"]),
        },
        repo_path=tmpdir,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
    )

    assert "system_feedback" in prompt
    assert "private host-side log" in prompt
    assert "agents cannot read" in prompt


def test_worker_prompt_falls_back_to_vendored_filespec_when_repo_copy_missing() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_src = tmpdir / ".trellis" / "runtime" / "src"
    runtime_src.mkdir(parents=True, exist_ok=True)
    (runtime_src / "FILESPEC.md").write_text("# vendored filespec\n", encoding="utf-8")

    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "active_node": None,
            "held_target": None,
            "configured_targets": ["thm:conn"],
            "worker_contract": _worker_contract(
                authorized_nodes=["Preamble"],
                phase="theorem_stating",
                mode="global",
                active_node=None,
                worker_context={
                    "active_difficulty": "Hard",
                    "worker_profile": "Theorem",
                    "validation_kind": "theorem_global",
                    "authorized_nodes": ["Preamble"],
                },
                blockers=[],
                current_present_nodes=["Preamble"],
                current_proof_nodes=[],
                current_deps={"Preamble": []},
                current_semantic_deps={"Preamble": []},
                current_target_claims={},
                configured_targets=["thm:conn"],
                blocked_targets=["thm:conn"],
            ),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_global",
                "authorized_nodes": ["Preamble"],
            },
            "worker_acceptance": _worker_acceptance("theorem_global", ["Preamble"]),
        },
        worker_gate={
            "worker_acceptance": _worker_acceptance("theorem_global", ["Preamble"]),
        },
        repo_path=tmpdir,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
    )

    assert str(runtime_src / "FILESPEC.md") in prompt
    assert str(tmpdir / "FILESPEC.md") not in prompt


def _build_loogle_probe_prompt(tmpdir: Path) -> str:
    return build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "active_node": None,
            "held_target": None,
            "configured_targets": ["thm:conn"],
            "worker_contract": _worker_contract(
                authorized_nodes=["Preamble"],
                phase="theorem_stating",
                mode="global",
                active_node=None,
                worker_context={
                    "active_difficulty": "Hard",
                    "worker_profile": "Theorem",
                    "validation_kind": "theorem_global",
                    "authorized_nodes": ["Preamble"],
                },
                blockers=[],
                current_present_nodes=["Preamble"],
                current_proof_nodes=[],
                current_deps={"Preamble": []},
                current_semantic_deps={"Preamble": []},
                current_target_claims={},
                configured_targets=["thm:conn"],
                blocked_targets=["thm:conn"],
            ),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_global",
                "authorized_nodes": ["Preamble"],
            },
            "worker_acceptance": _worker_acceptance("theorem_global", ["Preamble"]),
        },
        worker_gate={
            "worker_acceptance": _worker_acceptance("theorem_global", ["Preamble"]),
        },
        repo_path=tmpdir,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
    )


def test_worker_prompt_mentions_repo_local_loogle_helper() -> None:
    # No trellis.config.json present -> loogle defaults ON, helper included.
    tmpdir = Path(tempfile.mkdtemp())
    prompt = _build_loogle_probe_prompt(tmpdir)

    assert str(tmpdir / ".trellis" / "runtime" / "src" / "scripts" / "loogle_json.sh") in prompt
    assert 'bash ' in prompt
    assert "built-in timeout of 60 seconds" in prompt
    assert "Cold or broad queries can take several seconds." in prompt


def test_worker_prompt_omits_loogle_when_disabled() -> None:
    # loogle.enabled=false -> the Loogle helper fragment is dropped entirely.
    tmpdir = Path(tempfile.mkdtemp())
    (tmpdir / "trellis.config.json").write_text(
        json.dumps({"loogle": {"enabled": False}}), encoding="utf-8"
    )
    prompt = _build_loogle_probe_prompt(tmpdir)

    assert "loogle_json.sh" not in prompt
    assert "Loogle helper" not in prompt


def test_worker_prompt_clarifies_structure_scope_and_worker_fields() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "main_node",
            "held_target": "main_node",
            "configured_targets": ["thm:conn"],
            "worker_contract": _worker_contract(
                authorized_nodes=["main_node"],
                phase="theorem_stating",
                mode="targeted",
                active_node="main_node",
                held_target="main_node",
                worker_context={
                    "active_difficulty": "Hard",
                    "worker_profile": "Theorem",
                    "validation_kind": "theorem_targeted",
                    "authorized_nodes": ["main_node"],
                },
                blockers=[],
                current_present_nodes=["main_node"],
                current_proof_nodes=["main_node"],
                current_deps={"main_node": []},
                current_semantic_deps={"main_node": []},
                current_target_claims={},
                configured_targets=["thm:conn"],
                blocked_targets=["thm:conn"],
                reviewer_comments="Please restructure locally if needed.",
            ),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["main_node"],
            },
            "worker_acceptance": _worker_acceptance("theorem_targeted", ["main_node"]),
        },
        worker_gate={
            "worker_acceptance": _worker_acceptance("theorem_targeted", ["main_node"]),
        },
        repo_path=tmpdir,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
    )

    assert "FILESPEC.md` and the Trellis formalization scheme still govern tablet structure" in prompt
    assert '`"mode": "targeted"`' in prompt
    assert "Do not put free top-level prose, `\\section` commands, or extra theorem environments" in prompt
    assert "Definitions use `def`, `abbrev`, or `noncomputable def`" in prompt
    assert "proof-bearing nodes use `theorem` or `lemma`" in prompt
    # B3: semantic_dep_updates is dead protocol — no longer emitted in
    # worker field guidance.
    assert "semantic_dep_updates" not in prompt
    assert "Every new node must appear as a key" in prompt
    assert "Use `[]` when the node doesn't directly claim a configured paper target" in prompt
    assert "one node, one target maximum" in prompt
    assert "return `needs_restructure` instead of forcing a monolithic or out-of-scope patch" in prompt
    assert "Use `stuck` only when you cannot yet identify a specific honest broader fix" in prompt


def test_worker_prompt_uses_configurable_initial_theorem_dag_size_guidance() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "active_node": None,
            "held_target": None,
            "configured_targets": ["thm:conn"],
            "worker_contract": _worker_contract(
                authorized_nodes=["main_node"],
                phase="theorem_stating",
                mode="global",
                active_node=None,
                worker_context={
                    "active_difficulty": "Hard",
                    "worker_profile": "Theorem",
                    "validation_kind": "theorem_global",
                    "authorized_nodes": ["main_node"],
                },
                blockers=[],
                current_present_nodes=["main_node"],
                current_proof_nodes=["main_node"],
                current_deps={"main_node": []},
                current_semantic_deps={"main_node": []},
                current_target_claims={},
                configured_targets=["thm:conn"],
                blocked_targets=["thm:conn"],
            ),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_global",
                "authorized_nodes": ["main_node"],
            },
            "worker_acceptance": _worker_acceptance("theorem_global", ["main_node"]),
        },
        worker_gate={
            "worker_acceptance": _worker_acceptance("theorem_global", ["main_node"]),
        },
        repo_path=tmpdir,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
        theorem_initial_dag_size_guidance="20-60",
    )

    assert "a good target for the initial proof-bearing DAG size is `20-60` proof-bearing nodes" in prompt


def test_proof_worker_prompt_includes_operational_guidance() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "proof_formalization",
            "mode": "local",
            "active_node": "main_node",
            "held_target": None,
            "worker_contract": {
                "prompt_fragments": [
                    "common/TRELLIS_FORMALIZATION_SCHEME.md",
                    "worker/proof_formalization/00_intro.md",
                    "worker/proof_formalization/05_scope_local.md",
                    "worker/proof_formalization/06_gate_allow_new_obligations.md",
                    "worker/proof_formalization/07_gate_active_may_remain_open.md",
                    "shared/10_repository_root.md",
                    "shared/20_read_files.md",
                    "worker/common/15_loogle.md",
                    "shared/25_filespec.md",
                    "shared/30_project_invariants.md",
                    "worker/common/20_authority.md",
                    "worker/common/30_request.md",
                    "worker/common/34_verifier_evidence.md",
                    "worker/proof_formalization/10_operational_guidance.md",
                    "worker/proof_formalization/15_failure_triage.md",
                    "worker/proof_formalization/20_helper_decomposition.md",
                    "worker/common/33_routing_hints.md",
                    "worker/common/35_reviewer_comments.md",
                    "worker/common/37_field_guidance.md",
                    "worker/common/40_contract.md",
                    "worker/common/45_outcomes.md",
                    "worker/common/50_acceptance.md",
                    "shared/90_artifact_delivery.md",
                    "worker/common/95_gate_authority.md",
                ],
                "request_summary": {
                    "phase": "proof_formalization",
                    "mode": "local",
                    "active_node": "main_node",
                    "held_target": None,
                    "fresh_context": False,
                    "worker_context": {
                        "active_difficulty": "Hard",
                        "worker_profile": "ProofHard",
                        "validation_kind": "proof_local",
                        "authorized_nodes": ["main_node"],
                    },
                    "blockers": [],
                    "protected_nodes": [],
                    "current_present_nodes": ["main_node"],
                    "current_proof_nodes": ["main_node"],
                    "current_deps": {"main_node": []},
                    "current_semantic_deps": {"main_node": []},
                    "current_target_claims": {},
                },
                "reviewer_comments": "",
                "result_type": "worker_result_v1",
                "kernel_derives_structural_snapshot": True,
                "allowed_outcomes": ["valid", "invalid", "stuck", "needs_restructure"],
                "reported_delta_fields": [
                    "semantic_dep_updates",
                    "target_claim_updates",
                    "difficulty_updates",
                ],
                "forbidden_legacy_fields": ["status", "CRISIS"],
                "prompt_schema_example": {
                    "outcome": "valid / invalid / stuck / needs_restructure",
                    "summary": "brief summary",
                    "comments": "optional short note",
                    "semantic_dep_updates": {"node_id": ["semantic_dep_node", "..."]},
                    "target_claim_updates": {"node_id": ["target_id"]},
                    "difficulty_updates": {"node_id": "easy or hard"},
                },
                "scope_contract": {
                    "existing_node_scope_mode": "active_node_only",
                    "authorized_existing_nodes": ["main_node"],
                    "configured_targets": [],
                    "pending_targets": [],
                    "pending_targets_meaning": "targets_lacking_current_approved_support",
                    "new_nodes_allowed": True,
                },
                "stuck_contract": {
                    "allowed": True,
                    "forbid_tablet_changes_when_stuck": True,
                    "meaning": "cannot_make_progress_on_pending_work_under_current_scope",
                },
                "needs_restructure_contract": {
                    "allowed": True,
                    "forbid_tablet_changes_when_needs_restructure": False,
                    "meaning": "worker_can_name_broader_restructure_needed_but_current_scope_does_not_authorize_it",
                },
                "artifact_prompt_view": _artifact_prompt_view(
                    (
                        "python3",
                        "{{check_script_path}}",
                        "trellis-worker-result",
                        "{{raw_output_path}}",
                    ),
                    (
                        "python3",
                        "{{check_script_path}}",
                        "trellis-worker-result",
                        "{{raw_output_path}}",
                        "--repo",
                        "{{repo_path}}",
                        "--context-json",
                        "{{acceptance_context_path}}",
                    ),
                ),
            },
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "ProofHard",
                "validation_kind": "proof_local",
                "authorized_nodes": ["main_node"],
            },
            "worker_acceptance": _worker_acceptance("proof_local", ["main_node"]),
        },
        worker_gate={
            "worker_acceptance": _worker_acceptance("proof_local", ["main_node"]),
        },
        repo_path=tmpdir,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
    )

    assert "Start by reading the active goal state carefully before editing." in prompt
    assert "Treat the current tablet DAG as the search boundary by default." in prompt
    assert "For the edit-compile-fix inner loop, use `lake build Tablet.NodeName`" in prompt
    assert "authoritative sign-off gate" in prompt
    assert "Missing lemma or missing support node" in prompt
    assert "Wrong statement or wrong interface" in prompt
    assert "Proof search or implementation issue" in prompt
    assert "prefer meaningful helper decomposition over flailing inside one oversized proof" in prompt
    assert "`local` authorizes edits to the active node's Lean proof body and imports" in prompt
    assert "`scope_contract.allow_new_obligations=true`" in prompt
    assert "`scope_contract.must_close_active=false`" in prompt


def test_review_prompt_includes_verifier_reasoning_and_comments_guidance() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / "runtime"

    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "blockers": [],
            "review_blocker_choices": [],
            "review_contract": _review_contract(
                blocker_choices=[],
                allowed_next_nodes=["main_node"],
                targeted_allowed_nodes=["main_node"],
                verifier_evidence={
                    "corr": {
                        "v1": {
                            "overall": "REJECT",
                            "summary": "The statement omits a key quantifier.",
                            "comments": "Restore the quantified hypothesis.",
                        }
                    },
                    "sound": {
                        "v2": {
                            "node": "main_node",
                            "overall": "REJECT",
                            "summary": "The NL proof skips the component bound.",
                            "comments": "Add the missing bound explicitly.",
                            "soundness": {
                                "decision": "UNSOUND",
                                "explanation": "A crucial estimate is missing.",
                            },
                        }
                    },
                },
            ),
            "invalid_attempt": False,
            "human_input_outstanding": False,
            "blocked_targets": [],
            "protected_nodes": [],
            "allowed_decisions": ["continue", "need_input"],
            "allowed_next_modes": ["global", "targeted"],
            "kernel_hinted_next_active_nodes": ["main_node"],
            "targeted_next_active_nodes": ["main_node"],
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none"],
            "allowed_difficulty_update_nodes": ["main_node"],
        },
        repo_path=tmpdir,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )

    assert "Current verifier reasoning" in prompt
    assert "The statement omits a key quantifier." in prompt
    assert "A crucial estimate is missing." in prompt
    assert '"comments": "optional non-authoritative comments to the next worker"' in prompt
    assert "next_worker_context_mode" in prompt
    assert "paper_focus_ranges" in prompt
    assert "work_style_hint" in prompt
    # B4: positive framing of routing hints
    assert "Usually `resume` is a good choice" in prompt
    assert "When `Continue` is the chosen decision" in prompt
    # B5/B1: ProofFormalization-only restructure/difficulty/proof-focus
    # fragments are not included in TheoremStating. Only the always-on
    # paper-focus + revert-strategy fragments remain.
    assert "Keep the ranges as narrow as they can be while still containing the relevant source argument." in prompt
    assert "Use `last_commit` when the current live state is a bad direction" in prompt


def test_review_prompt_surfaces_deterministic_worker_rejection_reasons() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / "runtime"
    last_invalid_metadata = (
        tmpdir / ".trellis-history" / "worker_state" / "last_invalid" / "metadata.json"
    )
    last_invalid_metadata.parent.mkdir(parents=True, exist_ok=True)
    last_invalid_metadata.write_text('{"request_id": 42}\n', encoding="utf-8")

    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "blockers": [],
            "review_blocker_choices": [],
            "review_contract": _review_contract(
                blocker_choices=[],
                invalid_attempt=True,
                retry_outcome_kind="Invalid",
                deterministic_worker_rejection_reasons=[
                    "authoritative checker mismatch: worker-side acceptance reported success, but the supervisor authoritative check rejected the submitted result.",
                    "Tablet/Main.lean has an application type mismatch",
                ],
            ),
            "invalid_attempt": True,
            "retry_outcome_kind": "Invalid",
            "human_input_outstanding": False,
            "blocked_targets": [],
            "protected_nodes": [],
            "allowed_decisions": ["continue", "need_input"],
            "allowed_next_modes": ["global", "targeted"],
            "kernel_hinted_next_active_nodes": ["main_node"],
            "targeted_next_active_nodes": ["main_node"],
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none", "last_commit"],
            "allowed_difficulty_update_nodes": [],
        },
        repo_path=tmpdir,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )

    assert "Authoritative deterministic worker rejection" in prompt
    assert "authoritative checker mismatch" in prompt
    assert "Tablet/Main.lean has an application type mismatch" in prompt
    assert "inspect the raw checker artifacts" in prompt
    assert str(runtime_root / "bridge" / "latest_worker.json") in prompt
    assert str(last_invalid_metadata) in prompt
    assert str(tmpdir / ".trellis" / "checker" / "worker_request_42.json") in prompt


def test_review_prompt_without_deterministic_rejections_omits_checker_artifact_guidance() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / "runtime"

    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "blockers": [],
            "review_blocker_choices": [],
            "review_contract": _review_contract(blocker_choices=[]),
            "invalid_attempt": False,
            "retry_outcome_kind": "None",
            "human_input_outstanding": False,
            "blocked_targets": [],
            "protected_nodes": [],
            "allowed_decisions": ["continue", "need_input"],
            "allowed_next_modes": ["global", "targeted"],
            "kernel_hinted_next_active_nodes": ["main_node"],
            "targeted_next_active_nodes": ["main_node"],
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none", "last_commit"],
            "allowed_difficulty_update_nodes": [],
        },
        repo_path=tmpdir,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )

    assert "inspect the raw checker artifacts" not in prompt
    assert str(runtime_root / "bridge" / "latest_worker.json") not in prompt
    assert ".trellis/checker/worker_request_" not in prompt


def test_review_prompt_checker_artifact_guidance_falls_back_without_metadata() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / "runtime"

    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "blockers": [],
            "review_blocker_choices": [],
            "review_contract": _review_contract(
                blocker_choices=[],
                deterministic_worker_rejection_reasons=[
                    "authoritative checker mismatch: compact summary only",
                ],
            ),
            "invalid_attempt": True,
            "retry_outcome_kind": "Invalid",
            "human_input_outstanding": False,
            "blocked_targets": [],
            "protected_nodes": [],
            "allowed_decisions": ["continue", "need_input"],
            "allowed_next_modes": ["global", "targeted"],
            "kernel_hinted_next_active_nodes": ["main_node"],
            "targeted_next_active_nodes": ["main_node"],
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none", "last_commit"],
            "allowed_difficulty_update_nodes": [],
        },
        repo_path=tmpdir,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )

    assert "inspect the raw checker artifacts" in prompt
    assert "worker_request_<request_id>.json" in prompt
    assert "read request_id from" in prompt


def test_review_prompt_surfaces_latest_worker_rationale_from_kernel_request_summary() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / "runtime"

    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "blockers": [],
            "review_blocker_choices": [],
            "review_contract": _review_contract(
                blocker_choices=[],
                latest_worker_summary="built an initial 15-node DAG",
                latest_worker_comments="the component-counting branch likely needs repair",
            ),
            "invalid_attempt": False,
            "retry_outcome_kind": "None",
            "human_input_outstanding": False,
            "blocked_targets": [],
            "protected_nodes": [],
            "allowed_decisions": ["continue", "need_input"],
            "allowed_next_modes": ["global", "targeted"],
            "kernel_hinted_next_active_nodes": [],
            "targeted_next_active_nodes": [],
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none"],
            "allowed_difficulty_update_nodes": [],
        },
        repo_path=tmpdir,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )

    assert '"latest_worker_rationale": {' in prompt
    assert '"summary": "built an initial 15-node DAG"' in prompt
    assert '"comments": "the component-counting branch likely needs repair"' in prompt


def test_review_prompt_includes_acceptance_checker_when_present() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / "runtime"

    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "blockers": [],
            "review_blocker_choices": [],
            "review_contract": _review_contract(blocker_choices=[]),
            "invalid_attempt": False,
            "retry_outcome_kind": "None",
            "human_input_outstanding": False,
            "blocked_targets": [],
            "protected_nodes": [],
            "allowed_decisions": ["continue", "need_input"],
            "allowed_next_modes": ["global", "targeted"],
            "kernel_hinted_next_active_nodes": [],
            "targeted_next_active_nodes": [],
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none"],
            "allowed_difficulty_update_nodes": [],
        },
        repo_path=tmpdir,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )

    assert "You should test ahead of time whether your work will satisfy deterministic validity checks by running the command" in prompt
    assert "trellis-reviewer-result" in prompt
    assert "--context-json" in prompt
    assert "No additional acceptance checker is provided for this role." not in prompt


def test_worker_prompt_surfaces_deterministic_worker_rejection_reasons() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / "runtime"
    last_invalid_metadata = (
        tmpdir / ".trellis-history" / "worker_state" / "last_invalid" / "metadata.json"
    )
    last_invalid_metadata.parent.mkdir(parents=True, exist_ok=True)
    last_invalid_metadata.write_text('{"request_id": 99}\n', encoding="utf-8")
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "active_node": None,
            "held_target": None,
            "configured_targets": ["thm:conn"],
            "worker_contract": {
                **_worker_contract(
                    authorized_nodes=["main_node"],
                    phase="theorem_stating",
                    mode="global",
                    active_node=None,
                    worker_context={
                        "active_difficulty": "Hard",
                        "worker_profile": "Theorem",
                        "validation_kind": "theorem_global",
                        "authorized_nodes": ["main_node"],
                    },
                    blockers=[],
                    current_present_nodes=["main_node"],
                    current_proof_nodes=["main_node"],
                    current_deps={"main_node": []},
                    current_semantic_deps={"main_node": []},
                    current_target_claims={},
                    configured_targets=["thm:conn"],
                    blocked_targets=["thm:conn"],
                    deterministic_worker_rejection_reasons=[
                        "authoritative checker mismatch: worker-side acceptance reported success, but the supervisor authoritative check rejected the submitted result.",
                        "Tablet/SubcriticalExpectation.lean failed to synthesize an instance for HPow R R _",
                    ],
                ),
                "prompt_fragments": [
                    "common/00_trellis_scheme_brief.md",
                    "worker/theorem_stating/00_intro.md",
                    "worker/theorem_stating/05_frontier_work.md",
                    "shared/10_repository_root.md",
                    "shared/20_read_files.md",
                    "worker/common/15_loogle.md",
                    "shared/25_filespec.md",
                    "shared/30_project_invariants.md",
                    "worker/common/20_authority.md",
                    "worker/common/30_request.md",
                    "worker/common/32_deterministic_worker_rejection.md",
                    "worker/theorem_stating/10_mode_guidance.md",
                    "worker/theorem_stating/15_initial_dag_size.md",
                    "worker/theorem_stating/20_common_failure_modes.md",
                    "worker/common/33_routing_hints.md",
                    "worker/common/35_reviewer_comments.md",
                    "worker/common/37_field_guidance.md",
                    "worker/common/40_contract.md",
                    "worker/common/45_outcomes.md",
                    "worker/common/50_acceptance.md",
                    "shared/90_artifact_delivery.md",
                    "worker/common/95_gate_authority.md",
                ],
            },
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_global",
                "authorized_nodes": ["main_node"],
            },
            "worker_acceptance": _worker_acceptance("theorem_global", ["main_node"]),
        },
        worker_gate={
            "worker_acceptance": _worker_acceptance("theorem_global", ["main_node"]),
        },
        repo_path=tmpdir,
        runtime_root=runtime_root,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
    )

    assert "Authoritative deterministic rejection from the previous worker attempt" in prompt
    assert "authoritative checker mismatch" in prompt
    assert "Tablet/SubcriticalExpectation.lean failed to synthesize an instance for HPow R R _" in prompt
    assert "inspect the raw checker artifacts" in prompt
    assert str(runtime_root / "bridge" / "latest_worker.json") in prompt
    assert str(last_invalid_metadata) in prompt
    assert str(tmpdir / ".trellis" / "checker" / "worker_request_99.json") in prompt


def test_worker_prompt_empty_deterministic_rejection_fragment_omits_checker_artifacts() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / "runtime"
    contract = _worker_contract(
        authorized_nodes=["main_node"],
        phase="theorem_stating",
        mode="global",
        active_node=None,
        current_present_nodes=["main_node"],
        current_proof_nodes=["main_node"],
        current_deps={"main_node": []},
        current_semantic_deps={"main_node": []},
        configured_targets=["thm:conn"],
    )
    contract["prompt_fragments"] = [
        "common/00_trellis_scheme_brief.md",
        "worker/common/32_deterministic_worker_rejection.md",
        "worker/common/40_contract.md",
        "worker/common/45_outcomes.md",
        "shared/90_artifact_delivery.md",
    ]

    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "active_node": None,
            "held_target": None,
            "configured_targets": ["thm:conn"],
            "worker_contract": contract,
            "worker_context": {},
            "worker_acceptance": _worker_acceptance("theorem_global", ["main_node"]),
        },
        worker_gate={
            "worker_acceptance": _worker_acceptance("theorem_global", ["main_node"]),
        },
        repo_path=tmpdir,
        runtime_root=runtime_root,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
    )

    assert "Authoritative deterministic rejection from the previous worker attempt" in prompt
    assert "inspect the raw checker artifacts" not in prompt
    assert str(runtime_root / "bridge" / "latest_worker.json") not in prompt
    assert ".trellis/checker/worker_request_" not in prompt


def test_review_prompt_ignores_unscoped_latest_bridge_summaries() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    runtime_root = tmpdir / "runtime"
    bridge_dir = runtime_root / "bridge"
    bridge_dir.mkdir(parents=True, exist_ok=True)
    (bridge_dir / "latest_worker.json").write_text(
        json.dumps({"summary": "stale worker narrative"}),
        encoding="utf-8",
    )
    (bridge_dir / "latest_corr.json").write_text(
        json.dumps({"summary": "stale corr narrative"}),
        encoding="utf-8",
    )
    (bridge_dir / "latest_sound.json").write_text(
        json.dumps({"summary": "stale sound narrative"}),
        encoding="utf-8",
    )

    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "blockers": [],
            "review_blocker_choices": [],
            "review_contract": _review_contract(
                blocker_choices=[],
                allowed_next_nodes=["main_node"],
                targeted_allowed_nodes=["main_node"],
            ),
            "invalid_attempt": False,
            "human_input_outstanding": False,
            "blocked_targets": [],
            "protected_nodes": [],
            "allowed_decisions": ["continue", "need_input"],
            "allowed_next_modes": ["global", "targeted"],
            "kernel_hinted_next_active_nodes": ["main_node"],
            "targeted_next_active_nodes": ["main_node"],
            "allowed_reset_blocker_ids": [],
            "allowed_resets": ["none"],
            "allowed_difficulty_update_nodes": ["main_node"],
        },
        repo_path=tmpdir,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )

    assert "stale worker narrative" not in prompt
    assert "stale corr narrative" not in prompt
    assert "stale sound narrative" not in prompt


def test_correspondence_prompt_describes_preamble_as_one_way_support_check() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    tablet_dir = tmpdir / "Tablet"
    tablet_dir.mkdir(parents=True, exist_ok=True)
    (tablet_dir / "Preamble.lean").write_text("import Mathlib.Data.Nat.Basic\n", encoding="utf-8")
    (tablet_dir / "Preamble.tex").write_text(
        "\\begin{definition}[imports]\nNatural-number notation.\n\\end{definition}\n",
        encoding="utf-8",
    )

    prompt = build_correspondence_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "verify_nodes": ["Preamble"],
            "verify_targets": [],
            "blocked_targets": [],
            "corr_contract": _corr_contract(
                verify_nodes=["Preamble"],
                verify_targets=[],
                preamble_item_ids=["Preamble[1]"],
                preamble_items=[
                    {
                        "id": "Preamble[1]",
                        "env": "definition",
                        "title": "imports",
                        "body": "Natural-number notation.",
                    }
                ],
            ),
        },
        repo_path=tmpdir,
        lane_id="v1",
        raw_output_path=tmpdir / "corr.raw.json",
        done_path=tmpdir / "corr.done",
    )

    assert "Kernel-authored correspondence contract" in prompt
    assert "Preamble[1]" in prompt


def test_paper_prompt_renders_covering_nodes_by_target() -> None:
    tmpdir = Path(tempfile.mkdtemp())

    prompt = build_paper_faithfulness_prompt(
        request={
            "project_invariants": _project_invariants(),
            "paper_contract": _paper_contract(
                verify_targets=["target_a"],
                target_covering_nodes={"target_a": ["cover_a", "cover_b"]},
            ),
        },
        repo_path=tmpdir,
        lane_id="v1",
        raw_output_path=tmpdir / "paper.raw.json",
        done_path=tmpdir / "paper.done",
    )

    assert "Kernel-authored covering nodes by target" in prompt
    assert '"target_a"' in prompt
    assert '"cover_a"' in prompt
    assert '"cover_b"' in prompt
    assert "full mathematical content" in prompt


def test_correspondence_prompt_omits_blank_acceptance_checker_command() -> None:
    tmpdir = Path(tempfile.mkdtemp())

    prompt = build_correspondence_prompt(
        request={
            "project_invariants": _project_invariants(),
            "corr_contract": _corr_contract(
                verify_nodes=["main_node"],
                verify_targets=[],
            ),
        },
        repo_path=tmpdir,
        lane_id="v1",
        raw_output_path=tmpdir / "corr.raw.json",
        done_path=tmpdir / "corr.done",
    )

    assert "No additional acceptance checker is provided for this role." in prompt
    assert (
        "You should test ahead of time whether your work will satisfy deterministic validity checks by running the command"
        not in prompt
    )


def test_correspondence_prompt_provides_request_local_tmp_scratchpad() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    scratch = (
        tmpdir
        / ".trellis"
        / "tmp"
        / "correspondence"
        / "cycle-3-request-7-lane-v1"
    )
    scratch.mkdir(parents=True, exist_ok=True)
    stale_file = scratch / "stale.lean"
    stale_file.write_text("example : True := trivial\n", encoding="utf-8")

    prompt = build_correspondence_prompt(
        request={
            "project_invariants": _project_invariants(),
            "id": 7,
            "cycle": 3,
            "corr_contract": _corr_contract(
                verify_nodes=["main_node"],
                verify_targets=[],
            ),
        },
        repo_path=tmpdir,
        lane_id="v1",
        raw_output_path=tmpdir / "corr.raw.json",
        done_path=tmpdir / "corr.done",
    )

    assert "Correspondence scratch workspace" in prompt
    assert str(scratch) in prompt
    assert scratch.is_dir()
    assert not stale_file.exists()
    assert ".trellis/stuck-math-audit" not in prompt


def test_correspondence_prompt_includes_only_same_lane_previous_findings() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    contract = _corr_contract(verify_nodes=["main_node"], verify_targets=["thm:conn"])
    contract["previous_own_findings_by_lane"] = {
        "v1": {
            "overall": "REJECT",
            "summary": "lane one summary",
            "comments": "lane one comments",
            "correspondence": {"decision": "FAIL", "verdicts": []},
            "paper_faithfulness": {"decision": "PASS", "issues": []},
        },
        "v2": {
            "overall": "APPROVE",
            "summary": "lane two summary",
            "comments": "lane two comments",
            "correspondence": {"decision": "PASS", "verdicts": []},
            "paper_faithfulness": {"decision": "PASS", "issues": []},
        },
    }

    prompt = build_correspondence_prompt(
        request={
            "project_invariants": _project_invariants(),
            "corr_contract": contract,
        },
        repo_path=tmpdir,
        lane_id="v1",
        raw_output_path=tmpdir / "corr.raw.json",
        done_path=tmpdir / "corr.done",
    )

    assert "Your previous accepted lane finding" in prompt
    assert "lane one summary" in prompt
    assert "lane one comments" in prompt
    assert "lane two summary" not in prompt
    assert "lane two comments" not in prompt


def test_soundness_prompt_includes_only_same_lane_previous_findings() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    contract = _sound_contract(target_nodes=["main_node"], node="main_node")
    # Audit Finding 3 (Python-consumer follow-up): the sound contract
    # field is `previous_own_findings`, keyed by NodeId then LaneId.
    contract["previous_own_findings"] = {
        "main_node": {
            "v1": {
                "node": "main_node",
                "overall": "REJECT",
                "summary": "lane one structural summary",
                "comments": "lane one structural comments",
                "soundness": {
                    "decision": "STRUCTURAL",
                    "explanation": "split the proof",
                },
            },
            "v2": {
                "node": "main_node",
                "overall": "APPROVE",
                "summary": "lane two sound summary",
                "comments": "lane two sound comments",
                "soundness": {
                    "decision": "SOUND",
                    "explanation": "looks good",
                },
            },
        },
    }

    prompt = build_soundness_prompt(
        request={
            "project_invariants": _project_invariants(),
            "sound_contract": contract,
        },
        repo_path=tmpdir,
        lane_id="v1",
        node_name="main_node",
        raw_output_path=tmpdir / "sound.raw.json",
        done_path=tmpdir / "sound.done",
    )

    assert "Your previous accepted lane finding" in prompt
    assert "lane one structural summary" in prompt
    assert "lane one structural comments" in prompt
    assert "lane two sound summary" not in prompt
    assert "lane two sound comments" not in prompt


def test_soundness_prompt_includes_per_node_previous_findings_for_lane() -> None:
    """Regression for audit Finding 3 Python-consumer gap.

    The kernel ships sound previous-findings as
    `Map<NodeId, Map<LaneId, SoundReviewerLaneEvidence>>` (per-node-keyed).
    Pre-fix, `_lane_scoped_contract_json` and the prompt context built the
    `previous_own_findings_json` block from the OUTER key, so a request
    on lane `v1` with evidence for nodes A and B on lane `v1` rendered as
    empty (`null`) — silently dropping prior verifier findings.
    """
    tmpdir = Path(tempfile.mkdtemp())
    contract = _sound_contract(target_nodes=["node_a", "node_b"], node="node_a")
    contract["previous_own_findings"] = {
        "node_a": {
            "v1": {
                "node": "node_a",
                "overall": "REJECT",
                "summary": "node-a lane-1 prior structural finding",
                "comments": "split node_a proof",
                "soundness": {"decision": "STRUCTURAL", "explanation": "node_a"},
            },
        },
        "node_b": {
            "v1": {
                "node": "node_b",
                "overall": "REJECT",
                "summary": "node-b lane-1 prior structural finding",
                "comments": "split node_b proof",
                "soundness": {"decision": "STRUCTURAL", "explanation": "node_b"},
            },
            "v2": {
                "node": "node_b",
                "overall": "APPROVE",
                "summary": "node-b lane-2 sound prior",
                "comments": "n/a",
                "soundness": {"decision": "SOUND", "explanation": "ok"},
            },
        },
    }

    prompt = build_soundness_prompt(
        request={
            "project_invariants": _project_invariants(),
            "sound_contract": contract,
        },
        repo_path=tmpdir,
        lane_id="v1",
        node_name="node_a",
        raw_output_path=tmpdir / "sound.raw.json",
        done_path=tmpdir / "sound.done",
    )

    # Both nodes' v1 evidence must be visible in the rendered prompt.
    assert "node-a lane-1 prior structural finding" in prompt
    assert "node-b lane-1 prior structural finding" in prompt
    # Same-lane scoping still excludes the v2 entry from node_b.
    assert "node-b lane-2 sound prior" not in prompt


def test_worker_prompt_requires_shared_checker_before_done() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    worker_gate = {
        "worker_acceptance": _worker_acceptance(
            "theorem_targeted",
            ["ideal_bound_corollary"],
        ),
    }
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "ideal_bound_corollary",
            "held_target": None,
            "configured_targets": ["c.idealbd"],
            "worker_contract": _worker_contract(
                authorized_nodes=["ideal_bound_corollary"],
                configured_targets=["c.idealbd"],
                blocked_targets=["c.idealbd"],
            ),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["ideal_bound_corollary"],
            },
            "worker_acceptance": _worker_acceptance(
                "theorem_targeted",
                ["ideal_bound_corollary"],
            ),
            "blockers": [],
            "blocked_targets": ["c.idealbd"],
            "protected_nodes": [],
            "current_present_nodes": ["ideal_bound_corollary"],
            "current_proof_nodes": ["ideal_bound_corollary"],
            "current_deps": {"ideal_bound_corollary": []},
            "current_semantic_deps": {"ideal_bound_corollary": []},
            "current_target_claims": {"ideal_bound_corollary": ["c.idealbd"]},
        },
        worker_gate=worker_gate,
        repo_path=tmpdir,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
    )

    assert "trellis-worker-result" in prompt
    assert "Trellis formalization scheme" in prompt
    assert "passes raw artifact validator:" in prompt
    assert "Do not write the done marker until this raw JSON validator passes." in prompt
    assert "trellis-worker-result" in prompt
    assert "--context-json" in prompt
    assert "Backslashes inside JSON strings must be escaped." in prompt


def test_worker_prompt_uses_kernel_request_summary_not_top_level_fields() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    worker_gate = {
        "worker_acceptance": _worker_acceptance("theorem_targeted", ["n1"]),
    }
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "cleanup",
            "mode": "cleanup",
            "active_node": "wrong",
            "held_target": None,
            "worker_contract": _worker_contract(
                authorized_nodes=["n1"],
                phase="theorem_stating",
                mode="targeted",
                active_node="n1",
                worker_context={
                    "active_difficulty": "Hard",
                    "worker_profile": "Theorem",
                    "validation_kind": "theorem_targeted",
                    "authorized_nodes": ["n1"],
                },
                blockers=[],
                current_present_nodes=["n1"],
                current_proof_nodes=["n1"],
                current_deps={"n1": []},
                current_semantic_deps={"n1": []},
                current_target_claims={},
            ),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["n1"],
            },
            "worker_acceptance": _worker_acceptance("theorem_targeted", ["n1"]),
        },
        worker_gate=worker_gate,
        repo_path=tmpdir,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
    )

    assert '"phase": "theorem_stating"' in prompt
    assert '"mode": "targeted"' in prompt
    assert '"active_node": "n1"' in prompt
    assert '"phase": "cleanup"' not in prompt


def test_worker_prompt_uses_kernel_worker_profile_not_phase() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    worker_gate = {
        "worker_acceptance": _worker_acceptance("theorem_global", []),
    }
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "proof_formalization",
            "mode": "global",
            "active_node": "n1",
            "held_target": None,
            "configured_targets": [],
            "worker_contract": _worker_contract(authorized_nodes=[]),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_global",
                "authorized_nodes": [],
            },
            "worker_acceptance": _worker_acceptance("theorem_global", []),
            "blockers": [],
            "blocked_targets": [],
            "protected_nodes": [],
            "current_present_nodes": ["n1"],
            "current_proof_nodes": [],
            "current_deps": {},
            "current_semantic_deps": {},
            "current_target_claims": {},
        },
        worker_gate=worker_gate,
        repo_path=tmpdir,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
    )

    assert "trellis theorem-stating worker" in prompt
    assert "trellis proof-formalization worker" not in prompt


def test_worker_prompt_uses_prepared_gate_not_request_contract() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "n1",
            "held_target": None,
            "configured_targets": [],
            "worker_contract": _worker_contract(authorized_nodes=["n1"]),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["n1"],
            },
            "worker_acceptance": {
                "validation_kind": "proof_local",
                "authorized_nodes": ["wrong"],
                "validation_execution_plan": [],
                "observation_plan": {},
            },
            "blockers": [],
            "blocked_targets": [],
            "protected_nodes": [],
            "current_present_nodes": ["n1"],
            "current_proof_nodes": ["n1"],
            "current_deps": {"n1": []},
            "current_semantic_deps": {"n1": []},
            "current_target_claims": {},
        },
        worker_gate={
            "worker_acceptance": _worker_acceptance("theorem_targeted", ["n1"]),
        },
        repo_path=tmpdir,
        raw_output_path=tmpdir / "worker.raw.json",
        done_path=tmpdir / "worker.done",
        acceptance_context_path=tmpdir / "worker.acceptance.json",
    )

    assert '"validation_kind": "theorem_targeted"' in prompt
    assert '"authorized_nodes": [\n    "n1"\n  ]' in prompt


def test_bridge_materializes_project_runtime_support_when_missing() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    config = _fake_config(tmpdir)

    with patch("trellis.runtime.bridge.write_scripts") as mock_install:
        from trellis.runtime import bridge as trellis_bridge

        trellis_bridge._ensure_project_runtime_support(config)

    mock_install.assert_called_once()


def test_bridge_refreshes_project_runtime_support_even_when_snapshot_exists() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    config = _fake_config(tmpdir)
    runtime_src = config.state_dir / "runtime" / "src" / "trellis"
    scripts_dir = config.state_dir / "scripts"
    runtime_src.mkdir(parents=True, exist_ok=True)
    scripts_dir.mkdir(parents=True, exist_ok=True)
    (runtime_src / "check.py").write_text("stale\n", encoding="utf-8")
    (scripts_dir / "check.py").write_text("stale\n", encoding="utf-8")

    with patch("trellis.runtime.bridge.write_scripts") as mock_install:
        from trellis.runtime import bridge as trellis_bridge

        trellis_bridge._ensure_project_runtime_support(config)

    mock_install.assert_called_once()


def test_worker_bridge_normalizes_repo_state_and_semantic_updates() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    tablet_dir = tmpdir / "Tablet"
    tablet_dir.mkdir(parents=True, exist_ok=True)
    (tablet_dir / "n1.lean").write_text("theorem n1 : True := by trivial\n", encoding="utf-8")
    (tablet_dir / "n1.tex").write_text("\\begin{theorem}Test\\end{theorem}\n", encoding="utf-8")
    config = _fake_config(tmpdir)
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=tmpdir / "runtime",
        request=_kernel_bound_request(config, {
            "kind": "worker",
            "id": 11,
            "cycle": 7,
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "n1",
            "held_target": "n1",
            "configured_targets": ["target_a"],
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["n1"],
            },
            "worker_acceptance": _worker_acceptance("theorem_targeted", ["n1"]),
            "blocked_targets": ["target_a"],
            "blockers": [],
            "protected_nodes": [],
            "current_present_nodes": ["n1"],
            "current_proof_nodes": ["n1"],
            "current_deps": {"n1": []},
            "current_semantic_deps": {"n1": []},
            "current_target_claims": {"n1": ["target_a"]},
        }),
    )
    captured = {}

    def _fake_execute(single, *, port_resolver=None, validate_artifact=False):
        captured["single"] = single
        return SingleAgentResponse(
            request_id=single.request_id,
            cycle=single.cycle,
            kind=single.kind,
            burst_role=single.burst_role,
            ok=True,
            payload={
                "outcome": "valid",
                "summary": "updated semantic data",
                "comments": "",
                "semantic_dep_updates": {"n1": ["dep_sem"]},
                "target_claim_updates": {"n1": ["target_a", "target_b"]},
                "difficulty_updates": {"n1": "hard"},
            },
        )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.build_trellis_worker_acceptance_context",
        return_value={
            "ok": True,
            "errors": [],
            "data": {
                "request": dict(request.request),
                "worker_acceptance": dict(request.request["worker_acceptance"]),
                "active_node": "Preamble",
                "held_target": "",
                "authorized_nodes": [],
                "configured_targets": [],
                "current_present_nodes": ["Preamble", "n1"],
                "current_proof_nodes": ["n1"],
                "current_deps": {"n1": []},
                "current_semantic_deps": {"n1": []},
                "current_target_claims": {},
                "repo_path": str(tmpdir),
                "before_snapshot": {},
                "baseline_errors": [],
                "imports_before": [],
                "expected_active_hash": "",
                "baseline_declaration_hashes": {},
                "baseline_correspondence_hashes": {},
            },
        },
    ), patch(
        "trellis.runtime.bridge.build_worker_prompt",
        return_value="worker prompt",
    ), patch(
        "trellis.runtime.bridge.normalize_trellis_worker_result_data",
        return_value={
            "ok": True,
            "errors": [],
            "data": {
                "outcome": "valid",
                "summary": "updated semantic data",
                "comments": "",
                "semantic_dep_updates": {"n1": ["dep_sem"]},
                "target_claim_updates": {"n1": ["target_a", "target_b"]},
                "difficulty_updates": {"n1": "hard"},
            },
            "response": {
                "kind": "worker",
                "request_id": 11,
                "cycle": 7,
                "status": "Ok",
                "outcome": "Valid",
                "snapshot": {
                    "present_nodes": ["n1"],
                    "open_nodes": [],
                    "coverage": {"target_a": ["n1"]},
                    "target_fingerprints": {"n1": "corr-n1"},
                    "corr_current_fingerprints": {"n1": "corr-n1"},
                    "target_corr_current_fingerprints": {"target_a": "n1=corr-n1"},
                    "sound_current_fingerprints": {"n1": "sound-n1"},
                },
                "proof_node_updates": {},
                "dep_updates": {},
                "semantic_dep_updates": {"n1": {"Set": ["dep_sem"]}},
                "target_claim_updates": {"n1": {"Set": ["target_a", "target_b"]}},
                "difficulty_updates": {"n1": {"Set": "Hard"}},
            },
            "validation_errors": [],
            "contract_errors": [],
            "final_outcome": "valid",
        },
    ), patch(
        "trellis.runtime.bridge._hydrate_worker_response_via_kernel",
        side_effect=lambda **kwargs: kwargs["response"],
    ), patch(
        "trellis.runtime.bridge.execute_agent_request",
        side_effect=_fake_execute,
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert captured["single"].burst_role == "worker"
    assert captured["single"].provider.provider == "codex"
    assert response["kind"] == "worker"
    assert response["status"] == "Ok"
    assert response["outcome"] == "Valid"
    assert response["snapshot"]["present_nodes"] == ["n1"]
    assert response["snapshot"]["coverage"] == {"target_a": ["n1"]}
    assert response["snapshot"]["target_fingerprints"] == {"n1": "corr-n1"}
    assert response["snapshot"]["sound_current_fingerprints"] == {"n1": "sound-n1"}
    assert response["proof_node_updates"] == {}
    assert response["dep_updates"] == {}
    assert response["semantic_dep_updates"]["n1"] == {"Set": ["dep_sem"]}
    assert response["target_claim_updates"]["n1"] == {"Set": ["target_a", "target_b"]}
    assert response["difficulty_updates"]["n1"] == {"Set": "Hard"}


def test_worker_bridge_does_not_infer_semantic_updates_for_new_nodes() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    tablet_dir = tmpdir / "Tablet"
    tablet_dir.mkdir(parents=True, exist_ok=True)
    (tablet_dir / "n1.lean").write_text("theorem n1 : True := by trivial\n", encoding="utf-8")
    (tablet_dir / "n1.tex").write_text("\\begin{theorem}Test\\end{theorem}\n", encoding="utf-8")
    config = _fake_config(tmpdir)
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=tmpdir / "runtime",
        request=_kernel_bound_request(config, {
            "kind": "worker",
            "id": 19,
            "cycle": 10,
            "phase": "theorem_stating",
            "mode": "global",
            "active_node": "",
            "held_target": None,
            "configured_targets": [],
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_global",
                "authorized_nodes": [],
            },
            "worker_acceptance": _worker_acceptance("theorem_global", []),
            "blocked_targets": [],
            "blockers": [],
            "protected_nodes": [],
            "current_present_nodes": ["n1"],
            "current_proof_nodes": ["n1"],
            "current_deps": {"n1": []},
            "current_semantic_deps": {"n1": []},
            "current_target_claims": {},
        }),
    )

    def _fake_execute(single, *, port_resolver=None, validate_artifact=False):
        (tablet_dir / "n2.lean").write_text("def n2 : Nat := 0\n", encoding="utf-8")
        (tablet_dir / "n2.tex").write_text("\\begin{definition}A number.\\end{definition}\n", encoding="utf-8")
        return SingleAgentResponse(
            request_id=single.request_id,
            cycle=single.cycle,
            kind=single.kind,
            burst_role=single.burst_role,
            ok=True,
            payload={
                "outcome": "valid",
                "summary": "created n2 without semantic metadata",
                "comments": "",
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            },
        )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.build_trellis_worker_acceptance_context",
        return_value={
            "ok": True,
            "errors": [],
            "data": {
                "request": dict(request.request),
                "worker_acceptance": dict(request.request["worker_acceptance"]),
                "active_node": "Preamble",
                "held_target": "",
                "authorized_nodes": [],
                "configured_targets": [],
                "current_present_nodes": ["Preamble", "n1"],
                "current_proof_nodes": ["n1"],
                "current_deps": {"n1": []},
                "current_semantic_deps": {"n1": []},
                "current_target_claims": {},
                "repo_path": str(tmpdir),
                "before_snapshot": {},
                "baseline_errors": [],
                "imports_before": [],
                "expected_active_hash": "",
                "baseline_declaration_hashes": {},
                "baseline_correspondence_hashes": {},
            },
        },
    ), patch(
        "trellis.runtime.bridge.build_worker_prompt",
        return_value="worker prompt",
    ), patch(
        "trellis.runtime.bridge.normalize_trellis_worker_result_data",
        return_value={
            "ok": True,
            "errors": [],
            "data": {
                "outcome": "valid",
                "summary": "created n2 without semantic metadata",
                "comments": "",
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            },
            "response": {
                "kind": "worker",
                "request_id": 19,
                "cycle": 10,
                "status": "Ok",
                "outcome": "Invalid",
                "snapshot": {
                    "present_nodes": ["n1", "n2"],
                    "open_nodes": [],
                    "coverage": {},
                    "target_fingerprints": {"n1": "corr-n1", "n2": "corr-n2"},
                    "corr_current_fingerprints": {"n1": "corr-n1", "n2": "corr-n2"},
                    "target_corr_current_fingerprints": {},
                    "sound_current_fingerprints": {"n1": "sound-n1", "n2": "sound-n2"},
                },
                "proof_node_updates": {"n1": {"Set": True}},
                "dep_updates": {},
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            },
            "validation_errors": [],
            "contract_errors": [
                "worker must explicitly report semantic_dep_updates for every new node",
                "worker must explicitly report target_claim_updates for every new node",
            ],
            "final_outcome": "invalid",
        },
    ), patch(
        "trellis.runtime.bridge._hydrate_worker_response_via_kernel",
        side_effect=lambda **kwargs: kwargs["response"],
    ), patch(
        "trellis.runtime.bridge.execute_agent_request",
        side_effect=_fake_execute,
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert response["outcome"] == "Invalid"
    assert "n2" not in response["semantic_dep_updates"]
    assert "n2" not in response["target_claim_updates"]


def test_worker_bridge_does_not_hydrate_invalid_response() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    tablet_dir = tmpdir / "Tablet"
    tablet_dir.mkdir(parents=True, exist_ok=True)
    (tablet_dir / "Preamble.lean").write_text("import Mathlib.Data.Nat.Basic\n", encoding="utf-8")
    (tablet_dir / "Preamble.tex").write_text("", encoding="utf-8")
    config = _fake_config(tmpdir)
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=tmpdir / "runtime",
        request=_kernel_bound_request(config, {
            "kind": "worker",
            "id": 21,
            "cycle": 3,
            "phase": "theorem_stating",
            "mode": "global",
            "active_node": "",
            "held_target": None,
            "configured_targets": ["thm:conn"],
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_global",
                "authorized_nodes": ["Preamble"],
            },
            "worker_acceptance": _worker_acceptance("theorem_global", ["Preamble"]),
            "blocked_targets": ["thm:conn"],
            "blockers": [],
            "protected_nodes": [],
            "current_present_nodes": ["Preamble"],
            "current_proof_nodes": [],
            "current_deps": {"Preamble": []},
            "current_semantic_deps": {"Preamble": []},
            "current_target_claims": {"Preamble": []},
        }),
    )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.build_trellis_worker_acceptance_context",
        return_value={
            "ok": True,
            "errors": [],
            "data": {
                "request": dict(request.request),
                "worker_acceptance": dict(request.request["worker_acceptance"]),
                "active_node": "",
                "held_target": "",
                "authorized_nodes": ["Preamble"],
                "configured_targets": ["thm:conn"],
                "current_present_nodes": ["Preamble"],
                "current_proof_nodes": [],
                "current_deps": {"Preamble": []},
                "current_semantic_deps": {"Preamble": []},
                "current_target_claims": {"Preamble": []},
                "repo_path": str(tmpdir),
                "before_snapshot": {},
                "baseline_errors": [],
                "imports_before": [],
                "expected_active_hash": "",
                "baseline_declaration_hashes": {},
                "baseline_correspondence_hashes": {},
            },
        },
    ), patch(
        "trellis.runtime.bridge.build_worker_prompt",
        return_value="worker prompt",
    ), patch(
        "trellis.runtime.bridge.execute_agent_request",
        return_value=SingleAgentResponse(
            request_id="worker-21",
            cycle=3,
            kind="worker",
            burst_role="worker",
            ok=True,
            payload={
                "outcome": "valid",
                "summary": "invalid compile payload",
                "comments": "",
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            },
        ),
    ), patch(
        "trellis.runtime.bridge.normalize_trellis_worker_result_data",
        return_value={
            "ok": False,
            "errors": ["Tablet/Gnp.lean failed to compile"],
            "data": {
                "outcome": "valid",
                "summary": "invalid compile payload",
                "comments": "",
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            },
            "response": {
                "kind": "worker",
                "request_id": 21,
                "cycle": 3,
                "status": "Ok",
                "outcome": "Invalid",
                "snapshot": {
                    "present_nodes": ["Preamble", "Gnp"],
                    "open_nodes": ["Gnp"],
                    "coverage": {},
                    "target_fingerprints": {},
                    "corr_current_fingerprints": {},
                    "paper_current_fingerprints": {},
                    "sound_current_fingerprints": {},
                },
                "proof_node_updates": {},
                "node_kind_updates": {},
                "dep_updates": {},
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            },
            "validation_errors": ["Tablet/Gnp.lean failed to compile"],
            "contract_errors": [],
            "final_outcome": "invalid",
        },
    ), patch(
        "trellis.runtime.bridge._hydrate_worker_response_via_kernel",
        side_effect=AssertionError("invalid worker responses should not be hydrated"),
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert response["outcome"] == "Invalid"
    latest = json.loads((tmpdir / "runtime" / "bridge" / "latest_worker.json").read_text(encoding="utf-8"))
    assert latest["final_outcome"] == "invalid"
    assert latest["validation_errors"] == ["Tablet/Gnp.lean failed to compile"]


def test_worker_bridge_restores_kind_after_hydration() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    tablet_dir = tmpdir / "Tablet"
    tablet_dir.mkdir(parents=True, exist_ok=True)
    (tablet_dir / "Preamble.lean").write_text("import Mathlib.Data.Nat.Basic\n", encoding="utf-8")
    (tablet_dir / "Preamble.tex").write_text("", encoding="utf-8")
    config = _fake_config(tmpdir)
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=tmpdir / "runtime",
        request=_kernel_bound_request(config, {
            "kind": "worker",
            "id": 22,
            "cycle": 4,
            "phase": "theorem_stating",
            "mode": "global",
            "active_node": "",
            "held_target": None,
            "configured_targets": ["thm:conn"],
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_global",
                "authorized_nodes": ["Preamble"],
            },
            "worker_acceptance": _worker_acceptance("theorem_global", ["Preamble"]),
            "blocked_targets": ["thm:conn"],
            "blockers": [],
            "protected_nodes": [],
            "current_present_nodes": ["Preamble"],
            "current_proof_nodes": [],
            "current_deps": {"Preamble": []},
            "current_semantic_deps": {"Preamble": []},
            "current_target_claims": {"Preamble": []},
        }),
    )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.build_trellis_worker_acceptance_context",
        return_value={
            "ok": True,
            "errors": [],
            "data": {
                "request": dict(request.request),
                "worker_acceptance": dict(request.request["worker_acceptance"]),
                "active_node": "",
                "held_target": "",
                "authorized_nodes": ["Preamble"],
                "configured_targets": ["thm:conn"],
                "current_present_nodes": ["Preamble"],
                "current_proof_nodes": [],
                "current_deps": {"Preamble": []},
                "current_semantic_deps": {"Preamble": []},
                "current_target_claims": {"Preamble": []},
                "repo_path": str(tmpdir),
                "before_snapshot": {},
                "baseline_errors": [],
                "imports_before": [],
                "expected_active_hash": "",
                "baseline_declaration_hashes": {},
                "baseline_correspondence_hashes": {},
            },
        },
    ), patch(
        "trellis.runtime.bridge.build_worker_prompt",
        return_value="worker prompt",
    ), patch(
        "trellis.runtime.bridge.execute_agent_request",
        return_value=SingleAgentResponse(
            request_id="worker-22",
            cycle=4,
            kind="worker",
            burst_role="worker",
            ok=True,
            payload={
                "outcome": "valid",
                "summary": "valid worker payload",
                "comments": "",
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            },
        ),
    ), patch(
        "trellis.runtime.bridge.normalize_trellis_worker_result_data",
        return_value={
            "ok": True,
            "errors": [],
            "data": {
                "outcome": "valid",
                "summary": "valid worker payload",
                "comments": "",
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            },
            "response": {
                "kind": "worker",
                "request_id": 22,
                "cycle": 4,
                "status": "Ok",
                "outcome": "Valid",
                "snapshot": {
                    "present_nodes": ["Preamble", "A"],
                    "open_nodes": ["A"],
                    "coverage": {"thm:conn": ["A"]},
                    "target_fingerprints": {"A": "corr-A"},
                    "corr_current_fingerprints": {"A": "corr-A"},
                    "paper_current_fingerprints": {"thm:conn": "paper-A"},
                    "sound_current_fingerprints": {"A": "sound-A"},
                },
                "proof_node_updates": {"A": {"Set": True}},
                "node_kind_updates": {"A": {"Set": "Proof"}},
                "dep_updates": {"A": {"Set": ["Preamble"]}},
                "semantic_dep_updates": {"A": {"Set": ["Preamble"]}},
                "target_claim_updates": {"A": {"Set": ["thm:conn"]}},
                "difficulty_updates": {},
            },
            "validation_errors": [],
            "contract_errors": [],
            "final_outcome": "valid",
        },
    ), patch(
        "trellis.runtime.bridge._hydrate_worker_response_via_kernel",
        return_value={
            "request_id": 22,
            "cycle": 4,
            "status": "Ok",
            "outcome": "Valid",
            "snapshot": {
                "present_nodes": ["Preamble", "A"],
                "open_nodes": ["A"],
                "coverage": {"thm:conn": ["A"]},
                "target_fingerprints": {"A": "corr-A"},
                "corr_current_fingerprints": {"A": "corr-A"},
                "paper_current_fingerprints": {"thm:conn": "paper-A"},
                "sound_current_fingerprints": {"A": "sound-A"},
            },
            "proof_node_updates": {"A": {"Set": True}},
            "node_kind_updates": {"A": {"Set": "Proof"}},
            "dep_updates": {"A": {"Set": ["Preamble"]}},
            "semantic_dep_updates": {"A": {"Set": ["Preamble"]}},
            "target_claim_updates": {"A": {"Set": ["thm:conn"]}},
            "difficulty_updates": {},
        },
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert response["kind"] == "worker"


def test_worker_bridge_trusts_kernel_checked_valid_response() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    tablet_dir = tmpdir / "Tablet"
    tablet_dir.mkdir(parents=True, exist_ok=True)
    (tablet_dir / "Preamble.lean").write_text("import Mathlib.Data.Nat.Basic\n", encoding="utf-8")
    (tablet_dir / "Preamble.tex").write_text("", encoding="utf-8")
    config = _fake_config(tmpdir)
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=tmpdir / "runtime",
        request=_kernel_bound_request(config, {
            "kind": "worker",
            "id": 23,
            "cycle": 4,
            "phase": "theorem_stating",
            "mode": "global",
            "active_node": "",
            "held_target": None,
            "configured_targets": ["thm:conn"],
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_global",
                "authorized_nodes": ["Preamble"],
            },
            "worker_acceptance": _worker_acceptance("theorem_global", ["Preamble"]),
            "blocked_targets": ["thm:conn"],
            "blockers": [],
            "protected_nodes": [],
            "current_present_nodes": ["Preamble"],
            "current_proof_nodes": [],
            "current_deps": {"Preamble": []},
            "current_semantic_deps": {"Preamble": []},
            "current_target_claims": {"Preamble": []},
        }),
    )

    valid_response = {
        "kind": "worker",
        "request_id": 23,
        "cycle": 4,
        "status": "Ok",
        "outcome": "Valid",
        "snapshot": {
            "present_nodes": ["Preamble", "A"],
            "open_nodes": ["A"],
            "coverage": {"thm:conn": ["A"]},
            "target_fingerprints": {},
            "corr_current_fingerprints": {},
            "paper_current_fingerprints": {},
            "sound_current_fingerprints": {},
        },
        "proof_node_updates": {"A": {"Set": True}},
        "node_kind_updates": {"A": {"Set": "Proof"}},
        "dep_updates": {"A": {"Set": ["Preamble"]}},
        "semantic_dep_updates": {"A": {"Set": ["Preamble"]}},
        "target_claim_updates": {"A": {"Set": ["thm:conn"]}},
        "difficulty_updates": {},
    }

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.build_trellis_worker_acceptance_context",
        return_value={
            "ok": True,
            "errors": [],
            "data": {
                "request": dict(request.request),
                "worker_acceptance": dict(request.request["worker_acceptance"]),
                "active_node": "",
                "held_target": "",
                "authorized_nodes": ["Preamble"],
                "configured_targets": ["thm:conn"],
                "current_present_nodes": ["Preamble"],
                "current_proof_nodes": [],
                "current_deps": {"Preamble": []},
                "current_semantic_deps": {"Preamble": []},
                "current_target_claims": {"Preamble": []},
                "repo_path": str(tmpdir),
                "before_snapshot": {},
                "baseline_errors": [],
                "imports_before": [],
                "expected_active_hash": "",
                "baseline_declaration_hashes": {},
                "baseline_correspondence_hashes": {},
            },
        },
    ), patch(
        "trellis.runtime.bridge.build_worker_prompt",
        return_value="worker prompt",
    ), patch(
        "trellis.runtime.bridge.execute_agent_request",
        return_value=SingleAgentResponse(
            request_id="worker-23",
            cycle=4,
            kind="worker",
            burst_role="worker",
            ok=True,
            payload={
                "outcome": "valid",
                "summary": "valid worker payload",
                "comments": "",
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            },
        ),
    ), patch(
        "trellis.runtime.bridge.normalize_trellis_worker_result_data",
        return_value={
            "ok": True,
            "errors": [],
            "data": {
                "outcome": "valid",
                "summary": "valid worker payload",
                "comments": "",
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            },
            "response": valid_response,
            "validation_errors": [],
            "contract_errors": [],
            "final_outcome": "valid",
        },
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert response["kind"] == "worker"
    assert response["outcome"] == "Valid"
    latest = json.loads((tmpdir / "runtime" / "bridge" / "latest_worker.json").read_text(encoding="utf-8"))
    assert latest["final_outcome"] == "valid"
    assert latest["response"]["outcome"] == "Valid"
    assert latest["validation_errors"] == []


def test_worker_bridge_includes_preamble_in_snapshot_and_direct_deps() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    tablet_dir = tmpdir / "Tablet"
    tablet_dir.mkdir(parents=True, exist_ok=True)
    (tablet_dir / "Preamble.lean").write_text("import Mathlib.Data.Nat.Basic\n", encoding="utf-8")
    (tablet_dir / "Preamble.tex").write_text("", encoding="utf-8")
    (tablet_dir / "n1.lean").write_text(
        "import Tablet.Preamble\n\ntheorem n1 : True := by trivial\n",
        encoding="utf-8",
    )
    (tablet_dir / "n1.tex").write_text(
        "\\begin{theorem}Test\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        encoding="utf-8",
    )
    config = _fake_config(tmpdir)
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=tmpdir / "runtime",
        request=_kernel_bound_request(config, {
            "kind": "worker",
            "id": 18,
            "cycle": 9,
            "phase": "theorem_stating",
            "mode": "global",
            "active_node": "Preamble",
            "held_target": None,
            "configured_targets": [],
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_global",
                "authorized_nodes": [],
            },
            "worker_acceptance": _worker_acceptance("theorem_global", []),
            "blocked_targets": [],
            "blockers": [],
            "protected_nodes": [],
            "current_present_nodes": ["Preamble", "n1"],
            "current_proof_nodes": ["n1"],
            "current_deps": {"n1": []},
            "current_semantic_deps": {"n1": []},
            "current_target_claims": {},
        }),
    )

    def _fake_execute(single, *, port_resolver=None, validate_artifact=False):
        return SingleAgentResponse(
            request_id=single.request_id,
            cycle=single.cycle,
            kind=single.kind,
            burst_role=single.burst_role,
            ok=True,
            payload={
                "outcome": "valid",
                "summary": "no-op",
                "comments": "",
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            },
        )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.build_trellis_worker_acceptance_context",
        return_value={
            "ok": True,
            "errors": [],
            "data": {
                "request": dict(request.request),
                "worker_acceptance": dict(request.request["worker_acceptance"]),
                "active_node": "Preamble",
                "held_target": "",
                "authorized_nodes": [],
                "configured_targets": [],
                "current_present_nodes": ["Preamble", "n1"],
                "current_proof_nodes": ["n1"],
                "current_deps": {"n1": []},
                "current_semantic_deps": {"n1": []},
                "current_target_claims": {},
                "repo_path": str(tmpdir),
                "before_snapshot": {},
                "baseline_errors": [],
                "imports_before": [],
                "expected_active_hash": "",
                "baseline_declaration_hashes": {},
                "baseline_correspondence_hashes": {},
            },
        },
    ), patch(
        "trellis.runtime.bridge.build_worker_prompt",
        return_value="worker prompt",
    ), patch(
        "trellis.runtime.bridge.normalize_trellis_worker_result_data",
        return_value={
            "ok": True,
            "errors": [],
            "data": {
                "outcome": "valid",
                "summary": "no-op",
                "comments": "",
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            },
            "response": {
                "kind": "worker",
                "request_id": 18,
                "cycle": 9,
                "status": "Ok",
                "outcome": "Valid",
                "snapshot": {
                    "present_nodes": ["Preamble", "n1"],
                    "open_nodes": [],
                    "coverage": {},
                    "target_fingerprints": {"Preamble": "corr-Preamble", "n1": "corr-n1"},
                    "corr_current_fingerprints": {"Preamble": "corr-Preamble", "n1": "corr-n1"},
                    "target_corr_current_fingerprints": {},
                    "sound_current_fingerprints": {"Preamble": "sound-Preamble", "n1": "sound-n1"},
                },
                "proof_node_updates": {},
                "dep_updates": {"n1": {"Set": ["Preamble"]}},
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            },
            "validation_errors": [],
            "contract_errors": [],
            "final_outcome": "valid",
        },
    ), patch(
        "trellis.runtime.bridge._hydrate_worker_response_via_kernel",
        side_effect=lambda **kwargs: kwargs["response"],
    ), patch(
        "trellis.runtime.bridge.execute_agent_request",
        side_effect=_fake_execute,
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert response["snapshot"]["present_nodes"] == ["Preamble", "n1"]
    assert response["dep_updates"]["n1"] == {"Set": ["Preamble"]}
    assert response["snapshot"]["sound_current_fingerprints"]["Preamble"] == "sound-Preamble"


def test_worker_bridge_returns_malformed_response_on_normalization_failure() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    config = _fake_config(tmpdir)
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=tmpdir / "runtime",
        request=_kernel_bound_request(config, {
            "kind": "worker",
            "id": 31,
            "cycle": 12,
            "phase": "theorem_stating",
            "mode": "global",
            "active_node": "",
            "held_target": None,
            "configured_targets": ["thm:conn"],
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_global",
                "authorized_nodes": ["Preamble"],
            },
            "worker_acceptance": _worker_acceptance("theorem_global", ["Preamble"]),
            "worker_contract": _worker_contract(
                authorized_nodes=["Preamble"],
                configured_targets=["thm:conn"],
                blocked_targets=["thm:conn"],
            ),
            "blocked_targets": ["thm:conn"],
            "blockers": [],
            "protected_nodes": [],
            "current_present_nodes": ["Preamble"],
            "current_proof_nodes": [],
            "current_node_kinds": {"Preamble": "preamble"},
            "current_deps": {"Preamble": []},
            "current_semantic_deps": {"Preamble": []},
            "current_target_claims": {"Preamble": []},
        }),
    )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.build_trellis_worker_acceptance_context",
        return_value={
            "ok": True,
            "errors": [],
            "data": {
                "request": dict(request.request),
                "worker_acceptance": dict(request.request["worker_acceptance"]),
                "active_node": "",
                "held_target": "",
                "authorized_nodes": ["Preamble"],
                "configured_targets": ["thm:conn"],
                "current_present_nodes": ["Preamble"],
                "current_proof_nodes": [],
                "current_deps": {"Preamble": []},
                "current_semantic_deps": {"Preamble": []},
                "current_target_claims": {"Preamble": []},
                "repo_path": str(tmpdir),
                "before_snapshot": {},
                "baseline_errors": [],
                "imports_before": [],
                "expected_active_hash": "",
                "baseline_declaration_hashes": {},
                "baseline_correspondence_hashes": {},
            },
        },
    ), patch(
        "trellis.runtime.bridge.build_worker_prompt",
        return_value="worker prompt",
    ), patch(
        "trellis.runtime.bridge.execute_agent_request",
        return_value=SimpleNamespace(ok=True, payload={"outcome": "valid"}),
    ), patch(
        "trellis.runtime.bridge.normalize_trellis_worker_result_data",
        return_value={"ok": False, "errors": ["bad worker artifact"], "data": None},
    ), patch(
        "trellis.runtime.bridge.run_kernel_cli",
        return_value={
            "status": "build_malformed_response_ok",
            "output": {
                "kind": "worker",
                "request_id": 31,
                "cycle": 12,
                "status": "Malformed",
                "outcome": "Invalid",
                "snapshot": {},
                "proof_node_updates": {},
                "node_kind_updates": {},
                "dep_updates": {},
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            },
        },
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert response["kind"] == "worker"
    assert response["status"] == "Malformed"
    latest = json.loads((tmpdir / "runtime" / "bridge" / "latest_worker.json").read_text(encoding="utf-8"))
    assert latest["errors"] == ["bad worker artifact"]


def test_review_bridge_returns_malformed_response_on_normalization_failure() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    config = _fake_config(tmpdir)
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=tmpdir / "runtime",
        request=_kernel_bound_request(config, {
            "kind": "review",
            "id": 41,
            "cycle": 15,
            "phase": "theorem_stating",
            "mode": "global",
            "active_node": "",
            "held_target": None,
            "configured_targets": ["thm:conn"],
            "blockers": [],
            "blocked_targets": ["thm:conn"],
            "allowed_decisions": ["continue", "need_input"],
            "allowed_next_modes": ["global"],
            "kernel_hinted_next_active_nodes": [],
            "targeted_next_active_nodes": [],
            "allow_targeted_without_next_active": False,
            "allowed_resets": ["none"],
            "allowed_reset_blockers": [],
            "allowed_difficulty_update_nodes": [],
            "review_contract": _review_contract(blocker_choices=[]),
        }),
    )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.build_review_prompt",
        return_value="review prompt",
    ), patch(
        "trellis.runtime.bridge.execute_agent_request",
        return_value=SimpleNamespace(ok=True, raw_path=tmpdir / "review.raw.json"),
    ), patch(
        "trellis.runtime.bridge._load_raw_response_json",
        return_value={"decision": "continue"},
    ), patch(
        "trellis.runtime.bridge.normalize_trellis_reviewer_result_data",
        return_value={"ok": False, "errors": ["bad review artifact"], "data": None},
    ), patch(
        "trellis.runtime.bridge.run_kernel_cli",
        return_value={
            "status": "build_malformed_response_ok",
            "output": {
                "kind": "review",
                "request_id": 41,
                "cycle": 15,
                "status": "Malformed",
                "decision": "Continue",
                "task_blockers": [],
                "override_blockers": [],
                "reset_blockers": [],
                "next_active": None,
                "reset": "None",
                "next_mode": "Global",
                "difficulty_updates": {},
                "clear_human_input": False,
            },
        },
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert response["kind"] == "review"
    assert response["status"] == "Malformed"
    latest = json.loads((tmpdir / "runtime" / "bridge" / "latest_review.json").read_text(encoding="utf-8"))
    assert latest["errors"] == ["bad review artifact"]


def test_paper_bridge_allows_empty_verify_targets() -> None:
    tmpdir = Path(tempfile.mkdtemp())
    config = _fake_config(tmpdir)
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=tmpdir / "runtime",
        request={
            "project_invariants": _project_invariants(),
            "kind": "paper",
            "runtime_support_required": True,
            "id": 14,
            "cycle": 10,
            "phase": "theorem_stating",
            "paper_contract": _paper_contract(verify_targets=[]),
            "verify_lanes": ["v2", "v1"],
            "paper_verify_lane_bindings": _lane_bindings(
                ["v2", "v1"], config.verification.soundness_agents
            ),
            "paper_verify_targets": [],
        },
    )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.run_kernel_cli",
        side_effect=_fake_kernel_cli_for_verifier_normalization,
    ), patch(
        "trellis.runtime.bridge._run_paper_panel",
        side_effect=AssertionError("paper panel should not run for an empty frontier"),
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert response["kind"] == "paper"
    assert response["status"] == "Ok"
    assert response["target_lane_updates"] == {}

    latest = json.loads((tmpdir / "runtime" / "bridge" / "latest_paper.json").read_text(encoding="utf-8"))
    assert latest["normalized"]["target_lane_updates"] == {}
    assert latest["member_responses"] == []


# -----------------------------------------------------------------------------
# Authoritative-checker-only mismatch policy
# -----------------------------------------------------------------------------
# `_checker_mismatch_detail` used to reject whenever the worker-side in-burst
# `check.py` trace differed from the supervisor-side authoritative re-run
# trace (byte-for-byte on the comparable view). That fired on benign
# iterative-agent patterns — e.g. Gemini writes a placeholder `raw.json`,
# runs the check, then rewrites `raw.json` with the final content. Both
# sides pass, traces differ, response was wrongly rejected as Invalid.
#
# New policy: only flag when the supervisor rejects AND the worker's
# in-burst check claimed OK — the cheat-signal case.


def _trace(ok: bool, summary: str) -> dict:
    return {
        "result": {
            "ok": ok,
            "final_outcome": "valid" if ok else "invalid",
            "errors": [] if ok else ["bad thing"],
            "validation_errors": [],
            "contract_errors": [],
            "response": {
                "summary": summary,
                "outcome": "Valid" if ok else "Invalid",
            },
        }
    }


def _supervisor(ok: bool, summary: str) -> dict:
    # Supervisor result is the inner `result` shape; the mismatch detail
    # helper receives it directly, not wrapped in `{"result": ...}`.
    return {
        "ok": ok,
        "final_outcome": "valid" if ok else "invalid",
        "errors": [] if ok else ["supervisor says no"],
        "validation_errors": [],
        "contract_errors": [],
        "response": {"summary": summary, "outcome": "Valid" if ok else "Invalid"},
    }


def test_checker_mismatch_silent_when_supervisor_passes_and_traces_diverge():
    # Supervisor pass + worker pass with diverged response content
    # (placeholder-then-rewrite). Previous policy flagged; new policy
    # accepts.
    detail = bridge_module._checker_mismatch_detail(
        worker_trace=_trace(True, "Testing current status"),
        supervisor_result=_supervisor(True, "proof is closed"),
    )
    assert detail == ""


def test_checker_mismatch_silent_when_supervisor_passes_without_worker_trace():
    detail = bridge_module._checker_mismatch_detail(
        worker_trace=None,
        supervisor_result=_supervisor(True, "proof is closed"),
    )
    assert detail == ""


def test_checker_mismatch_silent_when_both_sides_fail():
    # Ordinary Invalid response — both sides agree the proposal fails.
    # No cheat suspicion; downstream handles the Invalid normally.
    detail = bridge_module._checker_mismatch_detail(
        worker_trace=_trace(False, "tried to fix a thing"),
        supervisor_result=_supervisor(False, "tried to fix a thing"),
    )
    assert detail == ""


def test_checker_mismatch_fires_only_on_cheat_signal():
    # Worker in-burst check claimed OK; supervisor re-run says not OK.
    # That's the only case worth flagging — the agent's OK claim cannot
    # be trusted.
    detail = bridge_module._checker_mismatch_detail(
        worker_trace=_trace(True, "all good"),
        supervisor_result=_supervisor(False, "actually no"),
    )
    assert detail.startswith(bridge_module._CHECKER_MISMATCH_PREFIX)
    assert "summary=" in detail
    assert "supervisor says no" in detail
    assert "worker=" not in detail
    assert "supervisor=" not in detail


def test_checker_mismatch_detail_is_prompt_sized():
    detail = bridge_module._checker_mismatch_detail(
        worker_trace=_trace(True, "x" * 10_000),
        supervisor_result={
            **_supervisor(False, "y" * 10_000),
            "errors": ["z" * 10_000],
        },
    )

    assert detail.startswith(bridge_module._CHECKER_MISMATCH_PREFIX)
    assert len(detail) < 2500
    assert "[truncated" in detail


def test_checker_mismatch_silent_when_supervisor_fails_and_worker_also_failed():
    # Worker claimed not-OK; supervisor agrees. No cheat claim to worry
    # about; this is just a normal rejection.
    detail = bridge_module._checker_mismatch_detail(
        worker_trace=_trace(False, "worker saw the problem"),
        supervisor_result=_supervisor(False, "supervisor saw the problem"),
    )
    assert detail == ""


# ---- Bug X principled fix Phase 4: SIGHUP recovery helper -------------------


def test_recover_done_artifact_no_done_returns_empty(tmp_path: Path) -> None:
    done = tmp_path / "x.done"
    raw = tmp_path / "x.raw.json"
    result = bridge_module._recover_done_artifact(done_path=done, raw_path=raw)
    assert result.payload is None
    assert result.parse_error is None


def test_recover_done_artifact_done_without_raw_returns_parse_error(
    tmp_path: Path,
) -> None:
    done = tmp_path / "x.done"
    raw = tmp_path / "x.raw.json"
    done.write_text("done\n", encoding="utf-8")
    result = bridge_module._recover_done_artifact(done_path=done, raw_path=raw)
    assert result.payload is None
    assert result.parse_error is not None
    assert "missing" in result.parse_error


def test_recover_done_artifact_done_with_unparseable_raw_returns_parse_error(
    tmp_path: Path,
) -> None:
    done = tmp_path / "x.done"
    raw = tmp_path / "x.raw.json"
    done.write_text("done\n", encoding="utf-8")
    raw.write_text("not json{{{", encoding="utf-8")
    result = bridge_module._recover_done_artifact(done_path=done, raw_path=raw)
    assert result.payload is None
    assert result.parse_error is not None
    assert "unparseable" in result.parse_error


def test_recover_done_artifact_done_with_non_object_raw_returns_parse_error(
    tmp_path: Path,
) -> None:
    done = tmp_path / "x.done"
    raw = tmp_path / "x.raw.json"
    done.write_text("done\n", encoding="utf-8")
    raw.write_text("[1, 2, 3]", encoding="utf-8")
    result = bridge_module._recover_done_artifact(done_path=done, raw_path=raw)
    assert result.payload is None
    assert result.parse_error is not None
    assert "not a JSON object" in result.parse_error


def test_recover_done_artifact_done_with_valid_raw_returns_payload(
    tmp_path: Path,
) -> None:
    done = tmp_path / "x.done"
    raw = tmp_path / "x.raw.json"
    done.write_text("done\n", encoding="utf-8")
    raw.write_text(json.dumps({"foo": "bar", "n": 1}), encoding="utf-8")
    result = bridge_module._recover_done_artifact(done_path=done, raw_path=raw)
    assert result.parse_error is None
    assert result.payload == {"foo": "bar", "n": 1}


# ---- Audit followup #2: SIGHUP recovery ordering & dirty-disk relaunch -----


def test_worker_bridge_sighup_recovery_uses_saved_acceptance_context() -> None:
    """Audit followup #2 (Problem A): when `.done + .raw.json + .acceptance.json`
    exist (prior pre-restart burst completed), the bridge MUST normalize the
    recovered payload against the originally saved acceptance context, NOT
    rebuild a fresh one from the (now worker-mutated) disk. Rebuilding would
    let unauthorized worker writes become the new `before_snapshot` baseline.
    """
    tmpdir = Path(tempfile.mkdtemp())
    tablet_dir = tmpdir / "Tablet"
    tablet_dir.mkdir(parents=True, exist_ok=True)
    # Disk state simulating post-worker mutation: the worker wrote n1.lean
    # to a different content than the pre-burst baseline. If the bridge
    # rebuilt the acceptance context here it would absorb this mutation
    # into `before_snapshot` and the kernel would never see it as a
    # candidate change.
    (tablet_dir / "n1.lean").write_text(
        "theorem n1 : True := by sorry  -- worker dirty\n", encoding="utf-8"
    )
    (tablet_dir / "n1.tex").write_text("\\begin{theorem}Test\\end{theorem}\n", encoding="utf-8")

    config = _fake_config(tmpdir)
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=tmpdir / "runtime",
        request=_kernel_bound_request(config, {
            "kind": "worker",
            "id": 88,
            "cycle": 4,
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "n1",
            "held_target": "n1",
            "configured_targets": ["target_a"],
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["n1"],
            },
            "worker_acceptance": _worker_acceptance("theorem_targeted", ["n1"]),
            "blocked_targets": ["target_a"],
            "blockers": [],
            "protected_nodes": [],
            "current_present_nodes": ["n1"],
            "current_proof_nodes": ["n1"],
            "current_deps": {"n1": []},
            "current_semantic_deps": {"n1": []},
            "current_target_claims": {"n1": ["target_a"]},
        }),
    )

    # Pre-stage the artifacts the prior pre-restart burst would have
    # written: `.done`, `.raw.json`, `.acceptance.json`. The acceptance
    # context here represents the pre-burst baseline (before the worker
    # mutated n1.lean) — its `before_snapshot` references the ORIGINAL
    # n1.lean content. The bridge MUST consume this saved context.
    #
    # Audit followup: `.acceptance.json` lives in `private/`, NOT in
    # `staging/`. The split exists because `staging/` is in the worker
    # writable allowlist and the recovery path loads `.acceptance.json`
    # back as the trusted normalization baseline; allowing the worker
    # to overwrite it between `.done` and supervisor restart would let
    # unauthorized worker writes get absorbed into the baseline.
    staging = (
        tmpdir / ".trellis" / "runtime" / "runtime" / "staging"
    )
    staging.mkdir(parents=True, exist_ok=True)
    private_dir = (
        tmpdir / ".trellis" / "runtime" / "runtime" / "private"
    )
    private_dir.mkdir(parents=True, exist_ok=True)
    canonical = "trellis_worker_88_result"
    pre_burst_acceptance = {
        "request": dict(request.request),
        "worker_acceptance": dict(request.request["worker_acceptance"]),
        "active_node": "n1",
        "held_target": "n1",
        "authorized_nodes": ["n1"],
        "configured_targets": ["target_a"],
        "current_present_nodes": ["n1"],
        "current_proof_nodes": ["n1"],
        "current_deps": {"n1": []},
        "current_semantic_deps": {"n1": []},
        "current_target_claims": {"n1": ["target_a"]},
        "repo_path": str(tmpdir),
        # Sentinel value the test asserts we see — would be different
        # if the bridge rebuilt against the dirty disk.
        "before_snapshot": {"Tablet/n1.lean": "PRE_BURST_BASELINE_HASH"},
        "baseline_errors": [],
        "imports_before": [],
        "expected_active_hash": "",
        "baseline_declaration_hashes": {},
        "baseline_correspondence_hashes": {},
    }
    (private_dir / f"{canonical}.acceptance.json").write_text(
        json.dumps(pre_burst_acceptance), encoding="utf-8"
    )
    recovered_payload = {
        "outcome": "valid",
        "summary": "pre-restart worker output",
        "comments": "",
    }
    (staging / f"{canonical}.raw.json").write_text(
        json.dumps(recovered_payload), encoding="utf-8"
    )
    (staging / f"{canonical}.done").write_text("done\n", encoding="utf-8")

    captured: dict[str, object] = {}

    def _capture_normalize(raw_payload, *, repo, acceptance_context):
        captured["acceptance_context"] = acceptance_context
        captured["raw_payload"] = raw_payload
        return {
            "ok": True,
            "errors": [],
            "data": {"outcome": "valid"},
            "response": {
                "kind": "worker",
                "request_id": 88,
                "cycle": 4,
                "status": "Ok",
                "outcome": "Valid",
                "snapshot": {
                    "present_nodes": ["n1"],
                    "open_nodes": [],
                    "coverage": {"target_a": ["n1"]},
                    "target_fingerprints": {"n1": "f1"},
                    "corr_current_fingerprints": {"n1": "f1"},
                    "target_corr_current_fingerprints": {"target_a": "n1=f1"},
                    "sound_current_fingerprints": {"n1": "s1"},
                },
                "proof_node_updates": {},
                "dep_updates": {},
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            },
            "validation_errors": [],
            "contract_errors": [],
            "final_outcome": "valid",
        }

    rebuild_calls = {"count": 0}

    def _rebuild_should_not_be_called(*args, **kwargs):
        rebuild_calls["count"] += 1
        raise AssertionError(
            "build_trellis_worker_acceptance_context must NOT be called on "
            "SIGHUP recovery — saved .acceptance.json is the only valid baseline"
        )

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.build_trellis_worker_acceptance_context",
        side_effect=_rebuild_should_not_be_called,
    ), patch(
        "trellis.runtime.bridge.normalize_trellis_worker_result_data",
        side_effect=_capture_normalize,
    ), patch(
        "trellis.runtime.bridge._hydrate_worker_response_via_kernel",
        side_effect=lambda **kwargs: kwargs["response"],
    ), patch(
        "trellis.runtime.bridge.execute_agent_request",
    ) as mock_execute, patch(
        "trellis.runtime.bridge.run_kernel_cli",
        return_value={"status": "restore_active_worker_base_ok", "restored": False},
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert rebuild_calls["count"] == 0, (
        "acceptance context was rebuilt against dirty disk — Bug X regression"
    )
    mock_execute.assert_not_called()  # Recovery path skips the burst.
    assert response["outcome"] == "Valid"
    # Most important assertion: normalization saw the SAVED pre-burst
    # baseline, not a freshly rebuilt one.
    saved_acceptance = captured["acceptance_context"]
    assert isinstance(saved_acceptance, dict)
    assert saved_acceptance["before_snapshot"] == {
        "Tablet/n1.lean": "PRE_BURST_BASELINE_HASH"
    }
    assert captured["raw_payload"] == recovered_payload


def test_worker_bridge_no_done_relaunch_restores_active_worker_base() -> None:
    """Audit followup #2 (Problem B): when no `.done` exists, the bridge
    relaunches the worker. Before rebuilding the acceptance context (which
    captures `before_snapshot`), the bridge MUST ask the kernel to restore
    `Tablet/` from the captured `active_worker_base` snapshot — otherwise a
    prior crashed worker's mid-write Tablet mutations become the new baseline.
    """
    tmpdir = Path(tempfile.mkdtemp())
    tablet_dir = tmpdir / "Tablet"
    tablet_dir.mkdir(parents=True, exist_ok=True)
    (tablet_dir / "n1.lean").write_text("theorem n1 : True := by trivial\n", encoding="utf-8")
    (tablet_dir / "n1.tex").write_text("\\begin{theorem}Test\\end{theorem}\n", encoding="utf-8")

    runtime_root = tmpdir / "runtime"
    runtime_root.mkdir(parents=True, exist_ok=True)
    # Presence of protocol_state.json triggers the kernel-CLI restore call.
    # Fast-path bypasses if absent — we want the call to happen here.
    (runtime_root / "protocol_state.json").write_text("{}", encoding="utf-8")

    config = _fake_config(tmpdir)
    request = BridgeCliRequest(
        config_path=tmpdir / "trellis.config.json",
        runtime_root=runtime_root,
        request=_kernel_bound_request(config, {
            "kind": "worker",
            "id": 99,
            "cycle": 5,
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "n1",
            "held_target": "n1",
            "configured_targets": ["target_a"],
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["n1"],
            },
            "worker_acceptance": _worker_acceptance("theorem_targeted", ["n1"]),
            "blocked_targets": ["target_a"],
            "blockers": [],
            "protected_nodes": [],
            "current_present_nodes": ["n1"],
            "current_proof_nodes": ["n1"],
            "current_deps": {"n1": []},
            "current_semantic_deps": {"n1": []},
            "current_target_claims": {"n1": ["target_a"]},
        }),
    )

    kernel_calls: list[dict[str, object]] = []

    def _fake_kernel_cli(payload):
        kernel_calls.append(dict(payload))
        action = payload.get("action")
        if action == "restore_active_worker_base":
            return {
                "status": "restore_active_worker_base_ok",
                "restored": True,
            }
        raise AssertionError(f"unexpected kernel CLI action: {action!r}")

    rebuild_seen: dict[str, object] = {}

    def _capture_rebuild(repo, request_arg, *, collect_observations, paper_source_path=None):
        rebuild_seen["repo"] = repo
        rebuild_seen["call_index"] = len(kernel_calls)
        rebuild_seen["paper_source_path"] = paper_source_path
        return {
            "ok": True,
            "errors": [],
            "data": {
                "request": dict(request_arg),
                "worker_acceptance": dict(request_arg["worker_acceptance"]),
                "active_node": "n1",
                "held_target": "n1",
                "authorized_nodes": ["n1"],
                "configured_targets": ["target_a"],
                "current_present_nodes": ["n1"],
                "current_proof_nodes": ["n1"],
                "current_deps": {"n1": []},
                "current_semantic_deps": {"n1": []},
                "current_target_claims": {"n1": ["target_a"]},
                "repo_path": str(tmpdir),
                "before_snapshot": {},
                "baseline_errors": [],
                "imports_before": [],
                "expected_active_hash": "",
                "baseline_declaration_hashes": {},
                "baseline_correspondence_hashes": {},
            },
        }

    with patch("trellis.runtime.bridge.load_config", return_value=config), patch(
        "trellis.runtime.bridge.PolicyManager"
    ) as mock_policy_manager, patch(
        "trellis.runtime.bridge.run_kernel_cli",
        side_effect=_fake_kernel_cli,
    ), patch(
        "trellis.runtime.bridge.build_trellis_worker_acceptance_context",
        side_effect=_capture_rebuild,
    ), patch(
        "trellis.runtime.bridge.build_worker_prompt",
        return_value="worker prompt",
    ), patch(
        "trellis.runtime.bridge.normalize_trellis_worker_result_data",
        return_value={
            "ok": True,
            "errors": [],
            "data": {"outcome": "valid"},
            "response": {
                "kind": "worker",
                "request_id": 99,
                "cycle": 5,
                "status": "Ok",
                "outcome": "Valid",
                "snapshot": {
                    "present_nodes": ["n1"],
                    "open_nodes": [],
                    "coverage": {"target_a": ["n1"]},
                    "target_fingerprints": {"n1": "f1"},
                    "corr_current_fingerprints": {"n1": "f1"},
                    "target_corr_current_fingerprints": {"target_a": "n1=f1"},
                    "sound_current_fingerprints": {"n1": "s1"},
                },
                "proof_node_updates": {},
                "dep_updates": {},
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
            },
            "validation_errors": [],
            "contract_errors": [],
            "final_outcome": "valid",
        },
    ), patch(
        "trellis.runtime.bridge._hydrate_worker_response_via_kernel",
        side_effect=lambda **kwargs: kwargs["response"],
    ), patch(
        "trellis.runtime.bridge.execute_agent_request",
        return_value=SingleAgentResponse(
            request_id="99",
            cycle=5,
            kind="worker",
            burst_role="worker",
            ok=True,
            payload={"outcome": "valid"},
        ),
    ):
        mock_policy_manager.return_value.current.return_value = _fake_policy()
        response = handle_bridge_request(request)

    assert response["outcome"] == "Valid"
    # Restore must run BEFORE acceptance-context rebuild — verifies
    # call ordering: kernel restore is the FIRST kernel CLI call, and
    # acceptance-context rebuild only happens AFTER at least one call.
    assert any(
        call.get("action") == "restore_active_worker_base" for call in kernel_calls
    ), "bridge did not call restore_active_worker_base before relaunch"
    assert rebuild_seen.get("call_index", 0) >= 1, (
        "acceptance-context rebuild ran BEFORE the active_worker_base "
        "restore — Problem B regression"
    )


def test_review_prompt_renders_source_recourse_when_env_vars_set(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """When TRELLIS_REVIEWER_SOURCE_SNAPSHOT/SHA are populated, the
    reviewer prompt includes the source-recourse fragment with the path
    and SHA filled in. This is the recourse path the reviewer takes when
    process semantics seem to block forward progress (cycle-26 cause)."""
    snapshot_dir = tmp_path / "trellis-source-snapshot" / "deadbeef"
    snapshot_dir.mkdir(parents=True)
    monkeypatch.setenv("TRELLIS_REVIEWER_SOURCE_SNAPSHOT", str(snapshot_dir))
    monkeypatch.setenv("TRELLIS_REVIEWER_SOURCE_SHA", "deadbeef")

    contract = _review_contract(blocker_choices=[])
    fragments = list(contract["prompt_fragments"])
    insert_at = fragments.index("shared/10_repository_root.md")
    fragments.insert(insert_at, "review/common/05_source_recourse.md")
    contract["prompt_fragments"] = fragments

    runtime_root = tmp_path / "runtime"
    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "review_contract": contract,
        },
        repo_path=tmp_path,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )

    assert "Source recourse" in prompt
    assert str(snapshot_dir) in prompt
    assert "deadbeef" in prompt


def test_review_prompt_omits_source_recourse_when_env_vars_unset(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Silent-degradation path: with the env vars unset the kernel does
    not emit `05_source_recourse.md` and the bridge does not populate
    the matching context keys, so a prompt rendered without the fragment
    must come through cleanly with no broken-template artifact."""
    monkeypatch.delenv("TRELLIS_REVIEWER_SOURCE_SNAPSHOT", raising=False)
    monkeypatch.delenv("TRELLIS_REVIEWER_SOURCE_SHA", raising=False)

    contract = _review_contract(blocker_choices=[])
    runtime_root = tmp_path / "runtime"
    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "review_contract": contract,
        },
        repo_path=tmp_path,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )

    assert "Source recourse" not in prompt
    assert "{{reviewer_source_snapshot_path}}" not in prompt
    assert "{{reviewer_source_sha}}" not in prompt


# ----- Bridge prompt-trim paired tests -------------------------------------


def test_drop_null_keys_strips_top_level_and_nested_nulls() -> None:
    """Helper for Trim 5/11: nulls disappear, intentionally-empty
    mappings/lists pass through unchanged."""
    from trellis.runtime.bridge_prompts import _drop_null_keys

    result = _drop_null_keys(
        {
            "a": 1,
            "b": None,
            "c": {"x": None, "y": "z", "deep": {"nope": None, "yes": True}},
            "d": [],
            "e": {},
            "f": [{"k": None, "k2": "v"}, {"all_null": None}],
        }
    )
    assert result == {
        "a": 1,
        "c": {"y": "z", "deep": {"yes": True}},
        "d": [],
        "e": {},
        "f": [{"k2": "v"}, {}],
    }


def test_drop_null_keys_passes_through_scalar_and_list_inputs() -> None:
    from trellis.runtime.bridge_prompts import _drop_null_keys

    assert _drop_null_keys(42) == 42
    assert _drop_null_keys("hello") == "hello"
    assert _drop_null_keys([1, None, {"a": None, "b": 1}]) == [1, None, {"b": 1}]


def test_prompt_fragment_renderer_drops_html_comments_before_substitution() -> None:
    from trellis.runtime.bridge_prompts import _strip_prompt_fragment_comments

    rendered = _strip_prompt_fragment_comments(
        "keep\n<!-- drop {{missing_context_key}}\nsecond dropped line -->\nkeep too"
    )

    assert "keep" in rendered
    assert "keep too" in rendered
    assert "drop" not in rendered
    assert "missing_context_key" not in rendered


def test_verifier_evidence_is_empty_helper() -> None:
    from trellis.runtime.bridge_prompts import _verifier_evidence_is_empty

    # Empty in all forms
    assert _verifier_evidence_is_empty({"paper": {}, "corr": {}, "sound": {}})
    assert _verifier_evidence_is_empty(
        {"paper": {}, "substantiveness": {}, "corr": {}, "sound": {}}
    )
    assert _verifier_evidence_is_empty({})
    assert _verifier_evidence_is_empty(
        {"paper": {}, "substantiveness": {}, "corr": [], "sound": []}
    )
    # Non-empty in any subkey breaks the predicate
    assert not _verifier_evidence_is_empty(
        {"paper": {}, "corr": {"lane_1": {"correspondence": {"decision": "FAIL"}}}, "sound": {}}
    )
    assert not _verifier_evidence_is_empty(
        {"substantiveness": {"a": "b"}, "corr": {}, "sound": {}}
    )


def test_review_prompt_omits_empty_verifier_evidence_block(tmp_path: Path) -> None:
    """Trim 7 absent-when-inert: post-Worker reviewer state with no
    verifier evidence collapses the {paper:{},corr:{},sound:{}} block to
    a one-line sentinel."""
    contract = _review_contract(
        blocker_choices=[],
        verifier_evidence={
            "paper": {},
            "substantiveness": {},
            "corr": {},
            "sound": {},
        },
    )
    runtime_root = tmp_path / "runtime"
    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "review_contract": contract,
        },
        repo_path=tmp_path,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )
    assert "no verifier evidence yet for this cycle" in prompt
    # The empty-fence form should NOT appear:
    assert '"corr": {}' not in prompt or "no verifier evidence yet" in prompt


def test_review_prompt_renders_verifier_evidence_when_present(tmp_path: Path) -> None:
    """Trim 7 present-when-relevant: when the kernel forwards real
    evidence (e.g. a corr lane FAIL), the JSON block must appear."""
    contract = _review_contract(
        blocker_choices=[],
        verifier_evidence={
            "paper": {},
            "substantiveness": {},
            "corr": {
                "lane_1": {
                    "correspondence": {"decision": "FAIL", "issues": []},
                    "overall": "REJECT",
                    "summary": "mismatch on n1",
                    "comments": "fix it",
                }
            },
            "sound": {},
        },
    )
    runtime_root = tmp_path / "runtime"
    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "review_contract": contract,
        },
        repo_path=tmp_path,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )
    assert "no verifier evidence yet for this cycle" not in prompt
    assert "lane_1" in prompt
    assert "mismatch on n1" in prompt


def test_review_prompt_writes_audit_report_file_and_truncates_inline_report(
    tmp_path: Path,
) -> None:
    contract = _review_contract(blocker_choices=[], phase="proof_formalization")
    insert_at = contract["prompt_fragments"].index("review/common/30_contract.md")
    contract["prompt_fragments"].insert(insert_at, "review/common/29b_audit_plan.md")
    contract["audit_plan"] = {
        "report": "\n".join(f"report line {idx}" for idx in range(1, 7)),
        "tasks": [{"id": "task-1", "title": "Fix statement", "body": "repair n1"}],
        "probe_paths": [".trellis/stuck-math-audit/probe.lean"],
        "written_at_cycle": 12,
        "written_by_request": 34,
        "trigger_at_write": "test trigger",
    }
    contract["request_summary"]["audit_plan"] = dict(contract["audit_plan"])
    runtime_root = tmp_path / "runtime"

    old = os.environ.get("TRELLIS_STUCK_MATH_AUDIT_REPORT_PROMPT_MAX_LINES")
    os.environ["TRELLIS_STUCK_MATH_AUDIT_REPORT_PROMPT_MAX_LINES"] = "3"
    try:
        prompt = build_review_prompt(
            request={
                "project_invariants": _project_invariants(),
                "id": 50,
                "cycle": 51,
                "phase": "proof_formalization",
                "mode": "global",
                "review_contract": contract,
            },
            repo_path=tmp_path,
            runtime_root=runtime_root,
            raw_output_path=runtime_root / "review.raw.json",
            done_path=runtime_root / "review.done",
            context_json_path=runtime_root / "review.context.json",
        )
    finally:
        if old is None:
            os.environ.pop("TRELLIS_STUCK_MATH_AUDIT_REPORT_PROMPT_MAX_LINES", None)
        else:
            os.environ["TRELLIS_STUCK_MATH_AUDIT_REPORT_PROMPT_MAX_LINES"] = old

    report_path = (
        tmp_path
        / ".trellis"
        / "stuck-math-audit"
        / "cycle-12-request-34"
        / "audit_report.md"
    )
    assert str(report_path) in prompt
    assert "{{audit_report_path}}" not in prompt
    assert "{{audit_report_prompt_line_limit}}" not in prompt
    assert "TRUNCATED FOR PROMPT" in prompt
    assert "report line 1" in prompt
    assert "report line 6" not in prompt
    report_text = report_path.read_text(encoding="utf-8")
    assert "report line 6" in report_text
    assert "# Reviewer Notes" in report_text


def test_review_prompt_omits_empty_blocker_choices_table(tmp_path: Path) -> None:
    """Trim 8 absent-when-inert: no blockers means the rendered
    "Available blocker ids" header collapses to a one-line sentinel."""
    contract = _review_contract(blocker_choices=[])
    runtime_root = tmp_path / "runtime"
    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "review_contract": contract,
        },
        repo_path=tmp_path,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )
    assert "no blockers yet" in prompt
    # The original index-table header should be replaced:
    assert "Index | Kind             | Object" not in prompt


def test_review_prompt_renders_blocker_choices_table_when_present(tmp_path: Path) -> None:
    """Trim 8 present-when-relevant: when the kernel emits non-empty
    blocker choices, the index-table header must appear."""
    blocker = {
        "id": "fingerprint:abc",
        "blocker": {
            "kind": "NodeCorr",
            "object": {"otype": "node", "node": "n1"},
        },
    }
    contract = _review_contract(blocker_choices=[blocker])
    runtime_root = tmp_path / "runtime"
    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "review_contract": contract,
        },
        repo_path=tmp_path,
        runtime_root=runtime_root,
        raw_output_path=runtime_root / "review.raw.json",
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )
    assert "no blockers yet" not in prompt
    assert "Index | Kind             | Object" in prompt
    assert "node:n1" in prompt


def test_review_prompt_blocker_summary_small_count_stays_inline(tmp_path: Path) -> None:
    """Process issue 4: with count <= BLOCKER_INLINE_LIMIT, full blocker
    table is inlined and no sidecar file is written."""
    blockers = [
        {
            "id": f"nodecorr:node:node{i}:fp-{i}",
            "blocker": {
                "kind": "NodeCorr",
                "object": {"otype": "node", "node": f"node{i}"},
                "fingerprint": f"fp-{i}",
            },
        }
        for i in range(3)
    ]
    contract = _review_contract(
        blocker_choices=blockers, active_node="node1"
    )
    runtime_root = tmp_path / "runtime"
    raw_path = runtime_root / "review.raw.json"
    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "review_contract": contract,
        },
        repo_path=tmp_path,
        runtime_root=runtime_root,
        raw_output_path=raw_path,
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )
    # All three blockers should be visible inline.
    assert "node:node0" in prompt
    assert "node:node1" in prompt
    assert "node:node2" in prompt
    # Actionable subset should be present and identify node1 (active_node).
    assert "Actionable subset" in prompt
    # No sidecar file should be created when count is under threshold.
    assert not raw_path.with_name("review.raw.blockers.json").exists()
    # The "exceeds inline limit" notice should NOT appear in small-count mode.
    assert "exceeds the inline limit" not in prompt


def test_review_prompt_blocker_summary_large_count_uses_sidecar(tmp_path: Path) -> None:
    """Process issue 4: when count > BLOCKER_INLINE_LIMIT, the bridge
    writes a sidecar and only the actionable subset is inlined."""
    blockers = [
        {
            "id": f"soundness:node:node{i}:fp-{i}",
            "blocker": {
                "kind": "Soundness",
                "object": {"otype": "node", "node": f"node{i}"},
                "fingerprint": f"fp-{i}",
            },
        }
        for i in range(20)  # well over the default limit of 8
    ]
    contract = _review_contract(
        blocker_choices=blockers, active_node="node3"
    )
    runtime_root = tmp_path / "runtime"
    raw_path = runtime_root / "review.raw.json"
    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "review_contract": contract,
        },
        repo_path=tmp_path,
        runtime_root=runtime_root,
        raw_output_path=raw_path,
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )
    # Header indicates total and per-kind counts.
    assert "20 blocker choices total" in prompt
    # The overflow note should appear inline.
    assert "exceeds the inline limit" in prompt
    # The actionable subset block should mention the active node.
    assert "node:node3" in prompt
    # A non-actionable blocker (e.g. node15) should NOT appear in the
    # inline body (only in the sidecar).
    assert "node:node15" not in prompt
    # The sidecar file should exist and contain all 20 entries.
    sidecar = raw_path.with_name("review.raw.blockers.json")
    assert sidecar.exists()
    sidecar_data = json.loads(sidecar.read_text(encoding="utf-8"))
    assert "blocker_choices" in sidecar_data
    assert len(sidecar_data["blocker_choices"]) == 20
    # The sidecar path appears in the prompt body for discoverability.
    assert str(sidecar) in prompt


def test_review_prompt_blocker_summary_actionable_empty_falls_back(tmp_path: Path) -> None:
    """Process issue 4: when no live blocker touches active_node/held_target,
    the actionable subset falls back to first-K alphabetical (with note)."""
    blockers = [
        {
            "id": f"soundness:node:other{i}:fp-{i}",
            "blocker": {
                "kind": "Soundness",
                "object": {"otype": "node", "node": f"other{i}"},
                "fingerprint": f"fp-{i}",
            },
        }
        for i in range(15)
    ]
    contract = _review_contract(
        blocker_choices=blockers, active_node="unrelated_node"
    )
    runtime_root = tmp_path / "runtime"
    raw_path = runtime_root / "review.raw.json"
    prompt = build_review_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "global",
            "review_contract": contract,
        },
        repo_path=tmp_path,
        runtime_root=runtime_root,
        raw_output_path=raw_path,
        done_path=runtime_root / "review.done",
        context_json_path=runtime_root / "review.context.json",
    )
    # Fallback note should appear inline.
    assert "fallback: no blockers touch active_node/held_target" in prompt
    # Sidecar should still exist.
    sidecar = raw_path.with_name("review.raw.blockers.json")
    assert sidecar.exists()


def test_worker_prompt_blocker_summary_small_count_stays_inline(tmp_path: Path) -> None:
    """Process issue 4 (worker side): with count <= threshold, all blockers
    are inlined and no sidecar is written. The blocker list moves out of
    the inline request_summary JSON so reviewer comments come first."""
    blockers = [
        {
            "kind": "NodeCorr",
            "object": {"otype": "node", "node": f"node{i}"},
            "fingerprint": f"fp-{i}",
        }
        for i in range(3)
    ]
    raw_path = tmp_path / "worker.raw.json"
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "proof_formalization",
            "mode": "local",
            "active_node": "node1",
            "held_target": None,
            "configured_targets": [],
            "worker_contract": _worker_contract(
                authorized_nodes=["node1"],
                phase="proof_formalization",
                mode="local",
                active_node="node1",
                worker_context={
                    "active_difficulty": "Hard",
                    "worker_profile": "ProofHard",
                    "validation_kind": "proof_local",
                    "authorized_nodes": ["node1"],
                },
                blockers=blockers,
                current_present_nodes=["node0", "node1", "node2"],
                current_proof_nodes=["node0", "node1", "node2"],
                current_deps={"node1": ["node0"], "node2": ["node1"]},
                current_semantic_deps={},
                current_target_claims={},
                reviewer_comments="Focus on closing node1's NodeCorr blocker.",
            ),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "ProofHard",
                "validation_kind": "proof_local",
                "authorized_nodes": ["node1"],
            },
            "worker_acceptance": _worker_acceptance("proof_local", ["node1"]),
        },
        worker_gate={"worker_acceptance": _worker_acceptance("proof_local", ["node1"])},
        repo_path=tmp_path,
        raw_output_path=raw_path,
        done_path=tmp_path / "worker.done",
        acceptance_context_path=tmp_path / "worker.acceptance.json",
    )
    # The new "Live blocker status" section should appear.
    assert "Live blocker status" in prompt
    # All three blockers visible inline (count <= limit).
    assert "node:node0" in prompt
    assert "node:node1" in prompt
    assert "node:node2" in prompt
    # No sidecar should be written under threshold.
    assert not raw_path.with_name("worker.raw.blockers.json").exists()
    # The reviewer comments must appear BEFORE the blocker status section.
    reviewer_idx = prompt.find("Focus on closing node1's NodeCorr blocker.")
    blocker_idx = prompt.find("Live blocker status")
    assert reviewer_idx >= 0
    assert blocker_idx >= 0
    assert reviewer_idx < blocker_idx, (
        "Reviewer comments must appear before the global blocker status block "
        "(process issue 4)"
    )
    # The inline request_summary JSON should NOT inline the raw blocker
    # list; it should redirect to the new section.
    assert "live blocker(s); see the dedicated" in prompt


def test_worker_prompt_blocker_summary_large_count_uses_sidecar(tmp_path: Path) -> None:
    """Process issue 4 (worker side): when blocker count overflows, the full
    list moves to a sidecar JSON and only the actionable subset is inlined."""
    blockers = [
        {
            "kind": "Soundness",
            "object": {"otype": "node", "node": f"node{i}"},
            "fingerprint": f"fp-{i}",
        }
        for i in range(20)
    ]
    raw_path = tmp_path / "worker.raw.json"
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "proof_formalization",
            "mode": "local",
            "active_node": "node3",
            "held_target": None,
            "configured_targets": [],
            "worker_contract": _worker_contract(
                authorized_nodes=["node3"],
                phase="proof_formalization",
                mode="local",
                active_node="node3",
                worker_context={
                    "active_difficulty": "Hard",
                    "worker_profile": "ProofHard",
                    "validation_kind": "proof_local",
                    "authorized_nodes": ["node3"],
                },
                blockers=blockers,
                current_present_nodes=[f"node{i}" for i in range(20)],
                current_proof_nodes=[f"node{i}" for i in range(20)],
                current_deps={"node3": ["node2"]},
                current_semantic_deps={},
                current_target_claims={},
                reviewer_comments="Repair node3 only this burst.",
            ),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "ProofHard",
                "validation_kind": "proof_local",
                "authorized_nodes": ["node3"],
            },
            "worker_acceptance": _worker_acceptance("proof_local", ["node3"]),
        },
        worker_gate={"worker_acceptance": _worker_acceptance("proof_local", ["node3"])},
        repo_path=tmp_path,
        raw_output_path=raw_path,
        done_path=tmp_path / "worker.done",
        acceptance_context_path=tmp_path / "worker.acceptance.json",
    )
    assert "Live blocker status" in prompt
    assert "20 live blocker" in prompt
    # The overflow note appears inline.
    assert "exceeds the inline limit" in prompt
    # Sidecar exists and contains all 20 blockers.
    sidecar = raw_path.with_name("worker.raw.blockers.json")
    assert sidecar.exists()
    sidecar_data = json.loads(sidecar.read_text(encoding="utf-8"))
    assert len(sidecar_data["blocker_choices"]) == 20
    # node3 (active) and node2 (direct dep) should be in the inline
    # actionable subset.
    assert "node:node3" in prompt
    assert "node:node2" in prompt
    # A non-actionable blocker (e.g. node15) should NOT appear in the
    # inline prompt body.
    assert "node:node15" not in prompt
    # Reviewer comments precede the blocker block.
    reviewer_idx = prompt.find("Repair node3 only this burst.")
    blocker_idx = prompt.find("Live blocker status")
    assert reviewer_idx < blocker_idx


def test_worker_acceptance_omits_authorized_nodes_for_active_node_only_kind(tmp_path: Path) -> None:
    """Trim 9 absent-when-inert: ProofEasy / ProofLocal use
    `active_node_only` scope; their always-empty `authorized_nodes`
    should be dropped from the prompt-rendered worker_acceptance with a
    short note explaining the scope."""
    acceptance = _worker_acceptance("proof_easy", [])
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "proof_formalization",
            "mode": "local",
            "active_node": "n1",
            "held_target": None,
            "configured_targets": [],
            "worker_contract": _worker_contract(
                authorized_nodes=[],
                phase="proof_formalization",
                mode="local",
                active_node="n1",
                worker_context={
                    "active_difficulty": "Easy",
                    "worker_profile": "ProofEasy",
                    "validation_kind": "proof_easy",
                    "authorized_nodes": [],
                },
                blockers=[],
                current_present_nodes=["n1"],
                current_proof_nodes=["n1"],
                current_deps={"n1": []},
                current_semantic_deps={"n1": []},
                current_target_claims={},
            ),
            "worker_context": {
                "active_difficulty": "Easy",
                "worker_profile": "ProofEasy",
                "validation_kind": "proof_easy",
                "authorized_nodes": [],
            },
            "worker_acceptance": acceptance,
        },
        worker_gate={"worker_acceptance": acceptance},
        repo_path=tmp_path,
        raw_output_path=tmp_path / "worker.raw.json",
        done_path=tmp_path / "worker.done",
        acceptance_context_path=tmp_path / "worker.acceptance.json",
    )
    assert "authorized_nodes_note" in prompt
    assert "active_node_only" in prompt


def test_worker_acceptance_keeps_authorized_nodes_for_authorized_existing_kind(
    tmp_path: Path,
) -> None:
    """Trim 9 present-when-relevant: theorem_targeted uses
    authorized_existing_nodes scope; its `authorized_nodes` must
    remain in the prompt JSON."""
    acceptance = _worker_acceptance("theorem_targeted", ["n1", "n2"])
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "n1",
            "held_target": None,
            "configured_targets": [],
            "worker_contract": _worker_contract(
                authorized_nodes=["n1", "n2"],
                phase="theorem_stating",
                mode="targeted",
                active_node="n1",
                worker_context={
                    "active_difficulty": "Hard",
                    "worker_profile": "Theorem",
                    "validation_kind": "theorem_targeted",
                    "authorized_nodes": ["n1", "n2"],
                },
                blockers=[],
                current_present_nodes=["n1", "n2"],
                current_proof_nodes=["n1", "n2"],
                current_deps={"n1": [], "n2": []},
                current_semantic_deps={"n1": [], "n2": []},
                current_target_claims={},
            ),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["n1", "n2"],
            },
            "worker_acceptance": acceptance,
        },
        worker_gate={"worker_acceptance": acceptance},
        repo_path=tmp_path,
        raw_output_path=tmp_path / "worker.raw.json",
        done_path=tmp_path / "worker.done",
        acceptance_context_path=tmp_path / "worker.acceptance.json",
    )
    # The acceptance section keeps the field with its values
    assert "authorized_nodes_note" not in prompt
    # And the actual node names appear (worker_acceptance JSON renders them)
    assert "n1" in prompt
    assert "n2" in prompt


def test_worker_contract_dedups_authorized_existing_nodes_when_match(tmp_path: Path) -> None:
    """Trim 1 absent-when-redundant: when worker_context.authorized_nodes
    and scope_contract.authorized_existing_nodes hold the same list, the
    prompt-facing scope_contract drops authorized_existing_nodes and
    leaves a one-line pointer."""
    acceptance = _worker_acceptance("theorem_targeted", ["n1"])
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "n1",
            "held_target": None,
            "configured_targets": [],
            "worker_contract": _worker_contract(
                authorized_nodes=["n1"],
                phase="theorem_stating",
                mode="targeted",
                active_node="n1",
                worker_context={
                    "active_difficulty": "Hard",
                    "worker_profile": "Theorem",
                    "validation_kind": "theorem_targeted",
                    "authorized_nodes": ["n1"],
                },
                blockers=[],
                current_present_nodes=["n1"],
                current_proof_nodes=["n1"],
                current_deps={"n1": []},
                current_semantic_deps={"n1": []},
                current_target_claims={},
            ),
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["n1"],
            },
            "worker_acceptance": acceptance,
        },
        worker_gate={"worker_acceptance": acceptance},
        repo_path=tmp_path,
        raw_output_path=tmp_path / "worker.raw.json",
        done_path=tmp_path / "worker.done",
        acceptance_context_path=tmp_path / "worker.acceptance.json",
    )
    # The pointer line replaces the duplicate field name in scope_contract
    assert "authorized_existing_nodes_ref" in prompt
    assert "request_summary.worker_context.authorized_nodes" in prompt


def test_worker_contract_keeps_authorized_existing_nodes_when_lists_differ(
    tmp_path: Path,
) -> None:
    """Trim 1 present-when-distinct: when the lists genuinely differ
    (e.g. a wider worker_context whitelist than the acceptance scope),
    both copies must remain so the worker can see the divergence."""
    # Construct a worker_contract whose scope_contract.authorized_existing_nodes
    # is ["n1"] but request_summary.worker_context.authorized_nodes is
    # ["n1", "n2"] (i.e. they DON'T match).
    contract = _worker_contract(
        authorized_nodes=["n1"],  # scope_contract
        phase="theorem_stating",
        mode="targeted",
        active_node="n1",
        worker_context={
            "active_difficulty": "Hard",
            "worker_profile": "Theorem",
            "validation_kind": "theorem_targeted",
            "authorized_nodes": ["n1", "n2"],  # request_summary.worker_context
        },
        blockers=[],
        current_present_nodes=["n1", "n2"],
        current_proof_nodes=["n1", "n2"],
        current_deps={"n1": [], "n2": []},
        current_semantic_deps={"n1": [], "n2": []},
        current_target_claims={},
    )
    acceptance = _worker_acceptance("theorem_targeted", ["n1"])
    prompt = build_worker_prompt(
        request={
            "project_invariants": _project_invariants(),
            "phase": "theorem_stating",
            "mode": "targeted",
            "active_node": "n1",
            "held_target": None,
            "configured_targets": [],
            "worker_contract": contract,
            "worker_context": {
                "active_difficulty": "Hard",
                "worker_profile": "Theorem",
                "validation_kind": "theorem_targeted",
                "authorized_nodes": ["n1", "n2"],
            },
            "worker_acceptance": acceptance,
        },
        worker_gate={"worker_acceptance": acceptance},
        repo_path=tmp_path,
        raw_output_path=tmp_path / "worker.raw.json",
        done_path=tmp_path / "worker.done",
        acceptance_context_path=tmp_path / "worker.acceptance.json",
    )
    # Pointer note should NOT appear (list mismatch keeps both)
    assert "authorized_existing_nodes_ref" not in prompt
    # Both copies of authorized_existing_nodes / authorized_nodes appear
    assert "authorized_existing_nodes" in prompt


# ----- fail-loudly halt on checker disagreement -----------------------
# `feedback_fail_loudly_on_dual_check`: when the kernel's dual-collector
# local-closure probe detects a primary-vs-axcheck disagreement, it
# persists `checker_disagreement_halt.json` at runtime-root. The bridge
# MUST refuse to dispatch any further bursts while the marker exists.


def _stub_bridge_request(tmp_path: Path) -> BridgeCliRequest:
    """A minimal `BridgeCliRequest` whose halt check fires BEFORE any
    config-load / kind-routing logic runs. The request body's contents
    don't matter — the halt check is the only thing exercised."""
    runtime_root = tmp_path / "runtime"
    runtime_root.mkdir(parents=True, exist_ok=True)
    config_path = tmp_path / "trellis.config.json"
    config_path.write_text("{}")  # never loaded when halt fires
    return BridgeCliRequest(
        config_path=config_path,
        runtime_root=runtime_root,
        request={"kind": "worker", "id": 1, "cycle": 1, "runtime_support_required": False},
    )


def test_checker_disagreement_halt_marker_blocks_new_dispatch(tmp_path: Path) -> None:
    bridge_request = _stub_bridge_request(tmp_path)
    marker = bridge_module.checker_disagreement_halt_marker_path(bridge_request.runtime_root)
    marker.write_text(json.dumps({
        "kind": "checker_disagreement",
        "active_node": "Foo",
        "clear_instructions": "delete this file to resume",
    }))

    assert bridge_module.checker_disagreement_halt_marker_present(bridge_request.runtime_root)
    with pytest.raises(BridgeError) as exc_info:
        handle_bridge_request(bridge_request)
    msg = str(exc_info.value)
    assert "checker_disagreement halt marker present" in msg
    assert str(marker) in msg
    assert "delete the file to resume" in msg


def test_bridge_dispatch_proceeds_when_no_halt_marker(tmp_path: Path) -> None:
    # No marker — the halt check is a no-op, so dispatch advances past
    # it and fails downstream on config load / kind routing instead
    # (the test fixture is intentionally minimal). The key assertion is
    # that the failure is NOT the halt error.
    bridge_request = _stub_bridge_request(tmp_path)
    assert not bridge_module.checker_disagreement_halt_marker_present(bridge_request.runtime_root)
    with pytest.raises(Exception) as exc_info:
        handle_bridge_request(bridge_request)
    assert "checker_disagreement halt marker present" not in str(exc_info.value)


def test_halt_marker_clearable_by_deletion_restores_dispatch(tmp_path: Path) -> None:
    bridge_request = _stub_bridge_request(tmp_path)
    marker = bridge_module.checker_disagreement_halt_marker_path(bridge_request.runtime_root)
    marker.write_text("{}")
    with pytest.raises(BridgeError):
        handle_bridge_request(bridge_request)
    # Operator clears the halt by deleting the marker.
    marker.unlink()
    assert not bridge_module.checker_disagreement_halt_marker_present(bridge_request.runtime_root)
    with pytest.raises(Exception) as exc_info:
        handle_bridge_request(bridge_request)
    assert "checker_disagreement halt marker present" not in str(exc_info.value)


# ----- fail-loudly halt on system_feedback emission -------------------
# Per fail-loudly policy: every system_feedback emission pauses the run.
# Distinct marker file (`system_feedback_halt.json`) from the
# checker-disagreement halt, so an operator can tell the two halt
# causes apart at a glance. The bridge MUST refuse to dispatch any
# further bursts while either marker exists.


def test_system_feedback_halt_marker_blocks_new_dispatch(tmp_path: Path) -> None:
    bridge_request = _stub_bridge_request(tmp_path)
    marker = bridge_module.system_feedback_halt_marker_path(bridge_request.runtime_root)
    marker.write_text(json.dumps({
        "kind": "system_feedback",
        "active_node": "Foo",
        "system_feedback": "tool X mis-parsed argument Y",
        "clear_instructions": "delete this file to resume",
    }))

    assert bridge_module.system_feedback_halt_marker_present(bridge_request.runtime_root)
    with pytest.raises(BridgeError) as exc_info:
        handle_bridge_request(bridge_request)
    msg = str(exc_info.value)
    assert "system_feedback halt marker present" in msg
    assert str(marker) in msg
    assert "delete the file to resume" in msg


def test_system_feedback_halt_marker_clearable_by_deletion_restores_dispatch(
    tmp_path: Path,
) -> None:
    bridge_request = _stub_bridge_request(tmp_path)
    marker = bridge_module.system_feedback_halt_marker_path(bridge_request.runtime_root)
    marker.write_text("{}")
    with pytest.raises(BridgeError):
        handle_bridge_request(bridge_request)
    marker.unlink()
    assert not bridge_module.system_feedback_halt_marker_present(bridge_request.runtime_root)
    with pytest.raises(Exception) as exc_info:
        handle_bridge_request(bridge_request)
    assert "system_feedback halt marker present" not in str(exc_info.value)


def test_any_halt_marker_helper_reports_either_marker(tmp_path: Path) -> None:
    bridge_request = _stub_bridge_request(tmp_path)
    root = bridge_request.runtime_root
    assert not bridge_module.any_halt_marker_present(root)

    checker = bridge_module.checker_disagreement_halt_marker_path(root)
    checker.write_text("{}")
    assert bridge_module.any_halt_marker_present(root)
    checker.unlink()
    assert not bridge_module.any_halt_marker_present(root)

    feedback = bridge_module.system_feedback_halt_marker_path(root)
    feedback.write_text("{}")
    assert bridge_module.any_halt_marker_present(root)


def test_executor_record_system_feedback_writes_halt_marker(tmp_path: Path) -> None:
    # Direct unit test on the wrapper's emission path: when an agent
    # burst returns a non-empty `system_feedback` string,
    # `_record_system_feedback` MUST persist the halt marker at
    # `<runtime_root>/system_feedback_halt.json`. Runtime root is
    # resolved from `TRELLIS_KERNEL_CACHE_ROOT` (same env var the
    # Rust kernel uses).
    from trellis.agent_wrapper import executor as executor_module
    from trellis.agent_wrapper.protocol import AgentLane, SingleAgentRequest
    from trellis.adapters import ProviderConfig

    work_dir = tmp_path / "repo"
    work_dir.mkdir()
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    runtime_root = tmp_path / "runtime"
    runtime_root.mkdir()

    request = SingleAgentRequest(
        request_id="req-123",
        cycle=42,
        kind="worker",
        burst_role="worker",
        provider=ProviderConfig(provider="claude"),
        prompt="",
        work_dir=work_dir,
        state_dir=state_dir,
        session_name="s",
        lane=AgentLane(kind="worker", node_name="Foo"),
        timeout_seconds=0.0,
    )

    with patch.dict(os.environ, {"TRELLIS_KERNEL_CACHE_ROOT": str(runtime_root)}):
        executor_module._record_system_feedback(
            request,
            artifact_name="artifact.json",
            system_feedback="tool X mis-parsed argument Y",
        )

    marker = runtime_root / "system_feedback_halt.json"
    assert marker.exists(), "halt marker must be persisted under runtime root"
    data = json.loads(marker.read_text())
    assert data["kind"] == "system_feedback"
    assert data["active_node"] == "Foo"
    assert data["cycle"] == 42
    assert data["request_id"] == "req-123"
    assert data["request_kind"] == "worker"
    assert data["burst_role"] == "worker"
    assert data["artifact"] == "artifact.json"
    assert data["system_feedback"] == "tool X mis-parsed argument Y"
    assert "DELETE this file to resume" in data["clear_instructions"]
    assert "unix_ts" in data and isinstance(data["unix_ts"], int)


def test_executor_record_system_feedback_empty_text_writes_no_marker(tmp_path: Path) -> None:
    # Negative: empty/whitespace feedback → no marker, no halt.
    from trellis.agent_wrapper import executor as executor_module
    from trellis.agent_wrapper.protocol import AgentLane, SingleAgentRequest
    from trellis.adapters import ProviderConfig

    work_dir = tmp_path / "repo"
    work_dir.mkdir()
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    runtime_root = tmp_path / "runtime"
    runtime_root.mkdir()

    request = SingleAgentRequest(
        request_id="req-456",
        cycle=1,
        kind="worker",
        burst_role="worker",
        provider=ProviderConfig(provider="claude"),
        prompt="",
        work_dir=work_dir,
        state_dir=state_dir,
        session_name="s",
        lane=AgentLane(kind="worker", node_name="Foo"),
        timeout_seconds=0.0,
    )

    with patch.dict(os.environ, {"TRELLIS_KERNEL_CACHE_ROOT": str(runtime_root)}):
        executor_module._record_system_feedback(
            request,
            artifact_name="artifact.json",
            system_feedback="   ",
        )

    marker = runtime_root / "system_feedback_halt.json"
    assert not marker.exists(), "empty feedback must NOT create halt marker"


def test_executor_record_system_feedback_preserves_existing_marker(tmp_path: Path) -> None:
    # First-marker-wins: a subsequent burst's system_feedback must not
    # clobber the original diagnostic. Mirrors the Rust
    # `existing_halt_marker_is_preserved_not_overwritten` test.
    from trellis.agent_wrapper import executor as executor_module
    from trellis.agent_wrapper.protocol import AgentLane, SingleAgentRequest
    from trellis.adapters import ProviderConfig

    work_dir = tmp_path / "repo"
    work_dir.mkdir()
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    runtime_root = tmp_path / "runtime"
    runtime_root.mkdir()
    marker = runtime_root / "system_feedback_halt.json"
    marker.write_text(json.dumps({"kind": "system_feedback", "active_node": "FirstNode"}))

    request = SingleAgentRequest(
        request_id="req-789",
        cycle=99,
        kind="worker",
        burst_role="worker",
        provider=ProviderConfig(provider="claude"),
        prompt="",
        work_dir=work_dir,
        state_dir=state_dir,
        session_name="s",
        lane=AgentLane(kind="worker", node_name="SecondNode"),
        timeout_seconds=0.0,
    )
    with patch.dict(os.environ, {"TRELLIS_KERNEL_CACHE_ROOT": str(runtime_root)}):
        executor_module._record_system_feedback(
            request,
            artifact_name="artifact.json",
            system_feedback="second feedback",
        )
    body = marker.read_text()
    assert "FirstNode" in body, "original marker MUST be preserved"
    assert "SecondNode" not in body
    assert "second feedback" not in body


def test_system_feedback_halt_marker_takes_precedence_after_checker_clears(
    tmp_path: Path,
) -> None:
    # Both markers present: bridge still halts; the message is whichever
    # check fires first (checker-disagreement here, matching the source
    # ordering). Once the operator clears checker but leaves the
    # system_feedback marker, dispatch is STILL refused.
    bridge_request = _stub_bridge_request(tmp_path)
    checker = bridge_module.checker_disagreement_halt_marker_path(bridge_request.runtime_root)
    feedback = bridge_module.system_feedback_halt_marker_path(bridge_request.runtime_root)
    checker.write_text("{}")
    feedback.write_text("{}")
    with pytest.raises(BridgeError) as exc_info:
        handle_bridge_request(bridge_request)
    assert "checker_disagreement halt marker present" in str(exc_info.value)

    checker.unlink()
    with pytest.raises(BridgeError) as exc_info:
        handle_bridge_request(bridge_request)
    assert "system_feedback halt marker present" in str(exc_info.value)
