"""Process memory (PROCESS_MEMORY_SPEC.md §8) — bridge-side tests.

Covers the `process_memory_block` builder (absent dir, cone/type
filtering, overflow degradation), the audit `pending_memory_challenges_block`,
render tests for every touched prompt fragment with the new context keys,
and the prompt_browser import.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from trellis.runtime.bridge_prompts import (
    _PROCESS_MEMORY_BLOCK_MAX_CHARS,
    pending_memory_challenges_block,
    process_memory_block,
    render_prompt_sections,
)


def _write_entry(
    repo: Path,
    cone: str,
    entry_id: str,
    *,
    entry_type: str = "refuted-route",
    status: str = "active",
    body: str = "Route X refuted: counterexample n=3.",
) -> None:
    path = repo / "process-memory" / cone / f"{entry_id}.md"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "---\n"
        f"id: {entry_id}\n"
        f"type: {entry_type}\n"
        f"status: {status}\n"
        f"coarse_node: {cone}\n"
        "created: {cycle: 3, request_id: 9}\n"
        "---\n\n"
        f"{body}\n",
        encoding="utf-8",
    )


def test_block_is_empty_when_directory_absent(tmp_path: Path) -> None:
    assert process_memory_block(tmp_path, "ConeA") == ""


def test_block_is_empty_when_no_active_entries(tmp_path: Path) -> None:
    _write_entry(tmp_path, "ConeA", "pm-0001-x", status="retired")
    assert process_memory_block(tmp_path, "ConeA") == ""


def test_block_renders_cone_bodies_and_index_one_liners(tmp_path: Path) -> None:
    _write_entry(tmp_path, "ConeA", "pm-0001-route-x", body="Route X refuted: n=3 fails.")
    _write_entry(
        tmp_path,
        "ConeA",
        "pm-0002-note",
        entry_type="process-note",
        body="Prefer the warm checker for probes.",
    )
    _write_entry(
        tmp_path,
        "global",
        "pm-0003-iface",
        entry_type="constraint",
        body="Interface fact: e_F = 2 b^(c/2) n.",
    )
    block = process_memory_block(tmp_path, "ConeA")
    # Cone refuted-route: full body under a heading.
    assert "### [pm-0001-route-x] refuted-route/ConeA" in block
    assert "Route X refuted: n=3 fails." in block
    # Cone process-note: one-liner only (no heading).
    assert "### [pm-0002-note]" not in block
    assert "- [pm-0002-note] process-note/ConeA — Prefer the warm checker" in block
    # Global constraint: index one-liner (not inlined for another cone's block).
    assert "### [pm-0003-iface]" not in block
    assert "- [pm-0003-iface] constraint/global — Interface fact" in block
    # File paths surfaced so roles can read the full entries.
    assert "process-memory/global/pm-0003-iface.md" in block
    # Positive directive + challenge channel.
    assert "settled process memory" in block
    assert "memory_challenges" in block


def test_block_without_challenge_directive_for_verifier_lanes(tmp_path: Path) -> None:
    _write_entry(tmp_path, "ConeA", "pm-0001-route-x")
    block = process_memory_block(tmp_path, "ConeA", include_challenge_directive=False)
    assert "settled process memory" in block
    assert "memory_challenges" not in block


def test_block_degrades_to_one_liners_on_overflow(tmp_path: Path) -> None:
    _write_entry(
        tmp_path,
        "ConeA",
        "pm-0001-big",
        body="HOOKLINE\n" + ("y" * (_PROCESS_MEMORY_BLOCK_MAX_CHARS + 100)),
    )
    block = process_memory_block(tmp_path, "ConeA")
    assert len(block) <= _PROCESS_MEMORY_BLOCK_MAX_CHARS
    assert "- [pm-0001-big] refuted-route/ConeA — HOOKLINE" in block
    assert "### [pm-0001-big]" not in block


def test_block_with_no_cone_renders_index_only(tmp_path: Path) -> None:
    _write_entry(tmp_path, "ConeA", "pm-0001-route-x")
    block = process_memory_block(tmp_path, None)
    assert "- [pm-0001-route-x] refuted-route/ConeA" in block
    assert "### [pm-0001-route-x]" not in block


def test_pending_memory_challenges_block_prefers_contract_and_is_empty_safe() -> None:
    assert pending_memory_challenges_block({}) == ""
    assert (
        pending_memory_challenges_block({"stuck_math_audit_contract": {}}) == ""
    )
    block = pending_memory_challenges_block(
        {
            "stuck_math_audit_contract": {
                "pending_memory_challenges": [
                    {
                        "origin": "worker",
                        "cycle": 12,
                        "request_id": 88,
                        "entry_id": "pm-0002-y",
                        "reason": "probe contradicts the bound",
                    }
                ]
            },
            "pending_memory_challenges": [],
        }
    )
    assert "## Pending memory challenges" in block
    assert "`pm-0002-y` (challenged by worker, cycle 12): probe contradicts the bound" in block
    assert "memory_operations" in block
    # Fallback: request-level field when the contract lacks the key.
    fallback = pending_memory_challenges_block(
        {
            "pending_memory_challenges": [
                {"origin": "reviewer", "cycle": 3, "entry_id": "pm-0001-z", "reason": "stale"}
            ]
        }
    )
    assert "`pm-0001-z` (challenged by reviewer, cycle 3): stale" in fallback


# ---- fragment render tests ---------------------------------------------

_ROLE_FRAGMENTS = [
    "worker/common/22_process_memory.md",
    "review/common/29d_process_memory.md",
    "verifier/common/16_process_memory.md",
]


@pytest.mark.parametrize("fragment", _ROLE_FRAGMENTS)
def test_role_fragments_render_block_and_drop_out_when_empty(fragment: str) -> None:
    sections = render_prompt_sections(
        [fragment], {"process_memory_block": "## Process memory\n\nBODY"}
    )
    assert len(sections) == 1
    assert "BODY" in sections[0]["text"]
    # Empty block => empty section => dropped (pre-migration prompts
    # render unchanged).
    assert render_prompt_sections([fragment], {"process_memory_block": ""}) == []


def test_audit_fragment_renders_block_and_pending_challenges() -> None:
    fragment = "stuck_math_audit/common/03b_process_memory.md"
    sections = render_prompt_sections(
        [fragment],
        {
            "process_memory_block": "## Process memory\n\nBODY",
            "pending_memory_challenges_block": "## Pending memory challenges\n\n- `pm-1`",
        },
    )
    assert len(sections) == 1
    assert "BODY" in sections[0]["text"]
    assert "Pending memory challenges" in sections[0]["text"]
    assert (
        render_prompt_sections(
            [fragment],
            {"process_memory_block": "", "pending_memory_challenges_block": ""},
        )
        == []
    )


def test_audit_output_contract_fragment_documents_memory_operations() -> None:
    sections = render_prompt_sections(
        ["stuck_math_audit/common/05_output_contract.md"],
        {
            "latest_stuck_math_audit_rejection_block": "(none)",
            "contract_json": "{}",
        },
    )
    text = sections[0]["text"]
    assert "`memory_operations` is optional" in text
    assert '"op": "supersede"' in text
    assert '"op": "retire"' in text
    # The v1 copy-forward instruction is gone.
    assert "Copy this section forward" not in text


def test_worker_audit_plan_fragments_point_at_process_memory() -> None:
    for fragment, context in [
        ("worker/common/34c_audit_plan.md", {"audit_plan_json": "{}"}),
        (
            "worker/common/34d_last_audit_plan.md",
            {"previous_audit_plan_snapshot_json": "{}"},
        ),
    ]:
        sections = render_prompt_sections([fragment], context)
        text = sections[0]["text"]
        assert "Process memory section" in text
        assert "## Established constraints and refuted routes" not in text


def test_prompt_browser_imports() -> None:
    import trellis.prompt_browser  # noqa: F401
