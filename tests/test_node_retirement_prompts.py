"""Audit-ordered node retirement — bridge-side prompt tests.

Covers the `node_retirement_block` builder (empty-safe; nodes + reason
rendering), the review/worker fragments that consume it, and the audit
output-contract bullet documenting `node_retirement_request`.
"""

from __future__ import annotations

from trellis.runtime.bridge_prompts import (
    node_retirement_block,
    render_prompt_sections,
)


def test_node_retirement_block_is_empty_safe() -> None:
    assert node_retirement_block({}) == ""
    assert node_retirement_block({"pending_node_retirement": None}) == ""
    assert node_retirement_block({"pending_node_retirement": {"nodes": [], "reason": "r"}}) == ""


def test_node_retirement_block_renders_nodes_and_reason() -> None:
    block = node_retirement_block(
        {
            "pending_node_retirement": {
                "nodes": ["H1", "H2"],
                "reason": "mis-factored tower",
                "requested_at_cycle": 7,
                "audit_request_id": 3,
            }
        }
    )
    assert "`H1`, `H2`" in block
    assert "Audit reason: mis-factored tower" in block


def test_review_fragment_renders_pending_retirement() -> None:
    sections = render_prompt_sections(
        ["review/common/30e_node_retirement.md"],
        {
            "node_retirement_block": node_retirement_block(
                {
                    "pending_node_retirement": {
                        "nodes": ["H1"],
                        "reason": "mis-factored",
                    }
                }
            )
        },
    )
    text = "\n".join(section["text"] for section in sections)
    assert "Pending node retirement" in text
    assert "`H1`" in text
    assert "Audit reason: mis-factored" in text
    assert "dispatch_node_retirement=true" in text
    assert "node_retirement_decline_reason" in text


def test_worker_fragment_renders_retirement_task() -> None:
    sections = render_prompt_sections(
        ["worker/common/30b_node_retirement_task.md"],
        {
            "node_retirement_block": node_retirement_block(
                {
                    "pending_node_retirement": {
                        "nodes": ["H1", "H2"],
                        "reason": "mis-factored",
                    }
                }
            )
        },
    )
    text = "\n".join(section["text"] for section in sections)
    assert "Node retirement task" in text
    assert "`H1`, `H2`" in text
    assert "deleted_nodes" in text


def test_audit_output_contract_documents_node_retirement_request() -> None:
    sections = render_prompt_sections(
        ["stuck_math_audit/common/05_output_contract.md"],
        {
            "latest_stuck_math_audit_rejection_block": "",
            "contract_json": "{}",
        },
    )
    text = "\n".join(section["text"] for section in sections)
    assert "node_retirement_request" in text
    assert "latest_node_retirement_decline" in text
